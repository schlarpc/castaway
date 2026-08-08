//! libav (ffmpeg) decode → RGBA [`DecodedFrame`]s, converted with swscale so frames can
//! be uploaded straight into the [`crate::wgpu_compositor::WgpuCompositor`].
//!
//! Two entry points, for the two shapes media arrives in:
//! - [`decode`] opens a URL/file and demuxes it itself — the `Play(url)` path.
//! - [`decode_stream`] has no container at all: a mirroring adapter has already
//!   depacketized and decrypted, so it hands over frames and names the codec.
//!
//! Decode is blocking and CPU/thread-affine, so both run on a dedicated thread or
//! `spawn_blocking`, never on the tokio runtime (ground rule 4). Frames are pushed:
//! `on_frame` is called per frame and returning `false` stops early (teardown /
//! drop-late). Nothing here touches tokio — [`decode_stream`] *pulls* its input through a
//! caller-supplied closure precisely so the choice of channel stays with the caller.
//!
//! ## Hardware decode lives on the same seam
//!
//! With the `hwaccel` feature and a [`HwPreference`] that allows it, the decoder is
//! pointed at VA-API (Linux) or D3D11VA (Windows) and frames come out as
//! [`castaway_core::FrameImage::Gpu`] surfaces the compositor imports without a copy.
//! Everything about that is a *runtime* decision: the same binary decodes in software on
//! a box with no GPU decode, and can drop back to software **mid-session** — which is the
//! normal case, not an edge case, because a driver refuses a profile or a bit depth long
//! after the session started. Every downgrade is logged once, because the symptom of a
//! silent one is only a warm room.
//!
//! Pointing libavcodec at a hardware decoder means writing `AVCodecContext` fields the
//! wrapper crate does not expose, so this module reaches into raw pointers (ground rule 8
//! permits that in `pipeline`); each block carries the invariant it relies on, and the
//! FFI itself lives next door in [`crate::hwaccel::ffmpeg_hw`].
#![allow(unsafe_code)]

use std::sync::Once;
use std::time::Duration;

use castaway_core::{DecodedFrame, EncodedFrame, PixelFormat, VideoCodec};
use ffmpeg_next as ffmpeg;
use tracing::warn;

use crate::error::PipelineError;
use crate::hwaccel::{FallbackPolicy, HwPreference};

/// swscale quality/speed tradeoff, shared by both entry points.
const SCALE_FLAGS: ffmpeg::software::scaling::flag::Flags =
    ffmpeg::software::scaling::flag::Flags::BILINEAR;

/// The timebase [`decode_stream`] hands the decoder. [`EncodedFrame::pts`] is already a
/// [`Duration`], so microseconds are just a unit conversion — and letting the decoder
/// carry the timestamp through its reorder buffer means B-frames come back out labelled
/// correctly instead of us guessing which input a decoded frame came from.
const STREAM_TIMEBASE: ffmpeg::Rational = ffmpeg::Rational(1, 1_000_000);

static INIT: Once = Once::new();

fn ensure_init() {
    INIT.call_once(|| {
        // Errors here are effectively unreachable; log via ffmpeg's own registration.
        let _ = ffmpeg::init();
    });
}

fn map_err(e: ffmpeg::Error) -> PipelineError {
    PipelineError::Decode(e.to_string())
}

/// Decode the video stream at `uri`, invoking `on_frame` for each RGBA frame. Returns
/// when the stream ends or `on_frame` returns `false`.
///
/// # Errors
/// [`PipelineError::Decode`] on open/decode failure.
pub fn decode<F>(uri: &str, preference: HwPreference, mut on_frame: F) -> Result<(), PipelineError>
where
    F: FnMut(DecodedFrame) -> bool,
{
    ensure_init();
    let mut hw = HwAttempt::new(preference);

    // Same restart structure as the mirror path: a hardware give-up rebuilds the decoder
    // rather than ending playback. Demuxing starts over, which for a file is a seek back
    // to the beginning of the stream — acceptable for the rare mid-file fallback, and the
    // alternative is a black screen.
    loop {
        match url_session(uri, &mut hw, &mut on_frame)? {
            SessionEnd::Finished => return Ok(()),
            SessionEnd::RebuildInSoftware => {
                warn!(uri, "decode: restarting playback in software mid-stream");
            }
        }
    }
}

/// What streams a media URL turned out to have, and what the container says it is.
///
/// Reported before playback starts because the answer changes what the panel shows: a
/// file with no video stream is *music*, and the receiver should put a now-playing card
/// up rather than fail with "no video stream" — which is what it used to do, while
/// advertising `http-get:*:audio/*:*` to every control point on the LAN.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaLayout {
    /// Whether a video stream was found.
    pub has_video: bool,
    /// Whether an audio stream was found.
    pub has_audio: bool,
    /// Total duration, when the container knows it.
    pub duration: Option<Duration>,
    /// `title` from the container tags, if any.
    pub title: Option<String>,
    /// `artist`.
    pub artist: Option<String>,
    /// `album`.
    pub album: Option<String>,
}

/// How many decoded audio blocks may be queued for the output.
///
/// This bound is load-bearing, not a tuning knob: the audio thread consumes in real time,
/// so a full queue blocks the demuxer, and *that* is what paces the whole session. Without
/// it a two-hour film decodes as fast as the disk can feed it. Roughly a second of audio
/// at typical block sizes — enough to absorb a scheduling hiccup, short enough that a
/// preempted session goes quiet promptly.
pub const AUDIO_QUEUE: usize = 64;

/// How many video packets may be held back waiting for their turn to be decoded.
///
/// This queue exists to break a deadlock, not to buffer for its own sake — see
/// [`VIDEO_DECODE_LEAD`] for the deadlock. It holds *packets*, which is why it can be
/// generous: a second of 1080p60 VP8 is a few hundred kilobytes, where a second of the
/// same content decoded is half a gigabyte.
///
/// Four seconds at 60 fps. It should never fill on a well-interleaved file — the audio
/// queue reaches its bound first and blocks the demuxer, which is the throttle the whole
/// session is meant to run on — so a full one means video is running far ahead of audio
/// in the container, and holding it is better than either dropping it or stalling on it.
const VIDEO_PACKET_QUEUE: usize = 240;

/// How far ahead of the clock a video packet is decoded.
///
/// The number that stops the demuxer deadlocking against itself. Video used to be decoded
/// the instant its packet was read and then *held* — inside the demux loop — until the
/// clock said it was due. But this thread is the only producer of the audio that drives
/// that clock, and the clock deliberately reads `submitted - OUTPUT_LEAD`: to present a
/// frame at `T` the demuxer must first have read audio to `T + 250ms`, which is precisely
/// what it cannot do while it is asleep holding the frame at `T`.
///
/// Measured on a VP8 1080p60 + Vorbis WebM before the fix (#52): `drain_paced` slept
/// 235–251 ms per video packet, the clock advanced 17 ms per 250 ms of wall — 0.11× — and
/// the picture came out at 5.5 fps instead of 60. The audio queue never filled once, so
/// the documented throttle never got to act. It was not a slow clock; it was two halves of
/// one thread waiting for each other.
///
/// So a video packet now waits *as a packet*, which costs nothing to hold, and the loop
/// goes back to reading — which is what lets audio through, which is what advances the
/// clock, which is what makes the packet due. The lead is one frame interval's worth of
/// slack so the decode is finished by the time the frame is wanted; the fine pacing is
/// still `drain_paced`'s, and its wait is now bounded by this rather than by the output
/// lead.
const VIDEO_DECODE_LEAD: Duration = Duration::from_millis(40);

