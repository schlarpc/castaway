//! The safe boundary around moonlight-common-c.
//!
//! Everything past the NVHTTP launch — RTSP, the ENet control stream, FEC'd RTP video,
//! encrypted Opus audio, input encoding — belongs to the linked C library (D37). This
//! module is the whole of castaway's contact with it: it converts a launch into a
//! `LiStartConnection` call, and converts the library's callbacks into the channels
//! `SessionEvent::Mirror` carries.
//!
//! Three constraints the C API imposes, which shape everything here:
//!
//! - **It is a process singleton.** `LiStartConnection`/`LiStopConnection` are
//!   documented as not thread-safe, the library keeps global state, and several
//!   callbacks (`DecoderRendererStart`, `DecoderRendererStop`) take no context pointer
//!   at all, so there is nowhere to hang a per-session handle. [`SESSION`] is that
//!   global, and [`StreamSession::start`] refuses a second concurrent session rather
//!   than letting two sets of callbacks race for one static.
//! - **Callbacks arrive on the library's own threads.** They are not tokio threads, so
//!   sending into a tokio channel from them is a blocking send — which is exactly what
//!   we want for audio (back-pressure) and exactly what we do not want for video,
//!   where a late frame should be dropped instead (ground rule 4).
//! - **`LiStartConnection` blocks** through the whole handshake. It runs on
//!   `spawn_blocking`; the adapter awaits the join handle.
//!
//! Unsafe is confined here and to `moonlight-sys`, per ground rule 8.
#![allow(unsafe_code)]

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use castaway_core::{AudioCodec, AudioFormat, EncodedFrame, FrameSource, MirrorAudio, VideoCodec};
use moonlight_sys as sys;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::error::GameStreamError;
use crate::nvhttp::ServerInfo;

/// Video frames in flight. Small on purpose: this is a live mirror, so a full queue
/// means the renderer is behind and the newest frame matters more than the queued one.
const VIDEO_QUEUE: usize = 8;
/// Audio frames in flight. Deeper — dropping audio is audible where dropping a video
/// frame is not.
const AUDIO_QUEUE: usize = 512;

/// What the streaming session should ask the host for.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// Stream width in pixels.
    pub width: u32,
    /// Stream height in pixels.
    pub height: u32,
    /// Frame rate.
    pub fps: u32,
    /// Video bitrate in kbps, inclusive of FEC overhead.
    pub bitrate_kbps: u32,
    /// The AES key handed to `/launch` as `rikey`; it keys input, control, and audio.
    pub ri_key: [u8; 16],
    /// The IV whose first four bytes were sent as `rikeyid`.
    pub ri_key_iv: [u8; 16],
    /// Allow HEVC when the host offers it. H.264 is always allowed.
    pub allow_hevc: bool,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20_000,
            ri_key: [0; 16],
            ri_key_iv: [0; 16],
            // Off by default for the same reason AirPlay's HEVC offer is: the panel's
            // decode path is proven on H.264, and a codec we negotiate but decode
            // badly looks like a broken host.
            allow_hevc: false,
        }
    }
}

/// A live streaming session. Dropping it stops the stream.
pub struct StreamSession {
    _private: (),
}

