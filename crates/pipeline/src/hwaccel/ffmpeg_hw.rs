//! Wiring a hardware decoder into libavcodec, which `ffmpeg-next` 7.1 wraps none of.
//!
//! There is no `hw_device_ctx`, no `get_format`, and no `AVHWFramesContext` anywhere in
//! the safe crate — every piece here is `ffmpeg_sys_next` reached through
//! `codec::Context::as_mut_ptr()`. Ground rule 8 allows that in `pipeline`; what it costs
//! is that each block states the invariant it relies on.
//!
//! Three things happen here, all before the decoder is opened:
//!
//! 1. A device context is created for the platform's decode API and attached, which is
//!    what tells libavcodec a fixed-function decoder is available at all.
//! 2. `get_format` is installed. libavcodec calls it once the stream's parameters are
//!    known, offering a list of pixel formats; picking the hardware one is what commits
//!    to GPU surfaces, and the list *not containing it* is how the driver says "not this
//!    profile, not this bit depth, not this many reference frames". That refusal is
//!    routine, not exceptional.
//! 3. Latency flags. A hwaccel decoder left to its own devices buffers two or three
//!    frames of reordering — the right trade for a film, exactly the wrong one for a live
//!    mirror, where late frames get dropped anyway (ground rule 4).
#![allow(
    unsafe_code,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::ptr_as_ptr
)]

use std::cell::Cell;
use std::ffi::{c_int, c_void};

use castaway_core::ColorInfo;
use ffmpeg_sys_next as sys;

use super::{HwBackendKind, HwGiveUp};
use crate::color::{self, SignalledRange, SignalledSpace};

/// An `AVHWDeviceContext` reference.
///
/// One per decode session. Opening it is the first thing that can fail — no render node,
/// no driver, a VM with no GPU passed through — and the failure is a plain
/// [`HwGiveUp::DeviceUnavailable`] rather than an error, because software decode is
/// waiting right there.
pub struct HwDevice {
    ptr: *mut sys::AVBufferRef,
    kind: HwBackendKind,
}

// SAFETY: `AVBufferRef` is internally reference-counted with atomics, and this wrapper
// hands out the pointer only to libavcodec calls made from the single decode thread that
// owns it. Moving that sole owner between threads introduces no sharing.
unsafe impl Send for HwDevice {}

impl Drop for HwDevice {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from `av_hwdevice_ctx_create` and is unreffed exactly once,
        // here. `av_buffer_unref` tolerates and nulls the pointer.
        unsafe { sys::av_buffer_unref(&raw mut self.ptr) };
    }
}

impl HwDevice {
    /// The libavutil device type for a backend family.
    const fn device_type(kind: HwBackendKind) -> sys::AVHWDeviceType {
        match kind {
            HwBackendKind::Vaapi => sys::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            HwBackendKind::D3d11Va => sys::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
        }
    }

    /// Open the platform's decode device.
    ///
    /// # Errors
    /// [`HwGiveUp::DeviceUnavailable`] if libavutil cannot create the context.
    pub fn open(kind: HwBackendKind) -> Result<Self, HwGiveUp> {
        let mut ptr: *mut sys::AVBufferRef = std::ptr::null_mut();
        // SAFETY: `av_hwdevice_ctx_create` writes a new reference into `ptr` on success
        // and leaves it untouched on failure. Null device/opts asks libavutil to pick the
        // default adapter, which is what we want on a single-GPU box.
        let rc = unsafe {
            sys::av_hwdevice_ctx_create(
                &raw mut ptr,
                Self::device_type(kind),
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            )
        };
        if rc < 0 || ptr.is_null() {
            return Err(HwGiveUp::DeviceUnavailable(format!(
                "{kind}: av_hwdevice_ctx_create failed ({})",
                av_error(rc)
            )));
        }
        Ok(Self { ptr, kind })
    }

    /// Which backend this device drives.
    #[must_use]
    pub const fn kind(&self) -> HwBackendKind {
        self.kind
    }

    /// The underlying `AVBufferRef`, for backends that need to reach into the device
    /// context (Windows needs the `ID3D11Device` inside it).
    #[must_use]
    pub const fn raw(&self) -> *mut sys::AVBufferRef {
        self.ptr
    }
}