/// The longest single sleep anywhere on the pacing path.
///
/// Sleeps are sliced so neither a preemption nor a seek is stuck behind a long hold: a
/// *paused* session's clock never advances, so a frame waiting for its turn waits
/// forever, and without slicing that leaks the thread and its decoder — one per paused
/// session, in silence.
const PACING_SLICE: Duration = Duration::from_millis(50);

/// The longest a held packet waits for its turn once the container is exhausted.
///
/// An item whose video outlasts its audio ends with a clock that has stopped, because its
/// master has run out. The last few frames should still be presented rather than held
/// against a turn that will never come.
const EOF_HOLD: Duration = Duration::from_secs(2);

/// Whether `packet` is close enough to the clock to be worth decoding now.
///
/// `true` when there is no timestamp to judge by and when the clock has not started: the
/// first packet of an item must go in, or a file with no audio never seeds the video
/// master clock and nothing is ever due.
fn packet_due(
    packet: &ffmpeg::codec::packet::Packet,
    time_base: ffmpeg::Rational,
    clock: &crate::clock::MediaClock,
) -> bool {
    // Presentation time, not decode time. Packets are *sent* in container order either
    // way — this only decides when — and gating on the time the picture is wanted is what
    // keeps a reordered stream's B-frames from arriving before their turn.
    let Some(ts) = packet.pts().or_else(|| packet.dts()) else {
        return true;
    };
    let at = rescale_to_duration(ts, time_base);
    clock
        .wait_for(at.saturating_sub(VIDEO_DECODE_LEAD))
        .is_none()
}

/// Decode `uri`, presenting video against `clock` and sending decoded audio to `audio`.
///
/// The pacing story in one place, because it is the thing that was missing entirely:
///
/// - Audio blocks go into a **bounded** channel that a real-time consumer drains. When it
///   fills, this function blocks, and with it the demuxer and the video decoder. Audio is
///   therefore the throttle for the whole session, which is what "audio master" means
///   before it means anything about lip sync.
/// - Video frames are then held individually until [`MediaClock`] says they are due, so
///   they land against the audio rather than merely at the same average rate.
/// - With no audio stream there is nothing to be paced by, so the clock is seeded from
///   the first frame and runs off the wall instead.
///
/// `on_open` is called once, before any frame, with what the container turned out to hold.
///
/// # Errors
/// [`PipelineError::Decode`] on open/decode failure. A file with neither audio nor video
/// is an error; a file with only one of them is not.
#[allow(clippy::too_many_arguments)]
pub fn decode_av<F, O>(
    uri: &str,
    preference: HwPreference,
    clock: &crate::clock::MediaClock,
    seek: Option<&crate::seek::SeekControl>,
    audio: Option<std::sync::mpsc::SyncSender<castaway_core::PcmFrame>>,
    stop: &dyn Fn() -> bool,
    mut on_open: O,
    mut on_frame: F,
) -> Result<(), PipelineError>
where
    F: FnMut(DecodedFrame) -> bool,
    O: FnMut(&MediaLayout),
{
    ensure_init();
    let mut hw = HwAttempt::new(preference);
    let mut opened = false;
    loop {
        match av_session(
            uri,
            &mut hw,
            clock,
            seek,
            audio.as_ref(),
            stop,
            &mut |layout: &MediaLayout| {
                // Only the first time round: a mid-session software fallback reopens the
                // file, and telling the pipeline "here is a new item" would restart the
                // card and the metadata for something that never stopped playing.
                if !opened {
                    opened = true;
                    on_open(layout);
                }
            },
            &mut on_frame,
        )? {
            SessionEnd::Finished => return Ok(()),
            SessionEnd::RebuildInSoftware => {
                warn!(uri, "decode: restarting playback in software mid-stream");
            }
        }
    }
}