impl StreamSession {
    /// Start streaming from a launched session. Blocking through the handshake — call
    /// from `spawn_blocking`.
    ///
    /// Returns the video and audio sources to hand to `SessionEvent::Mirror`. The
    /// session runs until [`StreamSession::stop`] or the host ends it.
    ///
    /// # Errors
    /// [`GameStreamError::Stream`] if the handshake fails (the stage name says which
    /// half of the protocol), or if a session is already running in this process.
    pub fn start(
        server: &ServerInfo,
        host_address: &str,
        session_url: &str,
        config: &StreamConfig,
    ) -> Result<(Self, FrameSource, MirrorAudio), GameStreamError> {
        let (video_tx, video_rx) = mpsc::channel(VIDEO_QUEUE);
        let (audio_tx, audio_rx) = mpsc::channel(AUDIO_QUEUE);
        let sinks = Sinks {
            video: video_tx,
            audio: audio_tx,
            audio_pts: Mutex::new(Duration::ZERO),
            codec: Mutex::new(VideoCodec::H264),
            audio_format: Mutex::new(AudioFormat::from_hz(48_000, 2)),
        };

        // Claim the singleton before touching the library at all.
        let slot = SESSION.get_or_init(|| Mutex::new(None));
        {
            let mut guard = slot.lock().map_err(|_| GameStreamError::Stream {
                stage: "session lock".into(),
                code: 0,
            })?;
            if guard.is_some() {
                return Err(GameStreamError::Stream {
                    stage: "session start".into(),
                    // moonlight-common-c keeps global state, so a second concurrent
                    // session would corrupt the first rather than fail cleanly.
                    code: -1,
                });
            }
            *guard = Some(sinks);
        }
        LAST_STAGE_ERROR.store(0, Ordering::SeqCst);

        let result = unsafe { start_connection(server, host_address, session_url, config) };
        if let Err(e) = result {
            clear_session();
            return Err(e);
        }

        let audio = MirrorAudio {
            source: FrameSource::Encoded(audio_rx),
            // The negotiated Opus configuration arrives in the audio init callback;
            // 48 kHz stereo is what a default GameStream session negotiates and what
            // the callback overwrites if the host chose otherwise.
            format: AudioFormat::from_hz(48_000, 2).unwrap_or_else(fallback_format),
            // Opus carries its own configuration in-band.
            config: None,
        };
        Ok((Self { _private: () }, FrameSource::Encoded(video_rx), audio))
    }

    /// Stop the session and release the singleton. Blocking.
    pub fn stop(self) {
        drop(self);
    }
}

impl Drop for StreamSession {
    fn drop(&mut self) {
        // SAFETY: the singleton guarantees exactly one live session, so this is the
        // matching LiStopConnection for the LiStartConnection that built `self`.
        unsafe { sys::LiStopConnection() };
        clear_session();
    }
}

fn fallback_format() -> AudioFormat {
    // 48 kHz stereo is expressible; this arm exists only because AudioFormat has no
    // Default, deliberately.
    AudioFormat::from_hz(48_000, 2).unwrap_or_else(|| unreachable!("48 kHz stereo is valid"))
}

/// The channels the C callbacks push into. One per process, because several callbacks
/// take no context pointer.
struct Sinks {
    video: mpsc::Sender<EncodedFrame>,
    audio: mpsc::Sender<EncodedFrame>,
    audio_pts: Mutex<Duration>,
    codec: Mutex<VideoCodec>,
    audio_format: Mutex<Option<AudioFormat>>,
}

static SESSION: OnceLock<Mutex<Option<Sinks>>> = OnceLock::new();
/// The last `stageFailed` code, since `LiStartConnection`'s return value alone does not
/// say which stage broke.
static LAST_STAGE_ERROR: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static LAST_STAGE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
/// Set when the host ends the session, so the adapter can distinguish "the game quit"
/// from "the network died".
static TERMINATED: AtomicBool = AtomicBool::new(false);

fn clear_session() {
    if let Some(slot) = SESSION.get() {
        if let Ok(mut guard) = slot.lock() {
            *guard = None;
        }
    }
}

fn with_sinks<R>(f: impl FnOnce(&Sinks) -> R) -> Option<R> {
    let guard = SESSION.get()?.lock().ok()?;
    guard.as_ref().map(f)
}

/// Whether the host ended the last session gracefully (the app exited).
#[must_use]
pub fn ended_gracefully() -> bool {
    TERMINATED.load(Ordering::SeqCst)
}

