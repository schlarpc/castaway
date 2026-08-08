//! The seam between the AirPlay protocol half and the decode half (#189).
//!
//! `raop_session.rs` proves the session machinery — negotiation, sockets, delivery —
//! with payloads that are ASCII placeholders, so it proves the bytes we sent came back
//! out and cannot fail on media that is undecodable. The decode tests in
//! `pipeline::audio_decode` prove ALAC round-trips through frames built by hand. Nothing
//! joined them: a magic cookie built wrong from the SDP, or a delivery path that
//! reorders, truncates or re-frames, produces a green journal and static.
//!
//! This is that join, in `app` because it is the only crate depending on both —
//! `proto-airplay` has no decoder and `pipeline` has no RTSP session, which is the
//! layering working (rule 2) and exactly why the gap existed. The harness is
//! `proto-airplay`'s own, reached by `#[path]` the way the Bluetooth seam test (#187)
//! reaches its fixtures: one copy, or the session driven here drifts from the one
//! proven there.
#![cfg(feature = "audio")]
#![allow(clippy::unwrap_used)]
// Tests bind ephemeral loopback sockets that never face the LAN; the registry
// (crates/app/src/surface.rs) governs production binds.
#![allow(clippy::disallowed_methods)]

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use castaway_core::{AudioCodec, FrameSource, MediaPorts, SessionEvent};
use pipeline::audio_decode::AudioDecoder;
use tokio::net::UdpSocket;

#[path = "../../proto-airplay/tests/raop_harness/mod.rs"]
mod raop;

/// Real ALAC: a one-second two-tone (440 Hz left, 1 kHz right, 0.7 of full scale)
/// encoded by ffmpeg's ALAC encoder, one length-prefixed record per packet.
///
/// In `proto-airplay` rather than here for the same reason the iPhone AAC capture
/// stays in `proto-bluetooth-audio`: it is AirPlay wire media, and two copies would
/// drift. Regenerate with:
///
/// ```text
/// ffmpeg -f lavfi -i "aevalsrc=0.7*sin(440*2*PI*t)|0.7*sin(1000*2*PI*t):s=44100:d=1" \
///        -c:a alac -sample_fmt s16p tone.m4a
/// ffprobe -show_packets -show_data -of json tone.m4a  # hex → length-prefixed records
/// ```
const ALAC_PACKETS: &[u8] =
    include_bytes!("../../proto-airplay/tests/fixtures/raop-alac-tone-44100-stereo.bin");

/// The `a=fmtp:` integers describing the fixture, copied field-for-field from the
/// encoder's own extradata (`ffprobe -show_streams -show_data`). Declaring exactly
/// these makes the cookie the session builds from the SDP byte-identical to the one
/// the encoder wrote — so a decode failure indicts the join, not the fixture.
///
/// They differ from a phone's (`raop::IPHONE_FMTP`) in frame length — ffmpeg's encoder
/// is fixed at 4096 samples per frame where an iPhone sends 352 — which is the field
/// the mutation check on `pipeline::audio_decode`'s ALAC test proved is load-bearing:
/// a cookie declaring the wrong frame length decodes to *no audio and no error*.
const FIXTURE_FMTP: &str = "4096 0 16 40 10 14 2 0 16388 1411200 44100";

/// Samples per channel in the fixture: one second exactly, the last frame partial.
const FIXTURE_SAMPLES: usize = 44_100;

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

/// The best normalised cross-correlation over lags in `0..=max_lag`.
///
/// A lag search because the decoder has priming latency of its own and that is not what
/// is under test; that a single alignment explains the whole signal is.
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

/// One channel's reference tone, matching what `aevalsrc` was asked for.
fn tone(frequency: f32, samples: usize) -> Vec<f32> {
    #[allow(clippy::cast_precision_loss)]
    (0..samples)
        .map(|n| (n as f32 / 44_100.0 * frequency * std::f32::consts::TAU).sin() * 0.7)
        .collect()
}