/// One decoder incarnation over a demuxed URL, video and audio together.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn av_session<F, O>(
    uri: &str,
    hw: &mut HwAttempt,
    clock: &crate::clock::MediaClock,
    seek: Option<&crate::seek::SeekControl>,
    audio_tx: Option<&std::sync::mpsc::SyncSender<castaway_core::PcmFrame>>,
    stop: &dyn Fn() -> bool,
    on_open: &mut O,
    on_frame: &mut F,
) -> Result<SessionEnd, PipelineError>
where
    F: FnMut(DecodedFrame) -> bool,
    O: FnMut(&MediaLayout),
{
    let mut ictx = open_input(uri)?;

    let video = ictx
        .streams()
        .best(ffmpeg::media::Type::Video)
        .map(|s| (s.index(), s.time_base(), s.parameters()));
    // Only claimed when this build can actually decode it: `has_audio` drives whether the
    // panel shows a music card, and a build with no decoder saying "yes, audio" would put
    // a card up over silence.
    #[cfg(feature = "audio")]
    let audio = audio_tx.and(
        ictx.streams()
            .best(ffmpeg::media::Type::Audio)
            .map(|s| (s.index(), s.time_base(), s.parameters())),
    );
    #[cfg(not(feature = "audio"))]
    let audio: Option<(usize, ffmpeg::Rational, ffmpeg::codec::Parameters)> = {
        let _ = &audio_tx;
        None
    };

    let layout = MediaLayout {
        has_video: video.is_some(),
        has_audio: audio.is_some(),
        duration: container_duration(&ictx),
        title: tag(&ictx, "title"),
        artist: tag(&ictx, "artist"),
        album: tag(&ictx, "album"),
    };
    if !layout.has_video && !layout.has_audio {
        return Err(PipelineError::Decode(
            "the media has neither a video nor an audio stream".into(),
        ));
    }
    on_open(&layout);

    let mut video_decoder = match &video {
        Some((_, _, parameters)) => {
            let mut ctx = ffmpeg::codec::context::Context::from_parameters(parameters.clone())
                .map_err(map_err)?;
            let codec_id = ctx.id();
            if hw.wants_hardware() {
                // SAFETY: the context has been filled from stream parameters but not
                // opened — `.decoder().video()` below is what opens it — and it will be
                // opened with the decoder for `codec_id`.
                unsafe { hw.attach(ctx.as_mut_ptr().cast(), codec_id) }?;
            }
            Some(ctx.decoder().video().map_err(map_err)?)
        }
        None => None,
    };
    #[cfg(feature = "audio")]
    let mut audio_decoder = match &audio {
        Some((_, _, parameters)) => {
            let ctx = ffmpeg::codec::context::Context::from_parameters(parameters.clone())
                .map_err(map_err)?;
            Some(ctx.decoder().audio().map_err(map_err)?)
        }
        None => None,
    };
    let mut scaler: Option<ffmpeg::software::scaling::Context> = None;
    // Video packets read but not yet due. See `VIDEO_DECODE_LEAD` — this is the queue that
    // keeps the demuxer reading (and so keeps audio flowing, and so keeps the clock
    // moving) while a picture waits for its turn.
    let mut held: std::collections::VecDeque<ffmpeg::codec::packet::Packet> =
        std::collections::VecDeque::new();

    // A manual loop rather than `for … in ictx.packets()`, because a seek needs `&mut
    // ictx` and the iterator holds it for the length of the loop. Taking the iterator one
    // packet at a time costs nothing — it is a wrapper around `av_read_frame` — and is
    // what makes moving the demuxer expressible at all.
    loop {
        if stop() {
            return Ok(SessionEnd::Finished);
        }
        if let Some(control) = seek {
            if let Some(target) = control.take() {
                // Everything held is from before the seek. Keeping it would decode the old
                // position's pictures after the demuxer had already moved — the video half
                // of the stale-audio problem `do_seek` step 3 is about.
                held.clear();
                do_seek(
                    &mut ictx,
                    video_decoder.as_mut(),
                    #[cfg(feature = "audio")]
                    audio_decoder.as_mut(),
                    clock,
                    control,
                    // Nothing to throw away, and nobody to answer: waiting out the grace
                    // period on every seek of a silent file would be a quarter-second of
                    // dead scrubber bought for nothing.
                    layout.has_audio,
                    target,
                );
            }
        }

        // Anything held whose turn has come, before reading more. A packet is due when
        // the clock has reached its decode timestamp less `VIDEO_DECODE_LEAD`; decode
        // order rather than presentation order, because that is the order a decoder has
        // to be fed and it is never later than the presentation time.
        if let (Some((_, time_base, _)), Some(decoder)) = (&video, video_decoder.as_mut()) {
            while held
                .front()
                .is_some_and(|p| packet_due(p, *time_base, clock))
            {
                let Some(packet) = held.pop_front() else {
                    break;
                };
                decoder.send_packet(&packet).map_err(map_err)?;
                hw.check_negotiation()?;
                match drain_paced(
                    decoder,
                    hw,
                    &mut scaler,
                    *time_base,
                    clock,
                    seek,
                    stop,
                    on_frame,
                )? {
                    Drained::Continue => {}
                    Drained::Stopped => return Ok(SessionEnd::Finished),
                    Drained::Restart => return Ok(SessionEnd::RebuildInSoftware),
                }
            }
        }
        // Nothing due and nowhere to put another. Only reachable when video runs far ahead
        // of audio in the container — normally the audio queue's bound is what stops the
        // demuxer, and it is the throttle this session is meant to run on.
        if held.len() >= VIDEO_PACKET_QUEUE {
            std::thread::sleep(PACING_SLICE);
            continue;
        }

        let Some((stream, packet)) = ictx.packets().next() else {
            break;
        };
        let index = stream.index();

        if let Some((vi, _, _)) = &video {
            if index == *vi {
                // Held rather than decoded-and-held: a packet costs nothing to keep, and
                // going back round the loop is what lets the audio through that this
                // packet is waiting on.
                held.push_back(packet);
                continue;
            }
        }

        #[cfg(feature = "audio")]
        if let (Some((ai, time_base, _)), Some(decoder), Some(tx)) =
            (&audio, audio_decoder.as_mut(), audio_tx)
        {
            if index == *ai {
                decoder.send_packet(&packet).map_err(map_err)?;
                let mut decoded = ffmpeg::frame::Audio::empty();
                while decoder.receive_frame(&mut decoded).is_ok() {
                    let mut block = crate::audio_decode::pcm_from_frame(&decoded)?;
                    // The decoder's timestamps are in the stream's time base; the clock
                    // and the card both speak `Duration`.
                    block.pts = decoded
                        .pts()
                        .map(|p| rescale_to_duration(p, *time_base))
                        .unwrap_or(block.pts);
                    // Blocking, deliberately: this is the throttle for the whole session
                    // (see `AUDIO_QUEUE`). A `try_send` here would drop audio to keep
                    // reading, which is the one thing audio must never do.
                    if tx.send(block).is_err() {
                        // The output went away — a preemption, or the device failed.
                        return Ok(SessionEnd::Finished);
                    }
                }
            }
        }
    }

    // Flush the video decoder so the last frames of the item are not left inside it —
    // and everything still held, which is the tail of the item and is now the only place
    // it exists.
    if let (Some(decoder), Some((_, time_base, _))) = (video_decoder.as_mut(), &video) {
        while let Some(packet) = held.pop_front() {
            // Still gated, and this is load-bearing rather than symmetry: for a file with
            // no audio the gate *is* the pacing, and a tail drained as fast as it decodes
            // plays the end of every silent item at whatever speed the CPU manages.
            //
            // Waiting here cannot deadlock the way waiting in the packet loop did: there
            // is nothing left to read, so nothing is being starved. It is bounded anyway,
            // because a clock whose master has run out — an item whose video outlasts its
            // audio — freezes, and the last frames should still be shown.
            let until = std::time::Instant::now() + EOF_HOLD;
            while !packet_due(&packet, *time_base, clock) {
                if stop() {
                    return Ok(SessionEnd::Finished);
                }
                if seek.is_some_and(crate::seek::SeekControl::pending)
                    || std::time::Instant::now() >= until
                {
                    break;
                }
                std::thread::sleep(PACING_SLICE);
            }
            if stop() {
                return Ok(SessionEnd::Finished);
            }
            decoder.send_packet(&packet).map_err(map_err)?;
            hw.check_negotiation()?;
            match drain_paced(
                decoder,
                hw,
                &mut scaler,
                *time_base,
                clock,
                seek,
                stop,
                on_frame,
            )? {
                Drained::Continue => {}
                Drained::Stopped => return Ok(SessionEnd::Finished),
                Drained::Restart => return Ok(SessionEnd::RebuildInSoftware),
            }
        }
        decoder.send_eof().map_err(map_err)?;
        if let Drained::Restart = drain_paced(
            decoder,
            hw,
            &mut scaler,
            *time_base,
            clock,
            seek,
            stop,
            on_frame,
        )? {
            return Ok(SessionEnd::RebuildInSoftware);
        }
    }
    Ok(SessionEnd::Finished)
}

/// Move the demuxer to `target` and put everything downstream back in step.
///
/// The order is the whole substance of it:
///
/// 1. **The demuxer moves**, to the last key frame at or before the target. Not past it:
///    a decoder fed inter frames whose reference pictures it never saw produces garbage,
///    and overshooting a seek is also the wrong answer to what was asked.
/// 2. **Both decoders are flushed**, or the pictures and blocks still inside them come out
///    labelled with the old position and are either dropped as hopelessly late or held for
///    a turn that has already passed.
/// 3. **The audio already queued is dropped**, by the thread that owns it. This is the
///    step whose absence is inaudible in a test and unmistakable in a room: the queue
///    holds around a second of decoded sound from where playback used to be.
/// 4. **The clock is re-anchored**, last, once nothing stale is left to move it back.
///
/// A seek that libavformat refuses is logged and otherwise ignored: playback carries on
/// from where it was, which is a control point's scrubber springing back — visibly wrong,
/// but far better than a torn-down session for a file that simply is not seekable.
fn do_seek(
    ictx: &mut ffmpeg::format::context::Input,
    video_decoder: Option<&mut ffmpeg::decoder::Video>,
    #[cfg(feature = "audio")] audio_decoder: Option<&mut ffmpeg::decoder::Audio>,
    clock: &crate::clock::MediaClock,
    control: &crate::seek::SeekControl,
    has_audio: bool,
    target: Duration,
) {
    let Ok(ts) = i64::try_from(target.as_micros()) else {
        warn!(?target, "seek: target is further away than time itself");
        return;
    };
    // `..ts`, so the demuxer lands at or before the target rather than after it. With no
    // stream index the timestamp is in `AV_TIME_BASE` units, which is microseconds.
    if let Err(e) = ictx.seek(ts, ..ts) {
        warn!(?target, error = %e, "seek: the demuxer would not move");
        return;
    }
    if let Some(decoder) = video_decoder {
        decoder.flush();
    }
    #[cfg(feature = "audio")]
    if let Some(decoder) = audio_decoder {
        decoder.flush();
    }
    if has_audio {
        control.flush_audio();
    }
    clock.seek_to(target);
}