/// Build the C structs and call `LiStartConnection`.
///
/// # Safety
/// Caller must hold the [`SESSION`] claim, so the callbacks below have sinks to push
/// into and no other session is running.
unsafe fn start_connection(
    server: &ServerInfo,
    host_address: &str,
    session_url: &str,
    config: &StreamConfig,
) -> Result<(), GameStreamError> {
    let address = CString::new(host_address).map_err(|_| GameStreamError::Stream {
        stage: "address".into(),
        code: 0,
    })?;
    let app_version = CString::new(server.app_version.clone()).unwrap_or_default();
    let gfe_version = CString::new(server.gfe_version.clone()).unwrap_or_default();
    let rtsp_url = CString::new(session_url).unwrap_or_default();

    let mut server_info: sys::SERVER_INFORMATION = std::mem::zeroed();
    // SAFETY: zeroing then filling is what LiInitializeServerInformation does; the
    // CStrings outlive the call because LiStartConnection copies what it needs before
    // returning (it blocks through the whole handshake).
    server_info.address = address.as_ptr();
    server_info.serverInfoAppVersion = app_version.as_ptr();
    server_info.serverInfoGfeVersion = gfe_version.as_ptr();
    server_info.rtspSessionUrl = rtsp_url.as_ptr();
    server_info.serverCodecModeSupport =
        i32::try_from(server.codec_mode_support).unwrap_or(sys::SCM_H264);

    let mut stream: sys::STREAM_CONFIGURATION = std::mem::zeroed();
    stream.width = i32::try_from(config.width).unwrap_or(1920);
    stream.height = i32::try_from(config.height).unwrap_or(1080);
    stream.fps = i32::try_from(config.fps).unwrap_or(60);
    stream.bitrate = i32::try_from(config.bitrate_kbps).unwrap_or(20_000);
    // 1024 is the library's own "if unsure" value and the one that survives a LAN with
    // an unhelpful MTU.
    stream.packetSize = 1024;
    stream.streamingRemotely = sys::STREAM_CFG_LOCAL;
    stream.audioConfiguration = audio_configuration_stereo();
    stream.supportedVideoFormats = if config.allow_hevc {
        sys::VIDEO_FORMAT_H264 | sys::VIDEO_FORMAT_H265
    } else {
        sys::VIDEO_FORMAT_H264
    };
    stream.clientRefreshRateX100 = i32::try_from(config.fps * 100).unwrap_or(6000);
    stream.colorSpace = sys::COLORSPACE_REC_709;
    stream.colorRange = sys::COLOR_RANGE_LIMITED;
    // Encrypt everything the host will agree to. The panel is not bandwidth- or
    // CPU-starved, and Sunshine can require it.
    // ENCFLG_ALL is 0xFFFFFFFF, which the C header uses as an int-typed "every flag".
    // bindgen widens it to i64 to preserve the literal, so it does not fit a c_int —
    // and -1 is the same 32 bits, which is what the C code reads either way.
    stream.encryptionFlags = c_int::try_from(sys::ENCFLG_ALL).unwrap_or(-1);
    for (dst, src) in stream
        .remoteInputAesKey
        .iter_mut()
        .zip(config.ri_key.iter())
    {
        *dst = src.cast_signed();
    }
    for (dst, src) in stream
        .remoteInputAesIv
        .iter_mut()
        .zip(config.ri_key_iv.iter())
    {
        *dst = src.cast_signed();
    }

    let mut video_callbacks: sys::DECODER_RENDERER_CALLBACKS = std::mem::zeroed();
    video_callbacks.setup = Some(video_setup);
    video_callbacks.submitDecodeUnit = Some(video_submit);
    // We hand frames to a channel and never block, so the library may submit straight
    // from its receive thread rather than paying for its own queue.
    video_callbacks.capabilities = sys::CAPABILITY_DIRECT_SUBMIT;

    let mut audio_callbacks: sys::AUDIO_RENDERER_CALLBACKS = std::mem::zeroed();
    audio_callbacks.init = Some(audio_init);
    audio_callbacks.decodeAndPlaySample = Some(audio_sample);
    audio_callbacks.capabilities = sys::CAPABILITY_DIRECT_SUBMIT;

    let mut conn_callbacks: sys::CONNECTION_LISTENER_CALLBACKS = std::mem::zeroed();
    conn_callbacks.stageStarting = Some(stage_starting);
    conn_callbacks.stageFailed = Some(stage_failed);
    conn_callbacks.connectionStarted = Some(connection_started);
    conn_callbacks.connectionTerminated = Some(connection_terminated);
    // logMessage is left null deliberately. It is a printf-style variadic, which Rust
    // cannot define on stable, and the library substitutes its own default; its
    // messages duplicate what the stage callbacks above already report.

    TERMINATED.store(false, Ordering::SeqCst);
    let rc = sys::LiStartConnection(
        &raw mut server_info,
        &raw mut stream,
        &raw mut conn_callbacks,
        &raw mut video_callbacks,
        &raw mut audio_callbacks,
        std::ptr::null_mut(),
        0,
        std::ptr::null_mut(),
        0,
    );
    if rc == 0 {
        return Ok(());
    }
    let stage = LAST_STAGE.load(Ordering::SeqCst);
    // SAFETY: LiGetStageName returns a static string for any int.
    let name = CStr::from_ptr(sys::LiGetStageName(stage))
        .to_string_lossy()
        .into_owned();
    Err(GameStreamError::Stream {
        stage: name,
        code: LAST_STAGE_ERROR.load(Ordering::SeqCst),
    })
}

