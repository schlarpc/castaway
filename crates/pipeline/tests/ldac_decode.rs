//! LDAC decode, against checked-in bitstreams rather than a live phone.
//!
//! The other half of the LDAC test vectors. `proto_bluetooth_audio::media` replays the
//! same audio as whole A2DP packets and asserts on its *framing* with a pure parser, in
//! every build; this replays the transport frames through Sony's library and asserts on
//! the *audio*, in builds that link it (`--features ldac`).
//!
//! The assertions are deliberately about sound rather than about the absence of errors,
//! for the reason the aptX test in `audio_decode` gives: nearly every way this path breaks
//! still produces samples. A decoder that walks a payload once instead of frame by frame
//! yields a sixth of the audio; the wrong sample format yields silence of the right length;
//! a mishandled channel layout yields one channel of a stereo pair. All three pass a check
//! that only looks for `Ok`.
//!
//! Fixtures come from `cargo run -p pipeline --features ldac --example ldac_fixtures`.
//! What they contain and why they were generated rather than captured is documented there.

use std::time::Duration;

use castaway_core::{AudioCodec, AudioFormat, EncodedFrame};
use pipeline::audio_decode::{can_decode, AudioDecoder, PcmBlock};

/// 440 Hz at 24000/32768 of full scale went in, so a faithful decode comes back at an RMS
/// near 0.732 / sqrt(2) = 0.518. LDAC at MQ is lossy but not by much; the window is wide
/// enough for the codec and far too narrow for silence, for half-scale, or for one channel.
const EXPECTED_RMS: std::ops::Range<f32> = 0.35..0.65;

/// Split a fixture into its length-prefixed records.
fn records(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while let Some(header) = data.get(at..at + 4) {
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        at += 4;
        let Some(record) = data.get(at..at + len) else {
            break;
        };
        out.push(record);
        at += len;
    }
    out
}

/// One transport-frame sequence as the depacketizer would hand it over.
fn frame(payload: &[u8], pts: Duration) -> EncodedFrame {
    EncodedFrame {
        video_codec: None,
        audio_codec: Some(AudioCodec::Ldac),
        pts,
        keyframe: true,
        data: bytes::Bytes::copy_from_slice(payload),
    }
}

/// Decode a whole fixture, returning every block.
fn decode_all(fixture: &[u8], format: AudioFormat) -> Vec<PcmBlock> {
    let mut decoder = AudioDecoder::new(AudioCodec::Ldac, format, None).expect("open LDAC decoder");
    let mut blocks = Vec::new();
    for (n, payload) in records(fixture).into_iter().enumerate() {
        // A plausible presentation time per packet; the decoder derives the times of the
        // frames inside one from the audio the earlier ones produced.
        let pts = Duration::from_millis(n as u64 * 17);
        decoder
            .decode(&frame(payload, pts), |block| blocks.push(block))
            .expect("decode");
    }
    decoder.flush(|block| blocks.push(block)).expect("flush");
    blocks
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let n = samples.len() as f32;
    (samples.iter().map(|s| s * s).sum::<f32>() / n).sqrt()
}

fn format(rate: u32, channels: u16) -> AudioFormat {
    AudioFormat::from_hz(rate, channels).expect("a sane format")
}

#[test]
fn ldac_is_decodable_when_the_feature_that_binds_the_library_is_on() {
    // The inverse of the test in `audio_decode` that asserts LDAC is *not* claimed on a
    // build with no backend. Together they are the whole of Q22: the advertised endpoint
    // table follows what exists, in both directions. If this fails while the feature is on,
    // the endpoint silently disappears and every sender falls back — better than silence,
    // but not what was asked for.
    assert!(
        can_decode(AudioCodec::Ldac),
        "the ldac backend must be live"
    );
}

#[test]
fn a_stereo_stream_decodes_to_audio_that_sounds_like_what_went_in() {
    let blocks = decode_all(
        include_bytes!("fixtures/ldac-44100-stereo.bin"),
        format(44_100, 2),
    );
    assert!(!blocks.is_empty(), "LDAC produced no audio");

    // Reported format comes from the *stream*, which is where LDAC keeps it.
    for block in &blocks {
        assert_eq!(block.sample_rate, 44_100);
        assert_eq!(block.channels, 2);
    }

    // 84 transport frames of 128 samples per channel — the count the encoder reported when
    // the fixture was made, and the count `proto-bluetooth-audio` walks to from the same
    // bytes. A decoder that called `ldacBT_decode` once per packet instead of once per
    // frame would land on 14 x 128 = 1792 here and pass every other assertion in this file.
    let total: usize = blocks.iter().map(PcmBlock::frame_count).sum();
    assert_eq!(total, 84 * 128, "every transport frame must be decoded");

    let samples: Vec<f32> = blocks
        .iter()
        .flat_map(|b| b.samples.iter().copied())
        .collect();
    let level = rms(&samples);
    assert!(
        EXPECTED_RMS.contains(&level),
        "decoded level {level} is not what a 440 Hz sine should produce"
    );

    // Both channels, evenly. A mono sine was encoded to both, so an imbalance means the
    // interleave is wrong — the failure that put audio in the left ear and silence in the
    // right for every ffmpeg codec in this pipeline before `pcm_from_frame` was fixed.
    let left = rms(&samples.iter().step_by(2).copied().collect::<Vec<_>>());
    let right = rms(&samples
        .iter()
        .skip(1)
        .step_by(2)
        .copied()
        .collect::<Vec<_>>());
    assert!(
        right > 0.05,
        "the right channel is silent: {left} vs {right}"
    );
    assert!(
        (left - right).abs() < 0.05,
        "a mono sine should decode evenly: {left} vs {right}"
    );
}

