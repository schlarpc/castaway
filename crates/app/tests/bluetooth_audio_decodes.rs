//! The seam between the Bluetooth protocol half and the decode half (#187).
//!
//! Both halves are well covered and **nothing joined them**. `adapter_end_to_end.rs`
//! drives the real state machines against real wire bytes and stops at an encoded
//! `SourceMessage::Frame`; the decode tests in `pipeline::audio_decode` start at an
//! `EncodedFrame` built by hand. So an off-by-one in the SBC or LDAC one-byte header
//! strip, or a LATM access unit that looks right and is not what ffmpeg wants, produces a
//! connected phone, a running session, and silence — with nothing failing.
//!
//! This is that join, and it lives in `app` because it is the only crate that depends on
//! both: `proto-bluetooth-audio` has no decoder and `pipeline` has no depacketiser, which
//! is the layering working (rule 2) and is exactly why the gap existed.
//!
//! The AAC case is the one that mattered. AAC has no in-band configuration a decoder can
//! recover — the `AudioSpecificConfig` lives in the AVDTP capability exchange and the
//! multiplex header — it sits 4th in `advertised()`, it is offered whenever ffmpeg reports
//! it, and **it is what every iPhone picks**. Before this it had no decode coverage at
//! all: no RMS test, no level test, not even a `can_decode` assertion.
#![cfg(feature = "audio")]
#![allow(clippy::unwrap_used)]

use bytes::{BufMut as _, Bytes, BytesMut};
use castaway_core::{AudioCodec, AudioFormat};
use pipeline::audio_decode::AudioDecoder;
use proto_bluetooth_audio::Depacketizer;

/// A real iPhone streaming A2DP AAC, captured after the RTP header was stripped and
/// length-prefixed.
///
/// Reached across the crate boundary rather than copied: it is 23 KB of somebody's actual
/// phone, `proto-bluetooth-audio` is where it belongs, and two copies would drift.
const IPHONE_AAC: &[u8] =
    include_bytes!("../../proto-bluetooth-audio/tests/fixtures/a2dp-aac-iphone.bin");

/// Split a length-prefixed fixture into its records.
fn records(data: &[u8]) -> Vec<Bytes> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 4 <= data.len() {
        let len = u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize;
        at += 4;
        if at + len > data.len() {
            break;
        }
        out.push(Bytes::copy_from_slice(&data[at..at + len]));
        at += len;
    }
    out
}

/// Wrap a payload in the RTP header A2DP carries it under.
fn rtp(sequence: u16, timestamp: u32, payload: &[u8]) -> Bytes {
    let mut buf = BytesMut::with_capacity(12 + payload.len());
    buf.put_u8(0x80); // version 2, no padding/extension/csrc
    buf.put_u8(96); // dynamic payload type
    buf.put_u16(sequence);
    buf.put_u32(timestamp);
    buf.put_u32(0xDEAD_BEEF);
    buf.extend_from_slice(payload);
    buf.freeze()
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let n = samples.len() as f32;
    (samples.iter().map(|s| s * s).sum::<f32>() / n).sqrt()
}