/// How long a network read may stall before the fetch is abandoned, in microseconds.
///
/// Not a tuning knob — the absence of one was a thread leak. `avformat_open_input` with no
/// options blocks forever against a black-holed server, in a region where the stop flag is
/// never looked at, so every cast at an unreachable host left a decode thread parked for
/// the life of the process. Thirty seconds is long enough that a slow first byte from a
/// loaded NAS is not mistaken for a dead one, and short enough that a wedged fetch is over
/// before anybody has finished asking why nothing happened.
const NETWORK_TIMEOUT_US: &str = "30000000";

/// How long libavformat may spend re-establishing a dropped connection.
///
/// Reconnection is on because the failure it prevents is the one this project keeps
/// finding: a Wi-Fi blip forty minutes into a film ends playback with a log line, and the
/// panel goes back to idle with nobody able to say why.
const RECONNECT_DELAY_MAX_S: &str = "5";

/// What castaway calls itself to an HTTP server.
///
/// Moved to `castaway-core` so the DLNA HEAD probe sends the same string (#99): a server
/// that varies what it serves by client — which is the whole reason to send this — has to
/// be asked about the resource and then handed the resource as one client.
use castaway_core::MEDIA_USER_AGENT as USER_AGENT;

/// Open `uri` for demuxing, with the options a network fetch needs.
///
/// Applied only to network URIs. A local file has no socket to time out and no headers to
/// carry, and handing libavformat options its protocol does not recognise earns a warning
/// per open for nothing.
fn open_input(uri: &str) -> Result<ffmpeg::format::context::Input, PipelineError> {
    if !is_network_uri(uri) {
        return ffmpeg::format::input(&uri).map_err(map_err);
    }
    let mut options = ffmpeg::Dictionary::new();
    // Applies to the socket underneath whatever protocol this is, so it covers the
    // connect *and* a stall mid-stream.
    options.set("rw_timeout", NETWORK_TIMEOUT_US);
    // The HTTP/TCP protocols' own name for the same idea; they read this one, not
    // `rw_timeout`, on some builds.
    options.set("timeout", NETWORK_TIMEOUT_US);
    options.set("user_agent", USER_AGENT);
    options.set("reconnect", "1");
    options.set("reconnect_streamed", "1");
    options.set("reconnect_delay_max", RECONNECT_DELAY_MAX_S);
    // The two headers that make this a DLNA client rather than an anonymous GET.
    //
    // `getcontentFeatures.dlna.org` asks the server to describe what it is about to send;
    // `transferMode.dlna.org: Streaming` is the mode for audio and video, as against
    // `Interactive` for images and `Background` for a bulk copy. Neither is mandatory, and
    // servers that key their behaviour off them are common enough that omitting them means
    // being served a different thing than every other renderer on the LAN gets.
    options.set(
        "headers",
        "getcontentFeatures.dlna.org: 1\r\ntransferMode.dlna.org: Streaming\r\n",
    );
    ffmpeg::format::input_with_dictionary(&uri, options).map_err(map_err)
}

/// Whether this URI is fetched over a network, rather than opened from the filesystem.
///
/// Deliberately a scheme test rather than "not a file": libavformat speaks a long list of
/// protocols, and the ones worth timing out are the ones with a peer at the other end.
fn is_network_uri(uri: &str) -> bool {
    let Some((scheme, _)) = uri.split_once("://") else {
        return false;
    };
    matches!(
        scheme.to_ascii_lowercase().as_str(),
        "http" | "https" | "rtsp" | "rtmp" | "rtmps" | "rtp" | "udp" | "tcp" | "mms" | "mmsh"
    )
}

/// The container's duration, when it has one. A live stream reports nothing usable, which
/// is exactly the case the scrubber must not draw a bar for.
fn container_duration(ictx: &ffmpeg::format::context::Input) -> Option<Duration> {
    let raw = ictx.duration();
    if raw <= 0 {
        return None;
    }
    #[allow(clippy::cast_sign_loss)]
    Some(Duration::from_micros(
        (raw as u64).saturating_mul(1_000_000) / u64::try_from(ffmpeg::ffi::AV_TIME_BASE).ok()?,
    ))
}

fn tag(ictx: &ffmpeg::format::context::Input, key: &str) -> Option<String> {
    ictx.metadata()
        .get(key)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
}

/// A stream timestamp in its own time base, as a [`Duration`].
///
/// This was once `#[cfg(feature = "audio")]`, on the reasoning that only the audio half
/// needs it. It does not: the video pacing path calls it too, so an `ffmpeg`-without-`audio`
/// build did not compile at all. No check built that combination, so it went unnoticed
/// (#180) — the cfg is gone rather than widened, because four lines of arithmetic are not
/// worth a feature gate.
fn rescale_to_duration(pts: i64, time_base: ffmpeg::Rational) -> Duration {
    let seconds = f64::from(time_base) * pts as f64;
    Duration::from_secs_f64(seconds.max(0.0))
}

/// One decoder incarnation over a demuxed URL.
fn url_session<F>(
    uri: &str,
    hw: &mut HwAttempt,
    on_frame: &mut F,
) -> Result<SessionEnd, PipelineError>
where
    F: FnMut(DecodedFrame) -> bool,
{
    let mut ictx = open_input(uri)?;
    let input = ictx
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or(PipelineError::Decode("no video stream".into()))?;
    let stream_index = input.index();
    let time_base = input.time_base();

    let mut decoder_ctx =
        ffmpeg::codec::context::Context::from_parameters(input.parameters()).map_err(map_err)?;
    let codec_id = decoder_ctx.id();
    if hw.wants_hardware() {
        // SAFETY: the context has been filled from stream parameters but not opened —
        // `.decoder().video()` below is what opens it — and it will be opened with the
        // decoder for `codec_id`.
        unsafe { hw.attach(decoder_ctx.as_mut_ptr().cast(), codec_id) }?;
    }
    let mut decoder = decoder_ctx.decoder().video().map_err(map_err)?;
    let mut scaler: Option<ffmpeg::software::scaling::Context> = None;

    for (stream, packet) in ictx.packets() {
        if stream.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet).map_err(map_err)?;
        hw.check_negotiation()?;
        match drain(&mut decoder, hw, &mut scaler, time_base, on_frame)? {
            Drained::Continue => {}
            Drained::Stopped => return Ok(SessionEnd::Finished),
            Drained::Restart => return Ok(SessionEnd::RebuildInSoftware),
        }
    }
    decoder.send_eof().map_err(map_err)?;
    drain(&mut decoder, hw, &mut scaler, time_base, on_frame)?;
    Ok(SessionEnd::Finished)
}

