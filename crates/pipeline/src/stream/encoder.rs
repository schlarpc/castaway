//! The H.264 encoder behind the output stream.
//!
//! **wgpu cannot encode.** It is a WebGPU implementation — graphics and compute, no video
//! encode surface, and none in the spec — so there is no "encoded readback" to ask it for.
//! An encoder has to be reached directly, and libavcodec is already linked, so this is a
//! thin `ffmpeg-sys-next` wrapper around whichever H.264 encoder the box actually has.
//!
//! ## Which encoder
//!
//! [`CANDIDATES`] is tried in order and the first that *opens* wins. That is a runtime
//! decision on purpose: the `stream` feature says a build can encode, never how. The
//! panel ships to a Windows box with an unknown GPU, development happens on Linux with a
//! different one, and CI has neither — one binary has to cover all three, and "which
//! encoders exist" is not knowable until the process is running against a driver.
//!
//! ## Which of them upload
//!
//! Most of libavcodec's hardware encoders take an NV12 frame in system memory and do the
//! upload themselves. VA-API does not: it wants frames from its own pool, so that entry
//! carries an `AVHWFramesContext` and each frame goes through `av_hwframe_transfer_data`.
//!
//! **That upload is not zero-copy, and neither is the software path.** The composited
//! frame is read back to system memory by [`crate::nv12`] and handed to the encoder from
//! there. The zero-copy version — exporting the wgpu texture's native handle and importing
//! it into the encoder's device, which is `crate::hwaccel`'s import path run backwards —
//! is a third interop path per vendor and is deliberately not attempted here; #101 says
//! what it would take. What this costs is the readback, which is 3 MB a frame at 1080p and
//! is the same copy the screenshot endpoint has always done.
//!
//! Nothing in this module is on the panel's render thread: an encoder lives on the thread
//! [`super::tap`] spawns for it, which is why it is `Send` and not `Sync`.
#![allow(
    unsafe_code,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use std::ffi::{c_int, CString};

use ffmpeg_sys_next as sys;

use super::cadence::FrameRate;
use super::fmp4::{self, AvcConfig, Sample};
use crate::av::{av_error, try_set_opt};
use crate::error::PipelineError;
use crate::nv12::Nv12Planes;

/// The hardware families whose encoders will not take a frame from system memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwFamily {
    /// VA-API, i.e. Mesa on Intel and AMD.
    Vaapi,
}

impl HwFamily {
    const fn device_type(self) -> sys::AVHWDeviceType {
        match self {
            Self::Vaapi => sys::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
        }
    }

    const fn pixel_format(self) -> sys::AVPixelFormat {
        match self {
            Self::Vaapi => sys::AVPixelFormat::AV_PIX_FMT_VAAPI,
        }
    }
}

/// How an encoder wants its pixels handed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Straight from system memory. The frame's data pointers reference the readback
    /// buffer directly and libavcodec copies only if it needs to keep the frame — so the
    /// common case is one copy off the GPU and no more.
    Software,
    /// Into a pool on the encoder's own device first.
    Upload(HwFamily),
}

/// One encoder worth trying, and what it needs.
#[derive(Debug, Clone, Copy)]
pub struct EncoderChoice {
    /// The libavcodec encoder name.
    pub name: &'static str,
    /// How it takes frames.
    pub delivery: Delivery,
    /// Private options to ask for, all of them optional. An encoder that has never heard
    /// of one is how we find out which encoder we got — see [`try_set_opt`].
    pub tuning: &'static [(&'static str, &'static str)],
}