/// What an iPhone actually sends becomes audio, through the code that would carry it.
///
/// The strongest evidence available for this path and the reason it is worth the
/// cross-crate include: every other codec's decode test feeds our own encoder's output
/// back to our own decoder, so a misunderstanding shared between the two configurations is
/// invisible. These bytes were not produced by anything in this tree.
///
/// What it would catch, none of which the halves catch separately: a LATM header length
/// computed from the wrong `StreamMuxConfig`, an access unit handed over with its
/// multiplex still attached (which ffmpeg rejects as invalid data and logs nothing), and
/// the `AAC_LATM` decoder id being used instead of `AAC` — that one refuses every packet
/// silently, and is a mistake the *name* invites.
#[test]
fn what_an_iphone_sends_over_a2dp_becomes_audio() {
    if !pipeline::audio_decode::can_decode(AudioCodec::Aac) {
        // Not a skip this build is allowed to take quietly: AAC being undecodable is the
        // #14 failure for the most common sender there is.
        panic!(
            "this build advertises AAC to every phone that asks and cannot decode it; \
             an iPhone would connect and play silence"
        );
    }

    let payloads = records(IPHONE_AAC);
    assert!(
        payloads.len() > 20,
        "the fixture should carry a couple of dozen packets, got {}",
        payloads.len()
    );

    // The real depacketiser, at the rate the capture was taken at.
    let mut depacketizer = Depacketizer::new(AudioCodec::Aac, 44_100);
    let mut decoder = AudioDecoder::new(
        AudioCodec::Aac,
        AudioFormat::from_hz(44_100, 2).unwrap(),
        None,
    )
    .unwrap();

    let mut decoded: Vec<f32> = Vec::new();
    let mut reported: Option<(u32, u16)> = None;
    let mut frames = 0usize;

    for (n, payload) in payloads.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let packet = rtp(n as u16, n as u32 * 1024, payload);
        let frame = depacketizer
            .push(packet)
            .unwrap_or_else(|e| panic!("packet {n} did not depacketise: {e}"));
        frames += 1;
        decoder
            .decode(&frame, |block| {
                reported = Some((block.sample_rate, block.channels));
                decoded.extend_from_slice(&block.samples);
            })
            .unwrap_or_else(|e| panic!("packet {n} depacketised and did not decode: {e}"));
    }
    decoder
        .flush(|block| decoded.extend_from_slice(&block.samples))
        .unwrap();

    assert_eq!(frames, payloads.len(), "every packet must yield a frame");
    assert!(
        !decoded.is_empty(),
        "the whole capture depacketised and decoded to nothing — which is precisely the \
         shape of the failure: no error anywhere, and no sound"
    );
    assert_eq!(
        reported,
        Some((44_100, 2)),
        "an iPhone streams 44.1 kHz stereo; a decoder reporting anything else plays it at \
         the wrong pitch rather than failing"
    );

    // Roughly the right amount of it. AAC-LC is 1024 samples per access unit per channel,
    // and the decoder drops the first frame while it primes — so anything within a frame
    // or two of `packets * 1024` is right, and half or double is not.
    let expected = payloads.len() * 1024;
    let got = decoded.len() / 2;
    assert!(
        got.abs_diff(expected) <= 4096,
        "decoded {got} frames from {} packets; expected about {expected}. A count that is \
         half or double this is a channel-layout or rate error, both of which decode \
         cleanly and sound wrong",
        payloads.len()
    );

    // And it is music rather than silence, in both ears.
    //
    // The capture is somebody's actual playback, so there is no reference waveform to
    // correlate against — shape is covered in `pipeline::audio_decode` against a signal we
    // control, and what can honestly be asserted here is that a real signal came out.
    //
    // It is **quiet**: measured on this fixture, RMS 0.0065 (about −44 dBFS) and peak
    // 0.039 (about −28 dBFS), with 86% of samples non-zero. So the thresholds below are
    // set from that measurement with a factor of three of room, and are chosen to
    // discriminate rather than to be tight — the failures they exist for are digital
    // silence (a wrong sample format, which gives exact zeroes) and full-scale noise (a
    // misparsed access unit), and those are three orders of magnitude away in either
    // direction, not a factor of three.
    let level = rms(&decoded);
    let peak = decoded.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    let nonzero = decoded.iter().filter(|s| s.abs() > 1e-4).count();

    assert!(
        peak > 0.01,
        "decoded {} samples with a peak of {peak} — that is silence with the right length, \
         which is exactly what a wrong sample format produces",
        decoded.len()
    );
    assert!(
        peak < 0.9,
        "peak {peak} is at the rails; a misparsed access unit decodes to noise, and noise \
         clips"
    );
    assert!(
        level > 0.002,
        "RMS {level} over a peak of {peak}: a handful of samples and silence in between is \
         not what a second of music looks like"
    );
    assert!(
        nonzero * 2 > decoded.len(),
        "only {nonzero} of {} samples carry anything",
        decoded.len()
    );

    let left = rms(&decoded.iter().step_by(2).copied().collect::<Vec<_>>());
    let right = rms(&decoded
        .iter()
        .skip(1)
        .step_by(2)
        .copied()
        .collect::<Vec<_>>());
    assert!(
        left > 0.001 && right > 0.001,
        "one ear is silent: left {left}, right {right}. Reading a planar frame through an \
         accessor sized from `linesize[0]` gives exactly this"
    );
}