/// What happened when a decoded frame was offered to the hardware export path.
///
/// Without the `hwaccel` feature only `Software` is ever produced; the others stay in the
/// type so the decode loop below reads the same either way, rather than growing a `cfg`
/// around every match arm.
#[cfg_attr(not(feature = "hwaccel"), allow(dead_code))]
enum Export {
    /// A GPU surface, ready for the compositor to import.
    Gpu(DecodedFrame),
    /// Not a hardware frame — the software conversion applies.
    Software,
    /// Export failed within budget; drop this frame and keep going.
    Dropped,
    /// Hardware has been abandoned; rebuild the decoder in software.
    Restart,
}

/// The hardware-decode attempt for one decode session: the policy, plus whatever is
/// currently attached to the decoder.
///
/// Always compiled. Without the `hwaccel` feature it degrades to "the policy said
/// software", which keeps the decode loops below free of `cfg`.
struct HwAttempt {
    policy: FallbackPolicy,
    #[cfg(feature = "hwaccel")]
    live: Option<HwLive>,
}

#[cfg(feature = "hwaccel")]
struct HwLive {
    /// The pixel format a frame carries when it really is a GPU surface. Frames that come
    /// out in anything else are software frames from a decoder that declined.
    format: ffmpeg::format::Pixel,
    exporter: crate::hwaccel::SurfaceExporter,
    /// Owns the device context for as long as the decoder is open.
    _setup: crate::hwaccel::ffmpeg_hw::HwSetup,
}

impl HwAttempt {
    /// Decide whether to try hardware at all.
    ///
    /// The compositor's import capability is part of the decision, not an afterthought:
    /// decoding to GPU surfaces that nothing can sample is strictly worse than decoding
    /// on the CPU, and the render thread has no way to tell the decoder it is dropping
    /// everything it sends.
    fn new(preference: HwPreference) -> Self {
        // `mut` is needed only when the hwaccel backends are compiled in — the give-up
        // path below is behind that feature — so a build without them would otherwise
        // warn about a mutability it cannot see the use of.
        #[cfg_attr(not(feature = "hwaccel"), allow(unused_mut))]
        let mut policy = FallbackPolicy::new(preference);
        #[cfg(feature = "render")]
        if policy.wants_hardware() {
            use crate::hwaccel::{import_capability, SurfaceImport};
            if import_capability() == SurfaceImport::Unsupported {
                policy.give_up(crate::hwaccel::HwGiveUp::DeviceUnavailable(
                    "the compositor's device cannot import GPU surfaces".into(),
                ));
            }
        }
        Self {
            policy,
            #[cfg(feature = "hwaccel")]
            live: None,
        }
    }

    const fn wants_hardware(&self) -> bool {
        self.policy.wants_hardware()
    }

    /// Attach a hardware decoder to an unopened context, if the policy still wants one.
    ///
    /// Never fails the session: an unavailable device or an unsupported codec becomes a
    /// logged fallback and a software decoder, unless the operator asked for
    /// [`HwPreference::HardwareOnly`].
    ///
    /// # Safety
    /// `ctx` must be an allocated, not-yet-opened `AVCodecContext` that will be opened
    /// with the decoder for `codec_id`.
    #[allow(unused_variables)]
    unsafe fn attach(
        &mut self,
        ctx: *mut std::ffi::c_void,
        codec_id: ffmpeg::codec::Id,
    ) -> Result<(), PipelineError> {
        #[cfg(feature = "hwaccel")]
        {
            use crate::hwaccel::ffmpeg_hw::{attach_for_id, HwDevice};
            use ffmpeg_sys_next as sys;

            self.live = None;
            if !self.policy.wants_hardware() {
                return Ok(());
            }
            let Some(kind) = crate::hwaccel::HwBackendKind::for_this_platform() else {
                return self.give_up(crate::hwaccel::HwGiveUp::NotCompiled);
            };
            let device = match HwDevice::open(kind) {
                Ok(device) => device,
                Err(reason) => return self.give_up(reason),
            };
            let exporter = match crate::hwaccel::SurfaceExporter::new(&device) {
                Ok(exporter) => exporter,
                Err(reason) => return self.give_up(reason),
            };
            // SAFETY: caller guarantees an unopened context; the id is converted by the
            // wrapper crate's own mapping, not reinterpreted.
            let setup = unsafe {
                attach_for_id(
                    ctx.cast::<sys::AVCodecContext>(),
                    sys::AVCodecID::from(codec_id),
                    device,
                )
            };
            match setup {
                Ok(setup) => {
                    self.live = Some(HwLive {
                        format: ffmpeg::format::Pixel::from(setup.hw_format),
                        exporter,
                        _setup: setup,
                    });
                    self.policy.confirm_hardware(kind);
                    Ok(())
                }
                Err(reason) => self.give_up(reason),
            }
        }
        #[cfg(not(feature = "hwaccel"))]
        {
            Ok(())
        }
    }

    /// Feed a give-up through the policy, turning a refusal into an error only when the
    /// operator asked for one.
    #[cfg(feature = "hwaccel")]
    fn give_up(&mut self, reason: crate::hwaccel::HwGiveUp) -> Result<(), PipelineError> {
        use crate::hwaccel::Reaction;
        match self.policy.give_up(reason) {
            Reaction::Fail(reason) => Err(PipelineError::HwDecode(reason.to_string())),
            Reaction::DropFrame | Reaction::FallBackToSoftware => {
                self.live = None;
                Ok(())
            }
        }
    }

    /// Notice a `get_format` refusal. libavcodec answers "not this stream" by simply not
    /// offering the hardware format, which is not an error anywhere — so it has to be
    /// asked about explicitly, or the session quietly runs in software forever.
    #[allow(clippy::unnecessary_wraps)]
    fn check_negotiation(&mut self) -> Result<(), PipelineError> {
        #[cfg(feature = "hwaccel")]
        {
            if self.live.is_some() && crate::hwaccel::ffmpeg_hw::take_format_rejected() {
                let kind = self
                    .live
                    .as_ref()
                    .and_then(|_| crate::hwaccel::HwBackendKind::for_this_platform());
                if let Some(kind) = kind {
                    return self.give_up(crate::hwaccel::HwGiveUp::FormatRejected(kind));
                }
            }
        }
        Ok(())
    }

    /// Offer a decoded frame to the export path.
    #[allow(unused_variables)]
    fn export(&mut self, decoded: &mut ffmpeg::frame::Video, pts: Duration) -> Export {
        #[cfg(feature = "hwaccel")]
        {
            let Some(live) = self.live.as_mut() else {
                return Export::Software;
            };
            if decoded.format() != live.format {
                // The decoder handed back a software frame despite being pointed at
                // hardware — `check_negotiation` will have said why.
                return Export::Software;
            }
            let (width, height) = (decoded.width(), decoded.height());
            // SAFETY: `decoded` is a live decoded frame in the hardware format, which is
            // exactly what the exporter requires.
            let exported = unsafe { live.exporter.export(decoded.as_mut_ptr()) };
            match exported {
                Ok(surface) => Export::Gpu(DecodedFrame::gpu(width, height, pts, surface)),
                Err(reason) => match self.policy.give_up(reason) {
                    crate::hwaccel::Reaction::DropFrame => Export::Dropped,
                    _ => {
                        self.live = None;
                        Export::Restart
                    }
                },
            }
        }
        #[cfg(not(feature = "hwaccel"))]
        {
            Export::Software
        }
    }
}