/// Apply the RAOP audio-packet encryption rule, as a sender would: AES-128-CBC over
/// whole blocks only, ragged tail in the clear, and a fresh cipher per packet — which
/// *is* the per-packet IV reset the receiver's decrypt side mirrors.
fn raop_encrypt(key: &[u8; 16], iv: &[u8; 16], payload: &[u8]) -> Vec<u8> {
    use aes::cipher::{BlockEncryptMut as _, KeyIvInit as _};
    let mut out = payload.to_vec();
    let n = out.len() - (out.len() % 16);
    if n == 0 {
        return out;
    }
    let mut enc = cbc::Encryptor::<aes::Aes128>::new(key.into(), iv.into());
    let chunks = aes::cipher::inout::InOutBuf::from(&mut out[..n])
        .into_chunks::<aes::cipher::consts::U16>()
        .0
        .into_out();
    enc.encrypt_blocks_mut(chunks);
    out
}

/// Negotiate a session with `sdp`, stream the fixture (encrypting each packet when the
/// SDP promised to), and return what the pipeline's decoder makes of what came out.
///
/// Every step is the shipped code's: the negotiation builds the magic cookie from the
/// SDP, the UDP socket the SETUP advertised receives real RTP, the session delivers
/// `EncodedFrame`s, and libavcodec decodes them with the delivered config.
async fn stream_and_decode(sdp: String, encrypt: Option<([u8; 16], [u8; 16])>) -> Vec<f32> {
    if !pipeline::audio_decode::can_decode(AudioCodec::Alac) {
        // Not a skip this build may take quietly: ALAC is what every iPhone sends first.
        panic!("this build advertises AirPlay and cannot decode ALAC");
    }

    let (mut stream, mut events) = raop::start(MediaPorts::Ephemeral).await;
    let (audio_port, _record) = raop::negotiate_with_sdp(&mut stream, &sdp).await;

    // The Audio event carries the format and config the *session* derived — the exact
    // bytes the app would hand the pipeline, which is the seam under test.
    let mut audio = None;
    for _ in 0..8 {
        let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(5), events.recv()).await
        else {
            break;
        };
        if let SessionEvent::Audio {
            source,
            format,
            config,
        } = msg.event
        {
            let FrameSource::Encoded(rx) = source else {
                panic!("expected encoded frames")
            };
            audio = Some((rx, format, config));
            break;
        }
    }
    let (mut frames, format, config) = audio.expect("RECORD should have started audio");
    assert_eq!(format.sample_rate(), 44_100);
    assert_eq!(format.channels(), 2);
    let config = config.expect("ALAC must carry its magic cookie");

    // The packets, over the socket the SETUP advertised.
    let packets = records(ALAC_PACKETS);
    assert_eq!(packets.len(), 11, "one second at 4096 samples per frame");
    let sender = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let target = SocketAddr::from(([127, 0, 0, 1], audio_port));
    for (i, payload) in packets.iter().enumerate() {
        let payload = match &encrypt {
            Some((key, iv)) => raop_encrypt(key, iv, payload),
            None => payload.to_vec(),
        };
        #[allow(clippy::cast_possible_truncation)]
        let packet = raop::audio_packet(i as u16, i as u32 * 4096, &payload);
        sender.send_to(&packet, target).await.unwrap();
    }

    // Decode every delivered frame with the delivered config.
    let mut decoder = AudioDecoder::new(AudioCodec::Alac, format, Some(&config)).unwrap();
    let mut decoded: Vec<f32> = Vec::new();
    for n in 0..packets.len() {
        let frame = tokio::time::timeout(Duration::from_secs(5), frames.recv())
            .await
            .unwrap_or_else(|_| panic!("frame {n} never arrived"))
            .expect("the channel is open");
        decoder
            .decode(&frame, |block| decoded.extend_from_slice(&block.samples))
            .unwrap_or_else(|e| panic!("frame {n} was delivered and did not decode: {e}"));
    }
    decoder
        .flush(|block| decoded.extend_from_slice(&block.samples))
        .unwrap();
    decoded
}