/// The same join with a signal we control, so the assertion can be on the waveform.
///
/// LDAC is the one codec with fixtures on *both* sides of the seam, from the same audio:
/// `proto-bluetooth-audio` has the whole A2DP packets and `pipeline` has the transport
/// frames, and both were written by one run of `examples/ldac_fixtures.rs`. So this needs
/// no encoder — it takes the packets, runs them through the real depacketiser, and
/// decodes what comes out, which is precisely the path that had no test.
///
/// What it catches that neither half catches alone: the one-byte A2DP header strip being
/// off by one. `media.rs` asserts the payload starts at a frame boundary (`data[0] ==
/// 0xAA`) and `ldac_decode.rs` decodes frames that were never packetised — so a strip that
/// took the wrong byte count would satisfy the first if the sync byte happened to land,
/// and never reach the second at all.
#[cfg(feature = "ldac")]
#[test]
fn ldac_survives_the_trip_through_the_depacketiser() {
    let rate = 44_100u32;
    assert!(
        pipeline::audio_decode::can_decode(AudioCodec::Ldac),
        "this build advertises LDAC and cannot decode it (#14)"
    );

    // The A2DP packets a sender puts on the air, not the transport frames underneath.
    let packets = records(include_bytes!(
        "../../proto-bluetooth-audio/tests/fixtures/a2dp-ldac-44100-stereo.bin"
    ));
    assert_eq!(packets.len(), 14, "the fixture is 14 packets of 6 frames");

    let mut depacketizer = Depacketizer::new(AudioCodec::Ldac, rate);
    let mut decoder = AudioDecoder::new(
        AudioCodec::Ldac,
        AudioFormat::from_hz(rate, 2).unwrap(),
        None,
    )
    .unwrap();
    let mut decoded: Vec<f32> = Vec::new();

    for (n, packet) in packets.into_iter().enumerate() {
        let frame = depacketizer
            .push(packet)
            .unwrap_or_else(|e| panic!("packet {n} did not depacketise: {e}"));
        decoder
            .decode(&frame, |b| decoded.extend_from_slice(&b.samples))
            .unwrap_or_else(|e| panic!("packet {n} depacketised and did not decode: {e}"));
    }
    decoder
        .flush(|b| decoded.extend_from_slice(&b.samples))
        .unwrap();

    // 84 transport frames of 128 samples per channel — the count the encoder reported, the
    // count `proto-bluetooth-audio`'s pure parser walks to, and now the count that survives
    // being decoded through the seam between them.
    assert_eq!(
        decoded.len() / 2,
        84 * 128,
        "every transport frame in every packet must reach the decoder"
    );

    // And it is the tone that was encoded. `examples/ldac_fixtures.rs` writes 440 Hz at
    // three quarters of full scale; a decode reached through the depacketiser that lost a
    // frame, or started one byte late, is a different waveform and not a quieter one.
    #[allow(clippy::cast_precision_loss)]
    let reference: Vec<f32> = (0..decoded.len() / 2)
        .map(|n| {
            let t = n as f32 / rate as f32;
            (t * 440.0 * std::f32::consts::TAU).sin() * (24_000.0 / 32_768.0)
        })
        .collect();
    let left: Vec<f32> = decoded.iter().step_by(2).copied().collect();
    let right: Vec<f32> = decoded.iter().skip(1).step_by(2).copied().collect();

    for (name, channel) in [("left", &left), ("right", &right)] {
        let score = best_correlation(&reference, channel, rate as usize / 50);
        assert!(
            score >= 0.98,
            "the {name} channel correlates {score:.4} with the tone that was encoded"
        );
    }
}

/// The best normalised cross-correlation over lags in `0..=max_lag`.
///
/// A lag search because the codec has latency of its own and that is not what is under
/// test; that a single alignment explains the whole signal is.
#[cfg(feature = "ldac")]
fn best_correlation(reference: &[f32], decoded: &[f32], max_lag: usize) -> f32 {
    let mut best = 0.0f32;
    for lag in 0..=max_lag {
        if lag >= decoded.len() {
            break;
        }
        let n = reference.len().min(decoded.len() - lag);
        let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
        for i in 0..n {
            let (x, y) = (f64::from(reference[i]), f64::from(decoded[lag + i]));
            dot += x * y;
            na += x * x;
            nb += y * y;
        }
        if na > 0.0 && nb > 0.0 {
            #[allow(clippy::cast_possible_truncation)]
            let score = (dot / (na.sqrt() * nb.sqrt())) as f32;
            best = best.max(score);
        }
    }
    best
}