#[test]
fn presentation_times_advance_across_the_frames_inside_one_packet() {
    // Six transport frames arrive in one A2DP packet with one RTP timestamp between them.
    // If every block took that timestamp, six blocks would claim the same instant and the
    // output would have nothing to pace against.
    let blocks = decode_all(
        include_bytes!("fixtures/ldac-44100-stereo.bin"),
        format(44_100, 2),
    );
    let first_packet: Vec<&PcmBlock> = blocks.iter().take(6).collect();
    for pair in first_packet.windows(2) {
        assert!(
            pair[1].pts > pair[0].pts,
            "pts went {:?} -> {:?}",
            pair[0].pts,
            pair[1].pts
        );
    }
    // 128 samples at 44.1 kHz is about 2.9 ms, so six frames span about 14.5 ms.
    let span = first_packet[5].pts - first_packet[0].pts;
    assert!(
        span > Duration::from_millis(13) && span < Duration::from_millis(16),
        "six frames spanned {span:?}"
    );
}

#[test]
fn the_stream_decodes_at_its_own_rate_and_not_the_one_we_negotiated() {
    // 96 kHz dual channel, opened with a decoder told 44.1 kHz stereo — which is what a
    // sender ignoring the negotiated configuration would produce. Sony's decoder reads the
    // rate out of the frame header and reconfigures itself, so the audio is fine; the point
    // is that the blocks say 96 kHz. If they said 44.1, the output device would be opened
    // at the wrong rate and everything would play more than twice too slow, with every
    // layer reporting success (Q25, in the one codec where the stream can be checked).
    let blocks = decode_all(
        include_bytes!("fixtures/ldac-96000-dual.bin"),
        format(44_100, 2),
    );
    assert!(!blocks.is_empty());
    for block in &blocks {
        assert_eq!(block.sample_rate, 96_000, "the stream's rate must win");
    }
    // 256 samples per channel per frame at 96 kHz, 42 frames.
    let total: usize = blocks.iter().map(PcmBlock::frame_count).sum();
    assert_eq!(total, 42 * 256);

    let samples: Vec<f32> = blocks
        .iter()
        .flat_map(|b| b.samples.iter().copied())
        .collect();
    assert!(
        EXPECTED_RMS.contains(&rms(&samples)),
        "dual channel is silent"
    );
}

#[test]
fn a_mono_stream_is_not_labelled_with_the_channel_count_we_negotiated() {
    // The one configuration where a block's size differs from a stereo one, and therefore
    // the only one that catches the shortcut this test exists to forbid: the library reports
    // the sample rate and *not* the channel count, so reusing the negotiated count is the
    // obvious move and is wrong. A mono block labelled stereo halves its own reported
    // duration, which the output device then plays at roughly double speed while every
    // layer reports success.
    let blocks = decode_all(
        include_bytes!("fixtures/ldac-44100-mono.bin"),
        // Negotiated stereo, deliberately — the mismatch is the point.
        format(44_100, 2),
    );
    assert!(!blocks.is_empty());
    for block in &blocks {
        assert_eq!(block.channels, 1, "a mono frame decodes to one channel");
        assert_eq!(block.sample_rate, 44_100);
        // 128 samples per channel at 44.1 kHz, and one channel.
        assert_eq!(block.frame_count(), 128);
    }
    let total: usize = blocks.iter().map(PcmBlock::frame_count).sum();
    assert_eq!(
        total,
        84 * 128,
        "the same frame count as the stereo fixture"
    );

    let samples: Vec<f32> = blocks
        .iter()
        .flat_map(|b| b.samples.iter().copied())
        .collect();
    assert!(EXPECTED_RMS.contains(&rms(&samples)), "mono is silent");
}