// The hardware pixel format `get_format` must select, for the current decode thread, and
// whether the last negotiation refused it.
//
// Thread-locals rather than `AVCodecContext::opaque`: `opaque` is a field the wrapper
// crate could start using, and frame-level threading is disabled on this path anyway (see
// `apply_low_latency`), so `get_format` runs on the same thread that installed the value.
thread_local! {
    static WANTED_FORMAT: Cell<sys::AVPixelFormat> = const {
        Cell::new(sys::AVPixelFormat::AV_PIX_FMT_NONE)
    };
    static FORMAT_REJECTED: Cell<bool> = const { Cell::new(false) };
}

/// libavcodec's format-negotiation callback.
///
/// Returning the hardware format commits the decoder to GPU surfaces. Returning anything
/// else means software output — which is a legitimate answer, not a crash, so when the
/// hardware format is absent from the offered list we flag it for the caller and hand
/// back libavcodec's own first choice. The decode loop then sees software frames coming
/// out of a decoder it asked to be hardware, notices the flag, and falls back explicitly
/// with a log line instead of silently rendering at half the frame rate.
unsafe extern "C" fn choose_format(
    _ctx: *mut sys::AVCodecContext,
    formats: *const sys::AVPixelFormat,
) -> sys::AVPixelFormat {
    let wanted = WANTED_FORMAT.with(Cell::get);
    if formats.is_null() {
        FORMAT_REJECTED.with(|c| c.set(true));
        return sys::AVPixelFormat::AV_PIX_FMT_NONE;
    }

    // SAFETY: libavcodec guarantees `formats` is a `AV_PIX_FMT_NONE`-terminated array
    // that outlives this call. The bound is a belt-and-braces stop in case it is not.
    let mut software = sys::AVPixelFormat::AV_PIX_FMT_NONE;
    for i in 0..64_isize {
        let fmt = unsafe { *formats.offset(i) };
        if fmt == sys::AVPixelFormat::AV_PIX_FMT_NONE {
            break;
        }
        if fmt == wanted {
            return fmt;
        }
        // The fallback has to be a *software* format. The list routinely contains other
        // hardware formats (CUDA, VDPAU…) that this codec supports but our device context
        // is not for; returning one of those makes libavcodec reject the setup outright
        // instead of decoding on the CPU, turning a graceful downgrade into a dead stream.
        // SAFETY: `fmt` came from libavcodec's own list, so it has a descriptor.
        if software == sys::AVPixelFormat::AV_PIX_FMT_NONE && !unsafe { is_hardware_format(fmt) } {
            software = fmt;
        }
    }

    FORMAT_REJECTED.with(|c| c.set(true));
    software
}

/// Whether a pixel format denotes a surface that lives on an accelerator rather than
/// pixels in system memory.
///
/// # Safety
/// `fmt` must be a pixel format libavutil knows.
unsafe fn is_hardware_format(fmt: sys::AVPixelFormat) -> bool {
    // SAFETY: `av_pix_fmt_desc_get` returns null for an unknown format, which is handled.
    let desc = unsafe { sys::av_pix_fmt_desc_get(fmt) };
    if desc.is_null() {
        return false;
    }
    // SAFETY: non-null, and the descriptor table is static for the process.
    unsafe { (*desc).flags & sys::AV_PIX_FMT_FLAG_HWACCEL as u64 != 0 }
}

/// Find the pixel format this codec's hardware config uses, if it has one for `kind`.
///
/// # Safety
/// `codec` must be a valid `AVCodec` pointer from `avcodec_find_decoder`.
unsafe fn hw_pixel_format(
    codec: *const sys::AVCodec,
    kind: HwBackendKind,
) -> Option<sys::AVPixelFormat> {
    let want = HwDevice::device_type(kind);
    for i in 0..64 {
        // SAFETY: `avcodec_get_hw_config` returns null past the end of the config list,
        // which terminates the loop.
        let config = unsafe { sys::avcodec_get_hw_config(codec, i) };
        if config.is_null() {
            return None;
        }
        // SAFETY: non-null, and libavcodec owns the static config for the process.
        let config = unsafe { &*config };
        let by_device_ctx =
            config.methods & sys::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as c_int != 0;
        if by_device_ctx && config.device_type == want {
            return Some(config.pix_fmt);
        }
    }
    None
}

/// Everything about a decoder that has been pointed at hardware.
pub struct HwSetup {
    /// The pixel format decoded frames will carry when they really are GPU surfaces.
    pub hw_format: sys::AVPixelFormat,
    /// The device context, kept alive for the decoder's lifetime.
    pub device: HwDevice,
}

