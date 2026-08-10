//! The seam between the AirPlay mirroring half and the video decode half (#281).
//!
//! `raop_session.rs` proves the mirroring machinery — FairPlay derivation, the SHA-512
//! stream keys, the continuous keystream, the AVCC→Annex-B rewrite — with payloads that
//! are ASCII placeholders, so it proves the bytes we sent came back out and cannot fail
//! on media a decoder would refuse. The decode tests in `pipeline::ffmpeg_decode` prove
//! H.264 decodes from streams built by hand. Nothing joined them: an SPS/PPS record
//! rebuilt wrong, a keystream half a block out, or a start-code rewrite that mangles one
//! NAL all deliver *something* — and only a decoder can say whether it is a picture.
//!
//! This is that join, in `app` because it is the only crate depending on both halves
//! (rule 2), exactly like `airplay_audio_decodes.rs` (#189). The harness is
//! `proto-airplay`'s own, reached by `#[path]`: one copy, or the session driven here
//! drifts from the one proven there.
#![cfg(feature = "render")]
#![allow(clippy::unwrap_used)]
// Tests bind ephemeral loopback sockets that never face the LAN; the registry
// (crates/app/src/surface.rs) governs production binds.
#![allow(clippy::disallowed_methods)]

use std::net::SocketAddr;
use std::time::Duration;

use aes::cipher::{KeyIvInit as _, StreamCipher as _};
use castaway_core::{
    EncodedFrame, FrameImage, FrameSource, MediaPorts, PixelFormat, SessionEvent, VideoCodec,
};
use pipeline::hwaccel::HwPreference;
use proto_airplay::{MirrorKeys, StreamConnectionId};
use tokio::io::AsyncWriteExt as _;

#[path = "../../proto-airplay/tests/raop_harness/mod.rs"]
mod raop;

/// A real H.264 IDR + P pair: two frames of an animated smooth gradient, encoded by
/// libx264 at 64×36 — tiny on purpose, because the point is real NAL structure, not
/// pixels. Annex-B, NALs in order SPS, PPS, SEI, IDR slice, P slice.
///
/// In `proto-airplay`'s fixtures for the same reason the ALAC tone is: it is AirPlay
/// wire media, and two copies would drift. Regenerate (with `SOURCE_RGB`) via:
///
/// ```text
/// ffmpeg -f lavfi -i "gradients=size=64x36:rate=30:speed=1:seed=42" -frames:v 2 \
///        -pix_fmt rgb24 -f rawvideo mirror-h264-gradients-64x36.rgb
/// ffmpeg -f rawvideo -pix_fmt rgb24 -s 64x36 -r 30 -i mirror-h264-gradients-64x36.rgb \
///        -c:v libx264 -preset veryslow -crf 10 \
///        -x264-params "bframes=0:keyint=300:min-keyint=300:scenecut=0" \
///        -pix_fmt yuv420p -frames:v 2 -f h264 mirror-h264-gradients-64x36.h264
/// ```
const H264: &[u8] =
    include_bytes!("../../proto-airplay/tests/fixtures/mirror-h264-gradients-64x36.h264");

/// The encoder's input: the same two frames as packed RGB24, straight off `gradients`.
const SOURCE_RGB: &[u8] =
    include_bytes!("../../proto-airplay/tests/fixtures/mirror-h264-gradients-64x36.rgb");

const WIDTH: usize = 64;
const HEIGHT: usize = 36;