/// Encoders to try, best first.
///
/// "Best" is: on the panel's own GPU before on its CPU, and among the hardware ones,
/// whichever is native to the platform. Every entry is optional — a box that has none of
/// them cannot stream, and says so rather than pretending.
pub const CANDIDATES: &[EncoderChoice] = &[
    // Linux hardware: Mesa's encoder on both Intel and AMD, which is what the development
    // box has and the one entry here that needs a frames pool.
    #[cfg(unix)]
    EncoderChoice {
        name: "h264_vaapi",
        delivery: Delivery::Upload(HwFamily::Vaapi),
        // Constant bitrate: a screen-content stream is mostly still, and VBR spends its
        // whole budget on the one frame somebody moves a window.
        tuning: &[("rc_mode", "CBR")],
    },
    EncoderChoice {
        name: "h264_nvenc",
        delivery: Delivery::Software,
        // `p4` is the middle of NVENC's quality/speed ladder; `ull` is its
        // ultra-low-latency tuning, which is what turns off the lookahead that would
        // otherwise hold frames back behind the segment they belong in.
        tuning: &[
            ("preset", "p4"),
            ("tune", "ull"),
            ("rc", "cbr"),
            ("delay", "0"),
        ],
    },
    EncoderChoice {
        name: "h264_amf",
        delivery: Delivery::Software,
        tuning: &[("usage", "ultralowlatency"), ("quality", "speed")],
    },
    // Windows/Intel. Takes system-memory frames and allocates its own surfaces.
    #[cfg(windows)]
    EncoderChoice {
        name: "h264_qsv",
        delivery: Delivery::Software,
        tuning: &[("preset", "veryfast"), ("look_ahead", "0")],
    },
    // Software, and the reason the list has a floor at all: it is what CI and a box with
    // no usable GPU encoder land on. Absent from the LGPL ffmpeg the Windows build ships,
    // which is exactly why this is a runtime probe and not a compile-time choice.
    EncoderChoice {
        name: "libx264",
        delivery: Delivery::Software,
        tuning: &[("preset", "veryfast"), ("tune", "zerolatency")],
    },
    EncoderChoice {
        name: "libopenh264",
        delivery: Delivery::Software,
        tuning: &[],
    },
];

/// An open H.264 encoder.
pub struct H264Encoder {
    ctx: *mut sys::AVCodecContext,
    /// The hardware device and its frame pool. Null on the software path.
    device: *mut sys::AVBufferRef,
    frames: *mut sys::AVBufferRef,
    /// An NV12 frame whose data pointers are aimed at the caller's readback buffer. It
    /// owns no pixels: libavcodec copies out of it if it needs to keep one.
    software: *mut sys::AVFrame,
    /// The upload target. Null on the software path.
    hardware: *mut sys::AVFrame,
    packet: *mut sys::AVPacket,
    choice: EncoderChoice,
    config: AvcConfig,
    width: u32,
    height: u32,
    /// One frame's presentation length, in [`fmp4::TIMESCALE`] ticks.
    duration: u32,
    /// The next frame's timestamp, in the encoder's own `1/fps` time base.
    pts: i64,
    /// Set once an init segment has been written from [`Self::config`]. Before that, a
    /// change in the parameter sets is a *correction*; after it, there is nowhere left to
    /// put one.
    described: bool,
    /// Whether the next frame must be coded as an IDR, whatever the GOP says.
    ///
    /// Set from another thread's request (a WebRTC peer that just joined, or one whose
    /// decoder lost sync) and consumed by the next [`Self::encode`]. See
    /// [`super::feed::LiveFeed`].
    force_keyframe: bool,
}

// SAFETY: every pointer here is owned solely by this struct, is created and destroyed by
// it, and is only ever dereferenced through `&mut self`. libavcodec's encode API is
// single-threaded per context, and moving the sole owner to the encode thread introduces
// no sharing. Not `Sync`, deliberately: two threads calling `encode` would be exactly the
// sharing this reasoning excludes.
unsafe impl Send for H264Encoder {}

impl std::fmt::Debug for H264Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("H264Encoder")
            .field("encoder", &self.choice.name)
            .field("size", &(self.width, self.height))
            .finish_non_exhaustive()
    }
}