/// Point an **unopened** decoder context at a hardware device, looking the decoder up by
/// codec id.
///
/// The URL path builds its context from stream parameters and never holds an `AVCodec`
/// pointer of its own, so it hands over the id instead.
///
/// # Safety
/// `ctx` must be an allocated, not-yet-opened `AVCodecContext` that will be opened with
/// the decoder for `codec_id`.
///
/// # Errors
/// [`HwGiveUp::CodecUnsupported`] if there is no decoder, or none with a hardware config.
pub unsafe fn attach_for_id(
    ctx: *mut sys::AVCodecContext,
    codec_id: sys::AVCodecID,
    device: HwDevice,
) -> Result<HwSetup, HwGiveUp> {
    // SAFETY: `avcodec_find_decoder` returns null or a static codec for the process.
    let codec = unsafe { sys::avcodec_find_decoder(codec_id) };
    if codec.is_null() {
        return Err(HwGiveUp::CodecUnsupported(device.kind()));
    }
    // SAFETY: caller guarantees the context; `codec` is non-null and static.
    unsafe { attach(ctx, codec, device) }
}

/// Point an **unopened** decoder context at a hardware device.
///
/// # Safety
/// `ctx` must be an allocated, not-yet-opened `AVCodecContext`, and `codec` the decoder it
/// will be opened with. Both must outlive the returned [`HwSetup`].
///
/// # Errors
/// [`HwGiveUp::CodecUnsupported`] if this decoder has no hardware config for the device.
pub unsafe fn attach(
    ctx: *mut sys::AVCodecContext,
    codec: *const sys::AVCodec,
    device: HwDevice,
) -> Result<HwSetup, HwGiveUp> {
    // SAFETY: caller guarantees `codec` is valid.
    let hw_format = unsafe { hw_pixel_format(codec, device.kind()) }
        .ok_or(HwGiveUp::CodecUnsupported(device.kind()))?;

    WANTED_FORMAT.with(|c| c.set(hw_format));
    FORMAT_REJECTED.with(|c| c.set(false));

    // SAFETY: `ctx` is allocated and not yet opened, so writing these fields is the
    // documented way to configure it. `av_buffer_ref` takes a new reference, so the
    // context's unref at close does not invalidate our `HwDevice`.
    unsafe {
        (*ctx).hw_device_ctx = sys::av_buffer_ref(device.ptr);
        (*ctx).get_format = Some(choose_format);
    }
    // SAFETY: same context, still unopened.
    unsafe { apply_low_latency(ctx) };

    Ok(HwSetup { hw_format, device })
}

/// Trade reordering depth for latency on an unopened decoder.
///
/// A hwaccel decoder will otherwise hold two or three frames before emitting the first,
/// and frame-level threading adds another queue on top. Both are the wrong trade for a
/// live mirror: we drop late frames anyway, so buffering to smooth them out just moves
/// the whole stream further behind the sender's screen.
///
/// # Safety
/// `ctx` must be an allocated, not-yet-opened `AVCodecContext`.
pub unsafe fn apply_low_latency(ctx: *mut sys::AVCodecContext) {
    // SAFETY: caller guarantees an unopened context; these are plain scalar fields that
    // avcodec_open2 reads.
    unsafe {
        (*ctx).flags |= sys::AV_CODEC_FLAG_LOW_DELAY as c_int;
        // Slice threading is fine — it does not reorder output. Frame threading is what
        // introduces the extra queue, so it is switched off explicitly rather than by
        // pinning thread_count to 1, which would also cost slice parallelism.
        (*ctx).thread_type &= !(sys::FF_THREAD_FRAME as c_int);
    }
}

/// Whether `get_format` was offered a list without our hardware format — i.e. the driver
/// declined this stream. Clears the flag.
#[must_use]
pub fn take_format_rejected() -> bool {
    FORMAT_REJECTED.with(|c| c.replace(false))
}

/// Read a decoded frame's colorimetry, resolving what the stream left unsaid.
///
/// # Safety
/// `frame` must point at a decoded `AVFrame`.
#[must_use]
pub unsafe fn color_of(frame: *const sys::AVFrame) -> ColorInfo {
    // SAFETY: caller guarantees a live decoded frame; these are scalar fields.
    let (space, range, height) = unsafe {
        let f = &*frame;
        (f.colorspace, f.color_range, f.height)
    };
    color::resolve(
        signalled_space(space),
        signalled_range(range),
        u32::try_from(height).unwrap_or(0),
    )
}