/// What a sender streams over a negotiated RAOP session becomes the tone it encoded.
///
/// The bar is 0.999 per channel because ALAC is lossless: anything short of near-unity
/// is a framing error somewhere in the join, not a codec being a codec. The two
/// channels carry different tones, so a swap anywhere in the path scores ~0 rather
/// than passing on symmetry.
#[tokio::test]
async fn what_a_sender_streams_over_raop_becomes_the_tone_it_encoded() {
    let decoded = stream_and_decode(raop::announce_sdp(FIXTURE_FMTP), None).await;

    // Lossless means exact: every sample the encoder was given comes out, none invented.
    assert_eq!(
        decoded.len() / 2,
        FIXTURE_SAMPLES,
        "one second in, one second out — a partial final frame that pads to 4096 or a \
         dropped packet both change this count"
    );

    let left: Vec<f32> = decoded.iter().step_by(2).copied().collect();
    let right: Vec<f32> = decoded.iter().skip(1).step_by(2).copied().collect();
    for (name, channel, frequency) in [("left", &left, 440.0), ("right", &right, 1000.0)] {
        let score = best_correlation(&tone(frequency, FIXTURE_SAMPLES), channel, 44_100 / 50);
        assert!(
            score >= 0.999,
            "the {name} channel correlates {score:.4} with its {frequency} Hz tone; ALAC \
             is lossless, so anything short of near-unity is a framing error in the join"
        );
    }
}

/// The same session encrypted the way an iPhone encrypts it decodes to the same PCM,
/// bit for bit.
///
/// The key rides in `a=rsaaeskey:` wrapped with the AirPort public half (RSA-OAEP over
/// SHA-1 — the real derivation, not a test double), the IV in `a=aesiv:`, and each
/// packet is AES-CBC over whole blocks with the tail in the clear. Bit-identity with
/// the clear run is the whole assertion: the decrypt path must be a no-op on the
/// *audio*, and any block it misses, double-decrypts, or chains across packets decodes
/// to different samples — loudly, because ALAC framing breaks — or to none.
#[tokio::test]
async fn an_encrypted_session_decodes_to_the_same_pcm_as_the_clear_one() {
    if !crypto_raop::has_airport_key() {
        // The AirPort key is carved at build time rather than checked in, so a build
        // without it cannot exercise the RSA path. `nix flake check` always has it.
        eprintln!("skipping: this build has no AirPort key");
        return;
    }
    use base64::Engine as _;
    let key = *b"0123456789abcdef";
    let iv = *b"ABCDEFGHIJKLMNOP";
    let wrapped = crypto_raop::airport_public_key()
        .unwrap()
        .encrypt(
            &mut rsa::rand_core::OsRng,
            rsa::Oaep::new::<sha1::Sha1>(),
            &key,
        )
        .unwrap();
    let b64 = base64::engine::general_purpose::STANDARD_NO_PAD;
    let sdp = format!(
        "{}a=rsaaeskey:{}\r\na=aesiv:{}\r\n",
        raop::announce_sdp(FIXTURE_FMTP),
        b64.encode(&wrapped),
        b64.encode(iv)
    );

    let encrypted = stream_and_decode(sdp, Some((key, iv))).await;
    let clear = stream_and_decode(raop::announce_sdp(FIXTURE_FMTP), None).await;

    assert_eq!(
        encrypted.len(),
        clear.len(),
        "the encrypted session decoded a different amount of audio than the clear one"
    );
    if let Some(at) = (0..clear.len()).find(|&i| encrypted[i].to_bits() != clear[i].to_bits()) {
        panic!(
            "decryption is not transparent: sample {at} is {} decrypted vs {} clear",
            encrypted[at], clear[at]
        );
    }
}