/// `MAKE_AUDIO_CONFIGURATION(2, 0x3)` — the macro bindgen cannot translate.
const fn audio_configuration_stereo() -> c_int {
    (0x3 << 16) | (2 << 8) | 0xCA
}

// --- C callbacks ------------------------------------------------------------------
//
// Every one of these is called from a thread moonlight-common-c owns. They must not
// panic (unwinding into C is undefined), so each one is total: no unwrap, no indexing.

extern "C" fn video_setup(
    video_format: c_int,
    width: c_int,
    height: c_int,
    _redraw_rate: c_int,
    _context: *mut c_void,
    _flags: c_int,
) -> c_int {
    let codec = if video_format & sys::VIDEO_FORMAT_MASK_H265 != 0 {
        VideoCodec::Hevc
    } else {
        VideoCodec::H264
    };
    info!(?codec, width, height, "GameStream video negotiated");
    with_sinks(|sinks| {
        if let Ok(mut slot) = sinks.codec.lock() {
            *slot = codec;
        }
    });
    0
}

extern "C" fn video_submit(unit: *mut sys::DECODE_UNIT) -> c_int {
    // SAFETY: the library hands us a valid unit for the duration of this call, with a
    // buffer chain it owns; we copy out and never retain a pointer.
    let Some(unit) = (unsafe { unit.as_ref() }) else {
        return sys::DR_OK;
    };
    let mut data = Vec::with_capacity(usize::try_from(unit.fullLength).unwrap_or(0));
    let mut entry = unit.bufferList;
    while !entry.is_null() {
        // SAFETY: the chain is well-formed until a null `next`, per Limelight.h; each
        // entry's `data` is non-null with a positive `length`.
        let e = unsafe { &*entry };
        if e.length > 0 && !e.data.is_null() {
            let Ok(len) = usize::try_from(e.length) else {
                continue;
            };
            // SAFETY: `data`/`length` describe one valid readable region.
            data.extend_from_slice(unsafe { std::slice::from_raw_parts(e.data.cast::<u8>(), len) });
        }
        entry = e.next;
    }

    let keyframe = unit.frameType == sys::FRAME_TYPE_IDR;
    let pts = Duration::from_micros(unit.presentationTimeUs);
    let codec = with_sinks(|s| s.codec.lock().map(|c| *c).unwrap_or(VideoCodec::H264))
        .unwrap_or(VideoCodec::H264);
    let frame = EncodedFrame {
        video_codec: Some(codec),
        audio_codec: None,
        pts,
        keyframe,
        // The library already emits Annex-B with start codes, with SPS/PPS/VPS as the
        // leading buffers of an IDR — which is exactly what the pipeline's decoder
        // wants, so nothing is rewritten here.
        data: bytes::Bytes::from(data),
    };

    let sent = with_sinks(|sinks| sinks.video.try_send(frame));
    match sent {
        // Dropping is the correct answer for a live mirror: the renderer is behind and
        // the next frame is worth more than this one (ground rule 4). The decoder is
        // not asked for an IDR, because the *stream* is fine — we are.
        Some(Err(mpsc::error::TrySendError::Full(_))) => {
            debug!("dropped a GameStream video frame; renderer is behind");
            sys::DR_OK
        }
        Some(Err(mpsc::error::TrySendError::Closed(_))) | None => sys::DR_OK,
        Some(Ok(())) => sys::DR_OK,
    }
}