/// Split an Annex-B stream into NAL units, without their start codes.
fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                starts.push((i, i + 3));
                i += 3;
                continue;
            }
            if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                starts.push((i, i + 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    starts
        .iter()
        .enumerate()
        .map(|(n, &(_, body))| {
            let end = starts.get(n + 1).map_or(data.len(), |&(next, _)| next);
            &data[body..end]
        })
        .collect()
}

/// The fixture, reshaped the way a mirroring sender ships it: the SPS/PPS as an
/// `AVCDecoderConfigurationRecord`, and each frame as an AVCC-framed access unit.
fn sender_form() -> (Vec<u8>, Vec<Vec<u8>>) {
    let nals = nal_units(H264);
    let mut sps = None;
    let mut pps = None;
    let mut units: Vec<Vec<Vec<u8>>> = Vec::new();
    for nal in nals {
        match nal[0] & 0x1f {
            7 => sps = Some(nal),
            8 => pps = Some(nal),
            // A slice NAL ends an access unit; anything else (the SEI) opens one.
            _ => {
                if units.last().is_none_or(|au| {
                    au.last()
                        .is_some_and(|last| matches!(last[0] & 0x1f, 1 | 5))
                }) {
                    units.push(Vec::new());
                }
                units.last_mut().unwrap().push(nal.to_vec());
            }
        }
    }
    let sps = sps.expect("the fixture carries an SPS");
    let pps = pps.expect("the fixture carries a PPS");

    // version, profile, compat, level from the SPS itself; 4-byte lengths; one of each.
    let mut record = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
    record.extend_from_slice(&u16::try_from(sps.len()).unwrap().to_be_bytes());
    record.extend_from_slice(sps);
    record.push(1);
    record.extend_from_slice(&u16::try_from(pps.len()).unwrap().to_be_bytes());
    record.extend_from_slice(pps);

    let avcc = units
        .into_iter()
        .map(|au| {
            let mut framed = Vec::new();
            for nal in au {
                framed.extend_from_slice(&u32::try_from(nal.len()).unwrap().to_be_bytes());
                framed.extend_from_slice(&nal);
            }
            framed
        })
        .collect();
    (record, avcc)
}

/// Mean absolute error between a decoded RGBA frame and one RGB24 source frame.
fn mean_error(rgba: &[u8], source: &[u8]) -> f64 {
    assert_eq!(rgba.len(), WIDTH * HEIGHT * 4);
    assert_eq!(source.len(), WIDTH * HEIGHT * 3);
    let mut total = 0u64;
    for px in 0..WIDTH * HEIGHT {
        for c in 0..3 {
            total += u64::from(rgba[px * 4 + c].abs_diff(source[px * 3 + c]));
        }
    }
    #[allow(clippy::cast_precision_loss)]
    {
        total as f64 / (WIDTH * HEIGHT * 3) as f64
    }
}

/// What a sender mirrors over a negotiated session decodes back to the frames the
/// encoder was given.
///
/// Every step is the shipped code's: the FairPlay vector drives the real key
/// derivation, the data port is the one the `SETUP` advertised, the session decrypts
/// and rewrites the frames, and libavcodec decodes exactly the bytes it delivered. The
/// bar is a mean pixel error under 2 against the encoder's own input — the whole path
/// (RGB→YUV, x264 at crf 10, decode, YUV→RGB) measures ≈1.4, while any framing or
/// keystream fault fails the decode outright rather than nudging the error.
#[tokio::test]
async fn a_mirrored_h264_pair_decodes_to_the_frames_the_encoder_was_given() {
    let (mut stream, mut events) = raop::start(MediaPorts::Ephemeral).await;
    let data_port = raop::negotiate_mirror(&mut stream).await;

    let mut frames = None;
    for _ in 0..8 {
        let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(5), events.recv()).await
        else {
            break;
        };
        if let SessionEvent::Mirror { video, .. } = msg.event {
            let FrameSource::Encoded(rx) = video else {
                panic!("expected encoded frames")
            };
            frames = Some(rx);
            break;
        }
    }
    let mut frames = frames.expect("SETUP should have started a mirroring session");

    // Encrypt with the key the real derivation must produce, one continuous keystream.
    let aes_key: [u8; 16] = raop::unhex(raop::FP_EXPECTED_AES_KEY).try_into().unwrap();
    let keys = MirrorKeys::derive(
        &aes_key,
        StreamConnectionId::from_plist_signed(raop::MIRROR_STREAM_ID),
    );
    let mut cipher = ctr::Ctr128BE::<aes::Aes128>::new(&keys.key.into(), &keys.iv.into());

    let (record, units) = sender_form();
    assert_eq!(units.len(), 2, "the fixture is an IDR + P pair");

    let mut data = tokio::net::TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], data_port)))
        .await
        .expect("the advertised data port accepts");

    #[allow(clippy::cast_precision_loss)]
    let encoded = (WIDTH as f32, HEIGHT as f32);
    // The codec config, then the IDR at the same timestamp, then the P a frame later.
    let mut out = raop::mirror_message(1, 7_000, &record, encoded);
    for (i, unit) in units.iter().enumerate() {
        let mut au = unit.clone();
        cipher.apply_keystream(&mut au);
        let ts = 7_000 + u64::try_from(i).unwrap() * 33_333_333;
        out.extend_from_slice(&raop::mirror_message(0, ts, &au, encoded));
    }
    data.write_all(&out).await.unwrap();
    data.flush().await.unwrap();

    let mut delivered: Vec<EncodedFrame> = Vec::new();
    for n in 0..units.len() {
        let frame = tokio::time::timeout(Duration::from_secs(5), frames.recv())
            .await
            .unwrap_or_else(|_| panic!("frame {n} never arrived"))
            .expect("the channel is open");
        delivered.push(frame);
    }
    assert!(delivered[0].keyframe, "the first access unit is an IDR");
    assert!(!delivered[1].keyframe, "the second is not");

    // Decode the delivered Annex-B with the shipped decoder. Blocking by design, so it
    // runs off the runtime (rule 4), exactly as `pipeline` runs it in production.
    let decoded = tokio::task::spawn_blocking(move || {
        let mut input = delivered.into_iter();
        let mut out = Vec::new();
        pipeline::ffmpeg_decode::decode_stream(
            VideoCodec::H264,
            HwPreference::SoftwareOnly,
            || input.next(),
            |frame| {
                out.push(frame);
                true
            },
        )
        .expect("the delivered stream decodes");
        out
    })
    .await
    .unwrap();

    assert_eq!(
        decoded.len(),
        2,
        "two access units in, two pictures out — a decoder refusing one is a delivery \
         fault, not a codec being a codec"
    );
    for (n, frame) in decoded.iter().enumerate() {
        // The negotiated geometry, straight from the SPS the session re-framed in-band.
        assert_eq!(
            (frame.width, frame.height),
            (
                u32::try_from(WIDTH).unwrap(),
                u32::try_from(HEIGHT).unwrap()
            ),
            "frame {n} geometry"
        );
        let FrameImage::Cpu { format, data } = &frame.image else {
            panic!("software decode yields CPU frames")
        };
        assert_eq!(*format, PixelFormat::Rgba8);
        let source = &SOURCE_RGB[n * WIDTH * HEIGHT * 3..(n + 1) * WIDTH * HEIGHT * 3];
        let error = mean_error(data, source);
        assert!(
            error < 2.0,
            "frame {n} differs from the encoder's input by a mean of {error:.2} per \
             channel; the encode/decode round trip alone measures ~1.4, so anything \
             near 2 is the join corrupting the stream"
        );
    }
}