impl H264Encoder {
    /// Open the best encoder this box has for a `width`×`height` stream.
    ///
    /// # Errors
    /// [`PipelineError::Encode`] listing what each candidate said, because "the stream
    /// does not work" is not actionable and "libx264 is not in this ffmpeg and the render
    /// node refused VA-API" is.
    pub fn open(
        width: u32,
        height: u32,
        rate: FrameRate,
        bitrate: u32,
        gop: u32,
    ) -> Result<Self, PipelineError> {
        let mut refused = Vec::new();
        for choice in CANDIDATES {
            match Self::open_one(*choice, width, height, rate, bitrate, gop) {
                Ok(encoder) => {
                    tracing::info!(
                        encoder = choice.name,
                        width,
                        height,
                        fps = rate.get(),
                        "output stream encoder opened"
                    );
                    return Ok(encoder);
                }
                Err(e) => {
                    tracing::debug!(encoder = choice.name, error = %e, "encoder declined");
                    refused.push(format!("{}: {e}", choice.name));
                }
            }
        }
        Err(PipelineError::Encode(format!(
            "no H.264 encoder would open ({})",
            refused.join("; ")
        )))
    }

    /// Try exactly one candidate.
    ///
    /// The half-built encoder is a real `Self` from the first line, so every early return
    /// frees whatever had been allocated by then through [`Drop`] rather than through a
    /// cleanup path per failure — which is the shape this kind of code leaks in.
    fn open_one(
        choice: EncoderChoice,
        width: u32,
        height: u32,
        rate: FrameRate,
        bitrate: u32,
        gop: u32,
    ) -> Result<Self, PipelineError> {
        let mut encoder = Self {
            ctx: std::ptr::null_mut(),
            device: std::ptr::null_mut(),
            frames: std::ptr::null_mut(),
            software: std::ptr::null_mut(),
            hardware: std::ptr::null_mut(),
            packet: std::ptr::null_mut(),
            choice,
            config: AvcConfig::default(),
            width,
            height,
            duration: rate.sample_duration_ticks(),
            pts: 0,
            described: false,
            force_keyframe: false,
        };
        // SAFETY: `encoder` is freshly constructed with every pointer null, which is what
        // `build` requires and what `Drop` tolerates if this returns early.
        unsafe { encoder.build(rate, bitrate, gop) }?;
        Ok(encoder)
    }