const fn signalled_space(space: sys::AVColorSpace) -> SignalledSpace {
    use sys::AVColorSpace as S;
    match space {
        S::AVCOL_SPC_BT709 => SignalledSpace::Bt709,
        // BT.470BG and SMPTE 170M are the same matrix, spelled for PAL and NTSC.
        S::AVCOL_SPC_BT470BG | S::AVCOL_SPC_SMPTE170M | S::AVCOL_SPC_SMPTE240M => {
            SignalledSpace::Bt601
        }
        S::AVCOL_SPC_BT2020_NCL => SignalledSpace::Bt2020Ncl,
        S::AVCOL_SPC_UNSPECIFIED => SignalledSpace::Unspecified,
        _ => SignalledSpace::Unsupported,
    }
}

const fn signalled_range(range: sys::AVColorRange) -> SignalledRange {
    use sys::AVColorRange as R;
    match range {
        R::AVCOL_RANGE_JPEG => SignalledRange::Full,
        R::AVCOL_RANGE_MPEG => SignalledRange::Limited,
        _ => SignalledRange::Unspecified,
    }
}

/// Render a libav return code as something a log line can carry.
///
/// Lives in [`crate::av`] now: the encoder needs it too, and it is not gated on this
/// module's feature.
pub use crate::av::av_error;

/// Unused-parameter sink for the `c_void` import, which only some platforms need.
const _: Option<*mut c_void> = None;

#[cfg(test)]
mod tests {
    use super::*;
    use castaway_core::{ColorRange, ColorSpace};

    #[test]
    fn ffmpeg_colorspace_tags_map_to_our_matrices() {
        // Both spellings of BT.601 must land on the same matrix — a stream tagged
        // SMPTE170M treated as "unknown" would get the height heuristic instead, which
        // is right often enough to hide the bug and wrong often enough to matter.
        use sys::AVColorSpace as S;
        assert_eq!(signalled_space(S::AVCOL_SPC_BT470BG), SignalledSpace::Bt601);
        assert_eq!(
            signalled_space(S::AVCOL_SPC_SMPTE170M),
            SignalledSpace::Bt601,
        );
        assert_eq!(signalled_space(S::AVCOL_SPC_BT709), SignalledSpace::Bt709);
        assert_eq!(
            signalled_space(S::AVCOL_SPC_BT2020_NCL),
            SignalledSpace::Bt2020Ncl,
        );
        assert_eq!(
            signalled_space(S::AVCOL_SPC_UNSPECIFIED),
            SignalledSpace::Unspecified,
        );
    }

    #[test]
    fn ffmpeg_range_tags_map_to_ours() {
        use sys::AVColorRange as R;
        assert_eq!(signalled_range(R::AVCOL_RANGE_JPEG), SignalledRange::Full);
        assert_eq!(
            signalled_range(R::AVCOL_RANGE_MPEG),
            SignalledRange::Limited,
        );
        assert_eq!(
            signalled_range(R::AVCOL_RANGE_UNSPECIFIED),
            SignalledRange::Unspecified,
        );
    }

    #[test]
    fn an_unlabelled_hd_frame_resolves_to_bt709_limited() {
        let got = color::resolve(
            signalled_space(sys::AVColorSpace::AVCOL_SPC_UNSPECIFIED),
            signalled_range(sys::AVColorRange::AVCOL_RANGE_UNSPECIFIED),
            1080,
        );
        assert_eq!(
            got,
            ColorInfo {
                space: ColorSpace::Bt709,
                range: ColorRange::Limited
            }
        );
    }

    #[test]
    fn h264_reports_a_hardware_config_for_this_platform() {
        // Not an availability check — this asks whether the *build* of ffmpeg we link
        // has a hwaccel config for H.264 at all. If it does not, no amount of driver
        // will help and the fallback path is the only one that will ever run.
        // SAFETY: `avcodec_find_decoder` returns either null or a static codec.
        let codec = unsafe { sys::avcodec_find_decoder(sys::AVCodecID::AV_CODEC_ID_H264) };
        assert!(!codec.is_null(), "no H.264 decoder in this ffmpeg build");
        let Some(kind) = HwBackendKind::for_this_platform() else {
            return;
        };
        // SAFETY: `codec` is non-null and static.
        let format = unsafe { hw_pixel_format(codec, kind) };
        assert!(
            format.is_some(),
            "this ffmpeg build has no {kind} config for H.264",
        );
    }

    #[test]
    fn av_error_renders_a_known_code() {
        // The give-up messages are the only diagnostic surface on a box nobody can
        // attach a debugger to, so a numeric-only fallback would be a real loss.
        let text = av_error(sys::AVERROR(sys::EINVAL));
        assert!(!text.is_empty());
        assert!(!text.starts_with("error "), "{text}");
    }
}