#[test]
fn a_payload_with_its_a2dp_header_still_attached_produces_nothing() {
    // The framing mistake, from the decoder's side. `proto-bluetooth-audio` refuses this
    // before it gets here — but that guard is one `if` away from being deleted, and this is
    // what the consequence looks like: no syncword, no audio, and a log line rather than a
    // stream of noise.
    let fixture = include_bytes!("fixtures/ldac-44100-stereo.bin");
    let payload = records(fixture)[0];
    let mut shifted = vec![0x06u8]; // the frame-count byte that should have come off
    shifted.extend_from_slice(payload);

    let mut decoder =
        AudioDecoder::new(AudioCodec::Ldac, format(44_100, 2), None).expect("open decoder");
    let mut blocks = 0usize;
    decoder
        .decode(&frame(&shifted, Duration::ZERO), |_| blocks += 1)
        .expect("a bad packet is not fatal");
    assert_eq!(blocks, 0, "a mis-framed payload must decode to nothing");
}

#[test]
fn a_corrupt_frame_is_skipped_rather_than_ending_the_session() {
    // One bad packet off a radio link must not take the music down — the same contract the
    // ffmpeg backend holds to.
    let mut decoder =
        AudioDecoder::new(AudioCodec::Ldac, format(44_100, 2), None).expect("open decoder");
    // Syncword present, everything after it nonsense.
    let garbage = [0xAAu8, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
    assert!(decoder
        .decode(&frame(&garbage, Duration::ZERO), |_| {})
        .is_ok());
    // And a fresh decoder still works afterwards.
    let blocks = decode_all(
        include_bytes!("fixtures/ldac-44100-stereo.bin"),
        format(44_100, 2),
    );
    assert!(!blocks.is_empty());
}

#[test]
fn a_rate_ldac_cannot_carry_is_refused_at_open() {
    // LDAC's A2DP capability can advertise 176.4 and 192 kHz; its bitstream has no
    // sample-rate index for either, and the library refuses to initialise at them. Better
    // here than as a stream of rejected frames, and it is why `codec::advertised` offers
    // only the four rates the codec can actually code.
    assert!(
        AudioDecoder::new(AudioCodec::Ldac, format(192_000, 2), None).is_err(),
        "192 kHz is advertisable and not codeable; open must fail"
    );
    assert!(AudioDecoder::new(AudioCodec::Ldac, format(96_000, 2), None).is_ok());
}

#[test]
fn a_whole_session_turns_ldac_frames_into_played_audio() {
    // The join, not the layers. Everything above proves the decoder; this proves a sample
    // reached an output — the assertion `selfplay` was missing from the other end, where
    // `is_playing` was echoed back from our own state machine and was true of a receiver
    // making no noise at all.
    use pipeline::audio_out::AudioOut;
    use pipeline::error::PipelineError;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    /// What the output was handed, readable after `run` has consumed the sink.
    #[derive(Default)]
    struct Played {
        frames: AtomicU64,
        peak: Mutex<f32>,
        opened_at: Mutex<Option<(u32, u16)>>,
    }

    // A newtype rather than `impl AudioOut for Arc<Played>`: `Arc` is not `#[fundamental]`,
    // so that impl is orphaned from an integration-test crate even though the in-crate
    // tests get away with it.
    struct Speaker(Arc<Played>);

    impl AudioOut for Speaker {
        fn start(&mut self, rate: u32, channels: u16) -> Result<(), PipelineError> {
            *self.0.opened_at.lock().expect("poisoned") = Some((rate, channels));
            Ok(())
        }
        fn write(&mut self, block: &PcmBlock) -> Result<(), PipelineError> {
            self.0
                .frames
                .fetch_add(block.frame_count() as u64, Ordering::SeqCst);
            let loudest = block.samples.iter().fold(0.0f32, |a, s| a.max(s.abs()));
            let mut peak = self.0.peak.lock().expect("poisoned");
            *peak = peak.max(loudest);
            Ok(())
        }
        fn stop(&mut self) {}
    }

    let fixture = include_bytes!("fixtures/ldac-44100-stereo.bin");
    let payloads = records(fixture);
    let (tx, rx) = tokio::sync::mpsc::channel(payloads.len() + 1);
    for (n, payload) in payloads.iter().enumerate() {
        tx.blocking_send(frame(payload, Duration::from_millis(n as u64 * 17)))
            .expect("queue");
    }
    drop(tx);

    let speaker = Arc::new(Played::default());
    pipeline::audio_session::run(
        rx,
        format(44_100, 2),
        None,
        Box::new(Speaker(Arc::clone(&speaker))),
        &AtomicBool::new(false),
        &pipeline::audio_session::Gain::default(),
    );

    let played = speaker.frames.load(Ordering::SeqCst);
    assert_eq!(played, 84 * 128, "the whole fixture must reach the output");
    assert!(
        *speaker.peak.lock().expect("poisoned") > 0.05,
        "the output received silence"
    );
    assert_eq!(
        *speaker.opened_at.lock().expect("poisoned"),
        Some((44_100, 2)),
        "the device must be opened at the rate the blocks carry"
    );
}