    /// # Safety
    /// Must be called exactly once, on a `Self` whose pointers are all null.
    unsafe fn build(
        &mut self,
        rate: FrameRate,
        bitrate: u32,
        gop: u32,
    ) -> Result<(), PipelineError> {
        let name = CString::new(self.choice.name)
            .map_err(|_| PipelineError::Encode("encoder name is not a C string".into()))?;
        // SAFETY: `name` is NUL-terminated and outlives the call, which does not retain it.
        let codec = unsafe { sys::avcodec_find_encoder_by_name(name.as_ptr()) };
        if codec.is_null() {
            return Err(PipelineError::Encode("not built into this ffmpeg".into()));
        }
        // SAFETY: `codec` is a static descriptor libavcodec just handed back.
        self.ctx = unsafe { sys::avcodec_alloc_context3(codec) };
        if self.ctx.is_null() {
            return Err(PipelineError::Encode("could not allocate a context".into()));
        }

        // SAFETY: `self.ctx` is a live, unopened context; every field written below is a
        // plain scalar that `avcodec_open2` reads.
        unsafe {
            let ctx = &mut *self.ctx;
            ctx.width = self.width as c_int;
            ctx.height = self.height as c_int;
            // The encoder's own clock is one tick per frame; the 90 kHz timeline the
            // segments are written on is `fmp4`'s, and the two are joined by
            // `FrameRate::sample_duration_ticks`. Keeping them apart means a frame rate
            // change never has to be expressed as a time base change mid-stream.
            ctx.time_base = sys::AVRational {
                num: 1,
                den: rate.get() as c_int,
            };
            ctx.framerate = sys::AVRational {
                num: rate.get() as c_int,
                den: 1,
            };
            ctx.bit_rate = i64::from(bitrate);
            ctx.rc_max_rate = i64::from(bitrate);
            // Half a second of buffer. Enough for rate control to absorb a keyframe,
            // short enough that it does not become latency of its own.
            ctx.rc_buffer_size = (bitrate / 2) as c_int;
            // One keyframe per segment, because a segment has to start on one.
            ctx.gop_size = gop as c_int;
            // No B-frames: decode order stays presentation order, which is what lets the
            // `trun` omit composition offsets entirely, and reordering is latency a live
            // duplicate has no use for.
            ctx.max_b_frames = 0;
            // Said here and in the init segment's `colr` box, because a player believes
            // whichever it finds and they must not disagree.
            ctx.colorspace = sys::AVColorSpace::AVCOL_SPC_BT709;
            ctx.color_primaries = sys::AVColorPrimaries::AVCOL_PRI_BT709;
            ctx.color_trc = sys::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            ctx.color_range = sys::AVColorRange::AVCOL_RANGE_MPEG;
            // Parameter sets out of band, which is where fMP4 wants them: in `avcC` in the
            // init segment, sent once, rather than in front of every keyframe.
            ctx.flags |= sys::AV_CODEC_FLAG_GLOBAL_HEADER as c_int;

            match self.choice.delivery {
                Delivery::Software => ctx.pix_fmt = sys::AVPixelFormat::AV_PIX_FMT_NV12,
                Delivery::Upload(family) => {
                    ctx.pix_fmt = family.pixel_format();
                    ctx.sw_pix_fmt = sys::AVPixelFormat::AV_PIX_FMT_NV12;
                }
            }
        }

        if let Delivery::Upload(family) = self.choice.delivery {
            // SAFETY: `self.ctx` is live and unopened; `attach_hw_frames` fills
            // `self.device`/`self.frames`, both of which are null on entry.
            unsafe { self.attach_hw_frames(family) }?;
        }

        // SAFETY: `self.ctx` is live and unopened, so `priv_data` is the encoder's own
        // options struct and every one of these is optional by construction.
        unsafe {
            let priv_data = (*self.ctx).priv_data;
            if !priv_data.is_null() {
                for (key, value) in self.choice.tuning {
                    if !try_set_opt(priv_data, key, value) {
                        tracing::debug!(
                            encoder = self.choice.name,
                            key,
                            value,
                            "encoder has no such option"
                        );
                    }
                }
            }
        }

        // SAFETY: context and codec are both live and the context has not been opened.
        let rc = unsafe { sys::avcodec_open2(self.ctx, codec, std::ptr::null_mut()) };
        if rc < 0 {
            return Err(PipelineError::Encode(format!(
                "avcodec_open2 failed ({})",
                av_error(rc)
            )));
        }

        // SAFETY: an opened encoder with `AV_CODEC_FLAG_GLOBAL_HEADER` has filled
        // `extradata`/`extradata_size`, or left the pointer null, which the guard catches.
        let extradata = unsafe {
            let ctx = &*self.ctx;
            if ctx.extradata.is_null() || ctx.extradata_size <= 0 {
                return Err(PipelineError::Encode(
                    "opened but published no parameter sets".into(),
                ));
            }
            std::slice::from_raw_parts(ctx.extradata, ctx.extradata_size as usize)
        };
        self.config = AvcConfig::from_extradata(extradata)?;

        // SAFETY: plain allocations; each is checked for null before it is used.
        unsafe {
            self.software = sys::av_frame_alloc();
            self.packet = sys::av_packet_alloc();
            if matches!(self.choice.delivery, Delivery::Upload(_)) {
                self.hardware = sys::av_frame_alloc();
            }
            if self.software.is_null()
                || self.packet.is_null()
                || (matches!(self.choice.delivery, Delivery::Upload(_)) && self.hardware.is_null())
            {
                return Err(PipelineError::Encode("out of memory".into()));
            }
            let frame = &mut *self.software;
            frame.format = sys::AVPixelFormat::AV_PIX_FMT_NV12 as c_int;
            frame.width = self.width as c_int;
            frame.height = self.height as c_int;
            frame.colorspace = sys::AVColorSpace::AVCOL_SPC_BT709;
            frame.color_range = sys::AVColorRange::AVCOL_RANGE_MPEG;
        }
        Ok(())
    }