/// Which ffmpeg decoder a negotiated codec asks for.
///
/// Exhaustive on purpose: a codec added to `core` must fail to compile here rather
/// than silently render black (#213).
fn codec_id(codec: VideoCodec) -> ffmpeg::codec::Id {
    match codec {
        VideoCodec::H264 => ffmpeg::codec::Id::H264,
        VideoCodec::Hevc => ffmpeg::codec::Id::HEVC,
        VideoCodec::Vp8 => ffmpeg::codec::Id::VP8,
        VideoCodec::Vp9 => ffmpeg::codec::Id::VP9,
    }
}

/// Decode a live stream of already-depacketized [`EncodedFrame`]s (the mirroring path).
///
/// There is no container and no demuxer here: the protocol adapter negotiated `codec`
/// and owns the frame boundaries, so all that is left is to feed the decoder. `next` is
/// called for each frame and returns `None` when the source is finished; `on_frame` takes
/// each decoded RGBA frame and returns `false` to stop.
///
/// Pull-based on purpose — `next` blocks on whatever channel the caller owns, from a
/// thread the caller picked, which keeps this module free of tokio (ground rule 4).
///
/// Decoding starts at the first key frame: a mirror session is joined mid-stream far more
/// often than not, and feeding a decoder frames that reference pictures it never saw only
/// produces garbage.
///
/// # Errors
/// [`PipelineError::Decode`] if `codec` has no decoder in this ffmpeg build, or the
/// decoder cannot be opened. A frame the decoder rejects is logged and skipped rather
/// than fatal — one corrupt packet must not tear down a live mirror.
pub fn decode_stream<N, F>(
    codec: VideoCodec,
    preference: HwPreference,
    mut next: N,
    mut on_frame: F,
) -> Result<(), PipelineError>
where
    N: FnMut() -> Option<EncodedFrame>,
    F: FnMut(DecodedFrame) -> bool,
{
    ensure_init();

    let id = codec_id(codec);
    let mut hw = HwAttempt::new(preference);

    // One iteration per decoder incarnation. A mid-session fallback drops out of the
    // inner loop, rebuilds in software, and resumes from the next key frame — a brief gap
    // in the mirror rather than the end of it, which is the whole point.
    loop {
        let outcome = stream_session(id, codec, &mut hw, &mut next, &mut on_frame)?;
        match outcome {
            SessionEnd::Finished => return Ok(()),
            SessionEnd::RebuildInSoftware => {
                warn!("mirror decode: restarting the decoder in software mid-session");
            }
        }
    }
}

/// How one decoder incarnation ended.
enum SessionEnd {
    /// The source finished, or the consumer asked to stop.
    Finished,
    /// Hardware was abandoned; the caller should build a software decoder and continue.
    RebuildInSoftware,
}

/// Run one decoder incarnation over the stream until it ends or hardware is given up on.
fn stream_session<N, F>(
    id: ffmpeg::codec::Id,
    codec: VideoCodec,
    hw: &mut HwAttempt,
    next: &mut N,
    on_frame: &mut F,
) -> Result<SessionEnd, PipelineError>
where
    N: FnMut() -> Option<EncodedFrame>,
    F: FnMut(DecodedFrame) -> bool,
{
    let found = ffmpeg::decoder::find(id)
        .ok_or_else(|| PipelineError::Decode(format!("this ffmpeg build has no {id:?} decoder")))?;
    let mut context = ffmpeg::decoder::new();
    context.set_packet_time_base(STREAM_TIMEBASE);
    if hw.wants_hardware() {
        // SAFETY: `context` is allocated and not yet opened — `open_as` below is what
        // opens it — and it will be opened with the decoder for `id`.
        unsafe { hw.attach(context.as_mut_ptr().cast(), id) }?;
    }
    let mut decoder = context
        .open_as(found)
        .map_err(map_err)?
        .video()
        .map_err(map_err)?;

    // Built on the first decoded frame and rebuilt on resize: without stream parameters
    // the picture size is not known until one comes out.
    let mut scaler: Option<ffmpeg::software::scaling::Context> = None;
    let mut synced = false;

    while let Some(frame) = next() {
        if frame.video_codec != Some(codec) {
            warn!(
                got = ?frame.video_codec,
                want = ?codec,
                "mirror decode: skipping a frame in the wrong codec",
            );
            continue;
        }
        if !synced {
            if !frame.keyframe {
                continue;
            }
            synced = true;
        }

        let mut packet = ffmpeg::codec::packet::Packet::copy(&frame.data);
        packet.set_pts(i64::try_from(frame.pts.as_micros()).ok());
        if frame.keyframe {
            packet.set_flags(ffmpeg::codec::packet::Flags::KEY);
        }

        if let Err(e) = decoder.send_packet(&packet) {
            warn!(error = %e, "mirror decode: decoder rejected a frame, skipping it");
            continue;
        }
        // Asked once per packet rather than once per session: the refusal only becomes
        // visible after libavcodec has parsed enough of the stream to negotiate.
        hw.check_negotiation()?;
        match drain(&mut decoder, hw, &mut scaler, STREAM_TIMEBASE, on_frame)? {
            Drained::Continue => {}
            Drained::Stopped => return Ok(SessionEnd::Finished),
            Drained::Restart => return Ok(SessionEnd::RebuildInSoftware),
        }
    }

    decoder.send_eof().map_err(map_err)?;
    drain(&mut decoder, hw, &mut scaler, STREAM_TIMEBASE, on_frame)?;
    Ok(SessionEnd::Finished)
}

/// What a drain pass concluded.
enum Drained {
    /// Keep feeding this decoder.
    Continue,
    /// `on_frame` asked to stop.
    Stopped,
    /// Hardware was abandoned mid-drain; the decoder must be rebuilt.
    Restart,
}

/// Pull every frame the decoder is holding and hand each one over — as a GPU surface when
/// the decoder produced one, otherwise scaled to RGBA by swscale.
fn drain<F>(
    decoder: &mut ffmpeg::decoder::Video,
    hw: &mut HwAttempt,
    scaler: &mut Option<ffmpeg::software::scaling::Context>,
    time_base: ffmpeg::Rational,
    on_frame: &mut F,
) -> Result<Drained, PipelineError>
where
    F: FnMut(DecodedFrame) -> bool,
{
    let mut decoded = ffmpeg::frame::Video::empty();
    while decoder.receive_frame(&mut decoded).is_ok() {
        let pts = frame_pts(decoded.pts(), time_base);
        let frame = match hw.export(&mut decoded, pts) {
            Export::Gpu(frame) => frame,
            // A tolerated export failure: the mirror is better served by a dropped frame
            // than by a stall.
            Export::Dropped => continue,
            Export::Restart => return Ok(Drained::Restart),
            Export::Software => scale_to_rgba(&decoded, scaler, pts)?,
        };
        if !on_frame(frame) {
            return Ok(Drained::Stopped);
        }
    }
    Ok(Drained::Continue)
}