extern "C" fn audio_init(
    _audio_configuration: c_int,
    opus_config: sys::POPUS_MULTISTREAM_CONFIGURATION,
    _context: *mut c_void,
    _flags: c_int,
) -> c_int {
    // SAFETY: valid for the duration of the call; we copy the two fields we need.
    let Some(cfg) = (unsafe { opus_config.as_ref() }) else {
        return 0;
    };
    let rate = u32::try_from(cfg.sampleRate).unwrap_or(48_000);
    let channels = u16::try_from(cfg.channelCount).unwrap_or(2);
    info!(rate, channels, "GameStream audio negotiated");
    with_sinks(|sinks| {
        if let Ok(mut slot) = sinks.audio_format.lock() {
            *slot = AudioFormat::from_hz(rate, channels);
        }
        if let Ok(mut pts) = sinks.audio_pts.lock() {
            *pts = Duration::ZERO;
        }
    });
    0
}

extern "C" fn audio_sample(data: *mut c_char, length: c_int) {
    if data.is_null() || length <= 0 {
        return;
    }
    // SAFETY: one valid readable region for the duration of the call; copied out.
    let Ok(len) = usize::try_from(length) else {
        return;
    };
    let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) }.to_vec();

    with_sinks(|sinks| {
        // Opus packets arrive at a fixed cadence with no timestamp of their own, so
        // the presentation clock is counted here. 5 ms is GameStream's frame duration;
        // a host negotiating something else moves the sample rate, not the cadence.
        let pts = sinks.audio_pts.lock().map_or(Duration::ZERO, |mut p| {
            let now = *p;
            *p += Duration::from_micros(5_000);
            now
        });
        let frame = EncodedFrame {
            video_codec: None,
            audio_codec: Some(AudioCodec::Opus),
            pts,
            keyframe: false,
            data: bytes::Bytes::from(bytes),
        };
        // Blocking is right here: these are not tokio threads, and audio that arrives
        // late is worse than audio that arrives slowly.
        let _ = sinks.audio.blocking_send(frame);
    });
}

extern "C" fn stage_starting(stage: c_int) {
    LAST_STAGE.store(stage, Ordering::SeqCst);
    // SAFETY: LiGetStageName returns a static string for any int.
    let name = unsafe { CStr::from_ptr(sys::LiGetStageName(stage)) };
    debug!(stage = %name.to_string_lossy(), "GameStream stage starting");
}

extern "C" fn stage_failed(stage: c_int, error_code: c_int) {
    LAST_STAGE.store(stage, Ordering::SeqCst);
    LAST_STAGE_ERROR.store(error_code, Ordering::SeqCst);
}

extern "C" fn connection_started() {
    info!("GameStream session established");
}

extern "C" fn connection_terminated(error_code: c_int) {
    if error_code == sys::ML_ERROR_GRACEFUL_TERMINATION {
        info!("GameStream session ended by the host");
        TERMINATED.store(true, Ordering::SeqCst);
    } else {
        warn!(error_code, "GameStream session terminated unexpectedly");
    }
    // Dropping the sinks closes both channels, which is how the pipeline learns the
    // stream ended.
    clear_session();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_audio_configuration_matches_the_c_macro() {
        // MAKE_AUDIO_CONFIGURATION(2, 0x3) — bindgen cannot translate the macro, so
        // the constant is hand-derived and pinned here. A wrong value negotiates a
        // channel layout the host will not send.
        assert_eq!(audio_configuration_stereo(), 0x0003_02CA);
        // And the surroundAudioInfo /launch must carry for it (mask << 16 | count).
        assert_eq!((0x3 << 16) | 2, 196_610);
    }
}