    /// Open the hardware device and the frame pool the encoder will draw surfaces from.
    ///
    /// # Safety
    /// `self.ctx` must be live and unopened; `self.device` and `self.frames` must be null.
    unsafe fn attach_hw_frames(&mut self, family: HwFamily) -> Result<(), PipelineError> {
        // Serialised against every other device open in the process — the compositor's
        // Vulkan device, the decoder's VA-API one — because bringing up two driver stacks
        // on one GPU at once segfaults inside Mesa. See `crate::gpu_lock`.
        let _opening = crate::gpu_lock::opening_device();
        // SAFETY: writes a new reference into `self.device` on success and leaves it
        // untouched on failure. A null device name asks libavutil for the default render
        // node, which is right on a single-GPU box and is what the panel is.
        let rc = unsafe {
            sys::av_hwdevice_ctx_create(
                &raw mut self.device,
                family.device_type(),
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            )
        };
        if rc < 0 || self.device.is_null() {
            return Err(PipelineError::Encode(format!(
                "no hardware device ({})",
                av_error(rc)
            )));
        }
        // SAFETY: `self.device` is a live device context reference.
        self.frames = unsafe { sys::av_hwframe_ctx_alloc(self.device) };
        if self.frames.is_null() {
            return Err(PipelineError::Encode("no hardware frame pool".into()));
        }
        // SAFETY: `av_hwframe_ctx_alloc` guarantees `data` points at an `AVHWFramesContext`
        // that the caller fills before `av_hwframe_ctx_init`.
        unsafe {
            let frames = &mut *(*self.frames).data.cast::<sys::AVHWFramesContext>();
            frames.format = family.pixel_format();
            frames.sw_format = sys::AVPixelFormat::AV_PIX_FMT_NV12;
            frames.width = self.width as c_int;
            frames.height = self.height as c_int;
            // Deep enough that the encoder holding a couple of surfaces never starves the
            // upload, shallow enough that it is not a frame's worth of VRAM per slot for
            // nothing.
            frames.initial_pool_size = 8;
        }
        // SAFETY: the context is filled and has not been initialised.
        let rc = unsafe { sys::av_hwframe_ctx_init(self.frames) };
        if rc < 0 {
            return Err(PipelineError::Encode(format!(
                "hardware frame pool refused {}x{} ({})",
                self.width,
                self.height,
                av_error(rc)
            )));
        }
        // SAFETY: both pointers are live; the context takes its own reference, which
        // `avcodec_free_context` releases.
        unsafe {
            (*self.ctx).hw_frames_ctx = sys::av_buffer_ref(self.frames);
            if (*self.ctx).hw_frames_ctx.is_null() {
                return Err(PipelineError::Encode(
                    "could not reference frame pool".into(),
                ));
            }
        }
        Ok(())
    }