/// [`drain`], with each frame held until the media clock says it is due.
///
/// The waiting is the difference between "the frames came out in order" and "the picture
/// matches the sound". It happens here rather than in the render loop because this is the
/// thread that can afford to sleep: the render loop presents whatever it was last handed
/// and must never block on a decoder.
///
/// A frame that is merely late is still shown — it is the best picture available. One that
/// is hopelessly late is dropped, because presenting it only puts the next one further
/// behind, and a decoder losing that race never wins it back frame by frame.
#[allow(clippy::too_many_arguments)]
fn drain_paced<F>(
    decoder: &mut ffmpeg::decoder::Video,
    hw: &mut HwAttempt,
    scaler: &mut Option<ffmpeg::software::scaling::Context>,
    time_base: ffmpeg::Rational,
    clock: &crate::clock::MediaClock,
    seek: Option<&crate::seek::SeekControl>,
    stop: &dyn Fn() -> bool,
    on_frame: &mut F,
) -> Result<Drained, PipelineError>
where
    F: FnMut(DecodedFrame) -> bool,
{
    // A seek is waiting to be served, so nothing this decoder is still holding is worth
    // presenting. Returning rather than draining is what lets the seek happen *now*: the
    // frames in here belong to the old position and the packet loop is where the demuxer
    // can move.
    let seek_pending = || seek.is_some_and(crate::seek::SeekControl::pending);

    let mut decoded = ffmpeg::frame::Video::empty();
    while decoder.receive_frame(&mut decoded).is_ok() {
        if stop() {
            return Ok(Drained::Stopped);
        }
        if seek_pending() {
            return Ok(Drained::Continue);
        }
        let pts = frame_pts(decoded.pts(), time_base);
        // Seeds the wall clock for a file with no audio, and does nothing once audio has
        // anchored it — the first caller wins.
        clock.start_video_master(pts);

        if clock.is_hopeless(pts) {
            continue;
        }
        // Sleep in slices so neither a preemption nor a pause is stuck behind a long
        // hold. A frame is rarely more than a frame-interval early, so the cap normally
        // costs nothing; it earns its keep on a paused session, where the clock has
        // stopped and this frame's turn never comes until it starts again.
        //
        // `stop` is checked inside the wait, not merely around it, and that is the whole
        // reason the wait is sliced. A *paused* session's clock never advances, so a frame
        // that is waiting for its turn waits forever — and preemption (someone else casts)
        // would then leak this thread and its decoder, one per paused session, in silence.
        //
        // A seek is checked in the same place and for the same reason, with one extra
        // twist: scrubbing a *paused* session is the ordinary way people find a spot, and
        // that is precisely the state where the clock never advances, so a frame waiting
        // its turn would swallow the seek until somebody pressed play.
        // Fine pacing only, and *bounded*, which is the half of #52 the packet gate
        // cannot do on its own. Coarse pacing is `packet_due`'s: a packet is not sent to
        // the decoder until the clock is within `VIDEO_DECODE_LEAD` of wanting it, so by
        // the time a frame reaches here it is at most that far early and this loop has at
        // most that long to wait.
        //
        // The bound is not a nicety. This is the demux thread, and it is the only producer
        // of the audio that advances the clock it is waiting on — an unbounded wait here
        // is a thread waiting for itself, which is exactly what made a 60 fps cast play at
        // two frames a second. With the bound the worst case is a frame presented up to
        // `VIDEO_DECODE_LEAD` early, which is inside anyone's lip-sync tolerance and is
        // not a direction that accumulates.
        let hold_until = std::time::Instant::now() + VIDEO_DECODE_LEAD;
        loop {
            if stop() {
                return Ok(Drained::Stopped);
            }
            if seek_pending() {
                return Ok(Drained::Continue);
            }
            // A pause is the one wait that is *not* bounded, and must not be: the clock is
            // frozen on purpose, the demuxer running ahead would be decoding a paused
            // session, and nothing here can end it — only the source can. Sliced so the
            // thread is still reachable by a stop or a seek, which is what stops a paused
            // session from leaking its decoder when someone else casts.
            if clock.is_paused() {
                std::thread::sleep(PACING_SLICE);
                continue;
            }
            let Some(remaining) = hold_until.checked_duration_since(std::time::Instant::now())
            else {
                break;
            };
            let Some(wait) = clock.wait_for(pts) else {
                break;
            };
            std::thread::sleep(wait.min(PACING_SLICE).min(remaining));
        }

        let frame = match hw.export(&mut decoded, pts) {
            Export::Gpu(frame) => frame,
            Export::Dropped => continue,
            Export::Restart => return Ok(Drained::Restart),
            Export::Software => scale_to_rgba(&decoded, scaler, pts)?,
        };
        if !on_frame(frame) {
            return Ok(Drained::Stopped);
        }
    }
    Ok(Drained::Continue)
}

/// Convert a software frame to packed RGBA, rebuilding the scaler if the picture changed.
fn scale_to_rgba(
    decoded: &ffmpeg::frame::Video,
    scaler: &mut Option<ffmpeg::software::scaling::Context>,
    pts: Duration,
) -> Result<DecodedFrame, PipelineError> {
    let (format, width, height) = (decoded.format(), decoded.width(), decoded.height());
    // `cached` is a no-op when the parameters match and a rebuild when they do not,
    // which is what a sender changing resolution mid-mirror needs.
    let fresh = match scaler.take() {
        Some(mut sws) => {
            sws.cached(
                format,
                width,
                height,
                ffmpeg::format::Pixel::RGBA,
                width,
                height,
                SCALE_FLAGS,
            );
            sws
        }
        None => ffmpeg::software::scaling::context::Context::get(
            format,
            width,
            height,
            ffmpeg::format::Pixel::RGBA,
            width,
            height,
            SCALE_FLAGS,
        )
        .map_err(map_err)?,
    };
    let sws = scaler.insert(fresh);

    let mut rgba = ffmpeg::frame::Video::empty();
    sws.run(decoded, &mut rgba).map_err(map_err)?;
    Ok(to_decoded_frame(&rgba, pts))
}

/// A decoder timestamp in its own timebase, as a wall-clock offset.
fn frame_pts(pts: Option<i64>, time_base: ffmpeg::Rational) -> Duration {
    let secs = pts.map_or(0.0, |p| {
        p as f64 * f64::from(time_base.numerator()) / f64::from(time_base.denominator())
    });
    Duration::from_secs_f64(secs.max(0.0))
}