/// A real Android phone's LDAC, decoded through the real depacketiser.
///
/// The LDAC test above proves the *seam* with audio we encoded ourselves, which means it
/// can only ever be as right as our encoder. This one is 100 packets straight off an
/// Android phone (2026-08-08, over the AX210, `examples/phone_bench`), which is #14's
/// point stated exactly: decode what a phone actually sends, not what a fixture does.
///
/// There is no reference tone to correlate against here, so the assertions are the ones a
/// wrong decode would fail:
///
/// * **the sample count**, which pins the frame geometry. 382 LDAC frames across the 100
///   packets and 97792 sample frames out is 256 samples per frame — the high-rate frame
///   size, twice the 128 LDAC uses at 44.1/48 kHz. A decoder that took the low-rate size
///   would return exactly half of this and still sound like something.
/// * **that the channels differ**, because a depacketiser that lost the interleave, or a
///   decode that duplicated one channel, produces plausible audio that is not stereo.
/// * **no clipping and no DC**, which is what a byte-misaligned decode of a sane signal
///   turns into.
///
/// Worth recording because it cost an hour of arithmetic: this phone's **RTP timestamps
/// advance 512 per packet regardless of whether the packet carries 2 frames or 4**, so
/// they imply half the audio that is actually there. The decoded length is the one that
/// matches the wall clock — 4188 packets of this stream decode to 39.7s, and the phone
/// streamed for about that long. Do not use these timestamps to derive a frame size.
#[cfg(feature = "ldac")]
#[test]
fn a_real_android_phones_ldac_decodes_through_the_depacketiser() {
    let rate = 96_000u32;
    assert!(
        pipeline::audio_decode::can_decode(AudioCodec::Ldac),
        "this build advertises LDAC and cannot decode it (#14)"
    );

    let packets = records(include_bytes!(
        "../../proto-bluetooth-audio/tests/fixtures/a2dp-ldac-96000-android.bin"
    ));
    assert_eq!(packets.len(), 100, "the fixture is 100 A2DP packets");

    let mut depacketizer = Depacketizer::new(AudioCodec::Ldac, rate);
    let mut decoder = AudioDecoder::new(
        AudioCodec::Ldac,
        AudioFormat::from_hz(rate, 2).unwrap(),
        None,
    )
    .unwrap();
    let mut decoded: Vec<f32> = Vec::new();

    for (n, packet) in packets.into_iter().enumerate() {
        let frame = depacketizer
            .push(packet)
            .unwrap_or_else(|e| panic!("packet {n} did not depacketise: {e}"));
        decoder
            .decode(&frame, |b| decoded.extend_from_slice(&b.samples))
            .unwrap_or_else(|e| panic!("packet {n} depacketised and did not decode: {e}"));
    }
    decoder
        .flush(|b| decoded.extend_from_slice(&b.samples))
        .unwrap();

    assert_eq!(
        decoded.len() / 2,
        382 * 256,
        "382 LDAC frames of 256 samples each must reach the decoder"
    );

    let left: Vec<f32> = decoded.iter().step_by(2).copied().collect();
    let right: Vec<f32> = decoded.iter().skip(1).step_by(2).copied().collect();

    for (name, channel) in [("left", &left), ("right", &right)] {
        let peak = channel.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        #[allow(clippy::cast_precision_loss)]
        let rms = (channel.iter().map(|s| s * s).sum::<f32>() / channel.len() as f32).sqrt();
        #[allow(clippy::cast_precision_loss)]
        let dc = channel.iter().sum::<f32>() / channel.len() as f32;

        assert!(
            rms > 0.02,
            "the {name} channel decoded to near-silence: {rms}"
        );
        assert!(peak < 1.0, "the {name} channel clipped at {peak}");
        assert!(
            dc.abs() < 0.01,
            "the {name} channel has a DC offset of {dc}, which a sane decode does not"
        );
    }

    // Real stereo. A duplicated channel is the failure this catches, and it is one that
    // sounds entirely fine.
    let difference = left
        .iter()
        .zip(&right)
        .map(|(l, r)| (l - r).abs())
        .fold(0.0f32, f32::max);
    assert!(
        difference > 0.01,
        "the channels are identical to {difference}, so the stereo image was lost"
    );
}