    /// Which encoder opened.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.choice.name
    }

    /// Whether frames go through the GPU's own pool rather than straight from memory.
    #[must_use]
    pub const fn delivery(&self) -> Delivery {
        self.choice.delivery
    }

    /// The parameter sets as currently understood.
    ///
    /// Not final until at least one frame has been encoded: the sets an encoder publishes
    /// in `extradata` are not always the ones its bitstream uses, and the first access
    /// unit is what settles it ([`AvcConfig::absorb`]). Use [`Self::describe`] when the
    /// answer is about to be written into an init segment.
    #[must_use]
    pub const fn config(&self) -> &AvcConfig {
        &self.config
    }

    /// The parameter sets, taken as final because an init segment is about to be written
    /// from them. A later change is then reported rather than silently applied.
    pub fn describe(&mut self) -> &AvcConfig {
        self.described = true;
        &self.config
    }

    /// Code the next frame as an IDR, whatever the GOP interval says.
    ///
    /// What a live subscriber needs to be able to start decoding at all: there is no init
    /// segment in RTP, so a viewer that joins mid-GOP has nothing until the next keyframe.
    /// At a keyframe a second that is most of a second of black, and after a loss it is
    /// most of a second of smear.
    pub const fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    /// Encode one frame, returning whatever coded pictures came out.
    ///
    /// Usually one, sometimes none — an encoder is entitled to buffer — so the caller
    /// takes a list rather than an `Option`.
    ///
    /// # Errors
    /// [`PipelineError::Encode`] if the frame is the wrong shape, the upload fails, or
    /// libavcodec refuses.
    pub fn encode(&mut self, planes: &Nv12Planes) -> Result<Vec<Sample>, PipelineError> {
        if (planes.width, planes.height) != (self.width, self.height) {
            return Err(PipelineError::Encode(format!(
                "encoder is {}x{}, frame is {}x{}",
                self.width, self.height, planes.width, planes.height
            )));
        }
        // The chroma plane has to actually be there. A short buffer would be read past its
        // end by libavcodec, not by us, which is the worst place for it to happen.
        let needed = planes.uv_offset + planes.uv_stride as usize * (planes.height as usize / 2);
        if planes.data.len() < needed {
            return Err(PipelineError::Encode(format!(
                "frame buffer is {} bytes, {needed} needed",
                planes.data.len()
            )));
        }

        // SAFETY: `self.software` is a live frame with no buffers of its own, so writing
        // its data pointers leaks nothing. The pointers reference `planes`, which outlives
        // this call; libavcodec's `av_frame_ref` on an unreferenced frame allocates and
        // copies, so nothing retains them past `avcodec_send_frame`.
        unsafe {
            let frame = &mut *self.software;
            frame.data[0] = planes.data.as_ptr().cast_mut();
            frame.linesize[0] = planes.y_stride as c_int;
            frame.data[1] = planes.data.as_ptr().add(planes.uv_offset).cast_mut();
            frame.linesize[1] = planes.uv_stride as c_int;
            frame.pts = self.pts;
        }
        self.pts += 1;

        let source = match self.choice.delivery {
            Delivery::Software => self.software,
            // SAFETY: the pool is live and `self.hardware` is a live frame; the unref
            // releases the previous frame's surface back to the pool before taking another.
            Delivery::Upload(_) => unsafe {
                sys::av_frame_unref(self.hardware);
                let rc = sys::av_hwframe_get_buffer(self.frames, self.hardware, 0);
                if rc < 0 {
                    return Err(PipelineError::Encode(format!(
                        "no surface from the pool ({})",
                        av_error(rc)
                    )));
                }
                let rc = sys::av_hwframe_transfer_data(self.hardware, self.software, 0);
                if rc < 0 {
                    return Err(PipelineError::Encode(format!(
                        "upload failed ({})",
                        av_error(rc)
                    )));
                }
                (*self.hardware).pts = (*self.software).pts;
                self.hardware
            },
        };

        // Asking for an IDR is done on whichever frame is actually sent, which on the
        // upload path is the hardware one — `av_hwframe_transfer_data` copies pixels, not
        // the picture type. Cleared immediately: it is one frame's instruction, and a
        // sticky one would code every frame as a keyframe at several times the bitrate.
        if self.force_keyframe {
            self.force_keyframe = false;
            // SAFETY: `source` is one of the two frames this struct owns, both live.
            unsafe {
                (*source).pict_type = sys::AVPictureType::AV_PICTURE_TYPE_I;
            }
        }

        // SAFETY: an opened encoder and a frame it accepts.
        let rc = unsafe { sys::avcodec_send_frame(self.ctx, source) };
        // Back to "the encoder decides" for every frame after this one.
        // SAFETY: as above; the send has returned, so libavcodec is done with the frame.
        unsafe {
            (*source).pict_type = sys::AVPictureType::AV_PICTURE_TYPE_NONE;
        }
        if rc < 0 && rc != sys::AVERROR(sys::EAGAIN) {
            return Err(PipelineError::Encode(format!(
                "send_frame failed ({})",
                av_error(rc)
            )));
        }
        let mut out = Vec::new();
        self.drain(&mut out)?;
        Ok(out)
    }

    /// Push out whatever the encoder is still holding. Called when the stream stops, so
    /// the last GOP is not lost with it.
    pub fn flush(&mut self) -> Vec<Sample> {
        // SAFETY: an opened encoder; a null frame is the documented flush signal.
        unsafe { sys::avcodec_send_frame(self.ctx, std::ptr::null()) };
        let mut out = Vec::new();
        if let Err(e) = self.drain(&mut out) {
            tracing::debug!(error = %e, "encoder flush");
        }
        out
    }

    /// Collect every packet the encoder is ready to hand over.
    fn drain(&mut self, out: &mut Vec<Sample>) -> Result<(), PipelineError> {
        loop {
            // SAFETY: an opened encoder and a live packet, which `avcodec_receive_packet`
            // unrefs itself before writing.
            let rc = unsafe { sys::avcodec_receive_packet(self.ctx, self.packet) };
            if rc == sys::AVERROR(sys::EAGAIN) || rc == sys::AVERROR_EOF {
                return Ok(());
            }
            if rc < 0 {
                return Err(PipelineError::Encode(format!(
                    "receive_packet failed ({})",
                    av_error(rc)
                )));
            }
            // SAFETY: a successful receive leaves a packet with `size` bytes at `data`.
            let (bytes, keyframe) = unsafe {
                let packet = &*self.packet;
                (
                    std::slice::from_raw_parts(packet.data, packet.size.max(0) as usize),
                    packet.flags & sys::AV_PKT_FLAG_KEY as c_int != 0,
                )
            };
            // The parameter sets this access unit carries in-band, if they disagree with
            // what `extradata` said. See `AvcConfig::absorb` for why one encoder does.
            if self.config.absorb(bytes) && self.described {
                // Past the init segment there is nowhere to put a new `avcC`: an `avc1`
                // track describes its decoder once. Nothing here can fix that, and a
                // silent one would be a picture that degrades for no visible reason.
                tracing::warn!(
                    encoder = self.choice.name,
                    "the encoder changed its parameter sets mid-stream; the init segment \
                     no longer describes it"
                );
            }
            // `None` where the access unit was nothing but parameter sets, which some
            // encoders emit beside a keyframe and which is not a picture.
            if let Some(data) = fmp4::annexb_to_avcc(bytes) {
                out.push(Sample {
                    data,
                    duration: self.duration,
                    keyframe,
                });
            }
            // SAFETY: the packet is live and this releases the reference just taken.
            unsafe { sys::av_packet_unref(self.packet) };
        }
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        // SAFETY: every one of these tolerates a null pointer and nulls what it frees, and
        // each pointer is owned solely by this struct. Order matters only in that the
        // context holds its own reference to the frame pool, released by
        // `avcodec_free_context`.
        unsafe {
            sys::av_packet_free(&raw mut self.packet);
            sys::av_frame_free(&raw mut self.software);
            sys::av_frame_free(&raw mut self.hardware);
            sys::avcodec_free_context(&raw mut self.ctx);
            sys::av_buffer_unref(&raw mut self.frames);
            sys::av_buffer_unref(&raw mut self.device);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::stream::hls::Segmenter;
    use std::time::Duration;

    /// A flat NV12 frame of one colour, laid out the way the readback produces them.
    fn planes(width: u32, height: u32, luma: u8) -> Nv12Planes {
        let stride = width.div_ceil(256) * 256;
        let uv_offset = (stride * height) as usize;
        let mut data = vec![luma; uv_offset + (stride * height / 2) as usize];
        data[uv_offset..].fill(128);
        Nv12Planes {
            width,
            height,
            data,
            y_stride: stride,
            uv_offset,
            uv_stride: stride,
        }
    }

    /// Open whatever this box has, or say why not and skip. CI has no GPU and may have no
    /// software encoder either, so this cannot be a hard requirement.
    fn encoder(width: u32, height: u32) -> Option<H264Encoder> {
        match H264Encoder::open(width, height, FrameRate::DEFAULT, 2_000_000, 30) {
            Ok(e) => Some(e),
            Err(e) => {
                eprintln!("no encoder here, skipping: {e}");
                None
            }
        }
    }

    #[test]
    fn an_encoder_publishes_parameter_sets_before_any_frame() {
        // The init segment is written from these, and it has to exist before the first
        // media segment — so an encoder that only reveals them in-band on the first
        // keyframe would leave a playlist pointing at an `EXT-X-MAP` we cannot write.
        let Some(encoder) = encoder(320, 240) else {
            return;
        };
        assert!(!encoder.config().sps.is_empty());
        assert!(!encoder.config().pps.is_empty());
        assert!(
            encoder.config().codec_string().starts_with("avc1."),
            "{}",
            encoder.config().codec_string()
        );
    }

    #[test]
    fn the_first_frame_out_is_one_a_player_can_start_on() {
        let Some(mut encoder) = encoder(320, 240) else {
            return;
        };
        let mut samples = Vec::new();
        for i in 0..10 {
            samples.extend(encoder.encode(&planes(320, 240, 40 + i * 4)).unwrap());
        }
        samples.extend(encoder.flush());
        assert!(!samples.is_empty(), "ten frames in, nothing out");
        assert!(
            samples[0].keyframe,
            "a stream has to be joinable at its head"
        );
        assert!(samples.iter().all(|s| !s.data.is_empty()));
    }

    #[test]
    fn samples_are_length_prefixed_not_start_code_delimited() {
        // The one conversion between libavcodec's world and MP4's. Getting it wrong
        // produces a segment that is exactly the right size and decodes to nothing.
        let Some(mut encoder) = encoder(320, 240) else {
            return;
        };
        let mut samples = Vec::new();
        for _ in 0..5 {
            samples.extend(encoder.encode(&planes(320, 240, 60)).unwrap());
        }
        samples.extend(encoder.flush());
        for sample in &samples {
            // An access unit is a *run* of NALs — SEI ahead of the slice, several slices
            // where the encoder split the picture — so what is checked is that the length
            // prefixes tile the sample exactly. A sample still in Annex-B would fail at
            // the first hop, because `00 00 00 01` reads as a one-byte NAL.
            let mut at = 0usize;
            let mut nals = 0;
            while at < sample.data.len() {
                let len = u32::from_be_bytes(sample.data[at..at + 4].try_into().unwrap()) as usize;
                assert!(len > 0, "zero-length NAL at {at}");
                at += 4 + len;
                nals += 1;
                assert!(at <= sample.data.len(), "NAL {nals} runs past the sample");
            }
            assert_eq!(at, sample.data.len(), "the NALs do not tile the sample");
        }
    }

    #[test]
    fn a_frame_of_the_wrong_shape_is_refused_rather_than_encoded() {
        // The panel can resize under a running stream. Handing libavcodec a frame whose
        // linesize does not match the context is a read past the end of our buffer, inside
        // C, which is the worst place for it to happen.
        let Some(mut encoder) = encoder(320, 240) else {
            return;
        };
        let err = encoder.encode(&planes(160, 120, 60)).unwrap_err();
        assert!(matches!(err, PipelineError::Encode(_)), "{err:?}");
    }

    #[test]
    fn a_truncated_buffer_is_refused_before_libavcodec_sees_it() {
        let Some(mut encoder) = encoder(320, 240) else {
            return;
        };
        let mut short = planes(320, 240, 60);
        short.data.truncate(short.uv_offset + 8);
        let err = encoder.encode(&short).unwrap_err();
        assert!(matches!(err, PipelineError::Encode(_)), "{err:?}");
    }

    #[test]
    fn a_second_of_frames_segments_into_a_playable_second() {
        // The whole chain below the tap: encode, repack, cut. What it pins is that the
        // encoder's keyframe interval and the segmenter's target agree — if they do not,
        // segments come out at whatever multiple of the GOP happens to land, and the
        // playlist's target duration stops being true.
        let Some(mut encoder) = encoder(320, 240) else {
            return;
        };
        let mut segmenter = Segmenter::new(Duration::from_secs(1));
        let mut segments = Vec::new();
        for i in 0..90 {
            for sample in encoder.encode(&planes(320, 240, 40 + (i % 8) * 8)).unwrap() {
                if let Some(segment) = segmenter.push_video(sample) {
                    segments.push(segment);
                }
            }
        }
        assert!(
            segments.len() >= 2,
            "three seconds in, {} segments out",
            segments.len()
        );
        for segment in &segments {
            assert!(segment.bytes.len() > 100);
            assert_eq!(&segment.bytes[4..8], b"moof");
        }
    }
}