/// Copy a scaled RGBA `ffmpeg` frame into an owned [`DecodedFrame`], stripping row
/// padding (swscale may pad the stride past `width*4`).
fn to_decoded_frame(rgba: &ffmpeg::frame::Video, pts: Duration) -> DecodedFrame {
    let width = rgba.width();
    let height = rgba.height();
    let stride = rgba.stride(0);
    let row_bytes = (width as usize) * 4;
    let src = rgba.data(0);

    let mut data = Vec::with_capacity(row_bytes * height as usize);
    for row in 0..height as usize {
        let start = row * stride;
        data.extend_from_slice(&src[start..start + row_bytes]);
    }

    DecodedFrame::cpu(
        width,
        height,
        PixelFormat::Rgba8,
        pts,
        bytes::Bytes::from(data),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Assert a frame came back as system-memory RGBA of exactly the right size — the
    /// software path's contract with the compositor.
    fn assert_rgba(f: &DecodedFrame) {
        let castaway_core::FrameImage::Cpu { format, data } = &f.image else {
            panic!("software decode must produce a CPU frame");
        };
        assert_eq!(*format, PixelFormat::Rgba8);
        assert_eq!(data.len(), (f.width * f.height * 4) as usize);
    }

    /// Generate a tiny test clip with the ffmpeg CLI, or skip if it isn't available.
    ///
    /// `name` keeps each test on its own file: the suite runs tests in parallel threads,
    /// and two of them writing one path races a half-written container into a decoder.
    fn make_test_clip(name: &str) -> Option<std::path::PathBuf> {
        let dir = std::env::temp_dir().join("castaway-ffmpeg-test");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("{name}.mp4"));
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=10:duration=1",
            ])
            .arg("-pix_fmt")
            .arg("yuv420p")
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?;
        crate::test_media::resolve("a test clip", status.success().then_some(path))
    }

    #[test]
    fn decodes_testsrc_to_rgba_frames() {
        let Some(path) = make_test_clip("decodes-to-rgba") else {
            return;
        };
        let mut frames = 0usize;
        let mut first_dims = None;
        decode(path.to_str().unwrap(), HwPreference::SoftwareOnly, |f| {
            if first_dims.is_none() {
                first_dims = Some((f.width, f.height));
                assert_rgba(&f);
            }
            frames += 1;
            true
        })
        .unwrap();
        assert!(frames >= 5, "expected several frames, got {frames}");
        assert_eq!(first_dims, Some((64, 48)));
    }

    /// Spacing between the timestamps the test attaches to its frames.
    const FRAME_INTERVAL: Duration = Duration::from_millis(100);

    /// Generate a bare Annex-B H.264 elementary stream — no container, so the SPS/PPS is
    /// in-band exactly as a mirroring sender delivers it. `-bf 0` matches how a real
    /// sender encodes: B-frames trade latency for size, which mirroring never wants.
    fn make_test_stream(name: &str) -> Option<std::path::PathBuf> {
        let dir = std::env::temp_dir().join("castaway-ffmpeg-test");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("{name}.h264"));
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=10:duration=1",
            ])
            .args(["-pix_fmt", "yuv420p", "-bf", "0", "-f", "h264"])
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?;
        crate::test_media::resolve("a raw H.264 test stream", status.success().then_some(path))
    }

    /// Split that elementary stream into the per-frame units an adapter would hand us.
    /// Demuxing happens *here*, in the test — the point of [`decode_stream`] is that it
    /// never sees a container.
    ///
    /// Timestamps are the test's own, not the demuxer's: a raw elementary stream carries
    /// none, and a real adapter derives them from RTP anyway.
    fn encoded_frames(path: &std::path::Path) -> Vec<EncodedFrame> {
        ensure_init();
        let mut ictx = ffmpeg::format::input(&path).unwrap();
        let index = ictx
            .streams()
            .best(ffmpeg::media::Type::Video)
            .unwrap()
            .index();

        let mut out: Vec<EncodedFrame> = Vec::new();
        for (stream, packet) in ictx.packets() {
            if stream.index() != index {
                continue;
            }
            let Some(data) = packet.data() else { continue };
            out.push(EncodedFrame {
                video_codec: Some(VideoCodec::H264),
                audio_codec: None,
                pts: FRAME_INTERVAL * u32::try_from(out.len()).unwrap(),
                keyframe: packet.is_key(),
                data: bytes::Bytes::copy_from_slice(data),
            });
        }
        out
    }

    #[test]
    fn stream_decode_turns_pushed_frames_into_rgba() {
        let Some(path) = make_test_stream("stream-decode") else {
            return;
        };
        let mut input = encoded_frames(&path).into_iter();
        assert!(input.len() >= 5, "expected several encoded frames");

        let mut dims = None;
        let mut times = Vec::new();
        decode_stream(
            VideoCodec::H264,
            HwPreference::SoftwareOnly,
            || input.next(),
            |f| {
                if dims.is_none() {
                    dims = Some((f.width, f.height));
                    assert_rgba(&f);
                }
                times.push(f.pts);
                true
            },
        )
        .unwrap();

        assert!(
            times.len() >= 5,
            "expected several decoded frames, got {}",
            times.len()
        );
        assert_eq!(dims, Some((64, 48)));

        // The timestamps we attached come back attached to the right pictures. If the
        // decoder were not carrying them, every frame would land at zero and the
        // compositor would have nothing to pace against.
        let want: Vec<_> = (0..times.len())
            .map(|i| FRAME_INTERVAL * u32::try_from(i).unwrap())
            .collect();
        assert_eq!(times, want);
    }

    #[test]
    fn stream_decode_waits_for_a_key_frame_before_starting() {
        let Some(path) = make_test_stream("stream-sync") else {
            return;
        };
        // Join mid-stream, as a receiver attaching to a live mirror does. Everything
        // before the next key frame references pictures we never saw, so nothing may be
        // decoded until one arrives — and this clip has only the one, at the start.
        let mut input = encoded_frames(&path).into_iter().skip(1);
        let mut frames = 0usize;
        decode_stream(
            VideoCodec::H264,
            HwPreference::SoftwareOnly,
            || input.next(),
            |_f| {
                frames += 1;
                true
            },
        )
        .unwrap();
        assert_eq!(frames, 0, "decoded {frames} frames without ever syncing");
    }

    #[test]
    fn stream_decode_skips_frames_in_another_codec() {
        let Some(path) = make_test_stream("stream-wrong-codec") else {
            return;
        };
        // An audio frame on the video source, or a sender that switched codecs without
        // renegotiating: feeding those to an H.264 decoder is how you get a hang or a
        // wall of garbage. They must be dropped, not decoded.
        let mut input = encoded_frames(&path).into_iter().map(|mut f| {
            f.video_codec = Some(VideoCodec::Vp8);
            f
        });
        let mut frames = 0usize;
        decode_stream(
            VideoCodec::H264,
            HwPreference::SoftwareOnly,
            || input.next(),
            |_f| {
                frames += 1;
                true
            },
        )
        .unwrap();
        assert_eq!(frames, 0);
    }

    #[test]
    fn every_negotiable_video_codec_has_a_decoder() {
        // A codec we advertise in an OFFER/ANSWER but cannot actually decode is a black
        // screen at the far end of a successful handshake — the worst kind of failure.
        ensure_init();
        for codec in [
            VideoCodec::H264,
            VideoCodec::Hevc,
            VideoCodec::Vp8,
            VideoCodec::Vp9,
        ] {
            let id = codec_id(codec);
            assert!(
                ffmpeg::decoder::find(id).is_some(),
                "{codec:?} is negotiable but this ffmpeg build cannot decode it",
            );
        }
    }

    #[test]
    fn callback_can_stop_early() {
        let Some(path) = make_test_clip("stop-early") else {
            return;
        };
        let mut frames = 0usize;
        decode(path.to_str().unwrap(), HwPreference::SoftwareOnly, |_f| {
            frames += 1;
            frames < 2 // stop after the second frame
        })
        .unwrap();
        assert_eq!(frames, 2);
    }
}
