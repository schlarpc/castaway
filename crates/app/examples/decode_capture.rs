//! Decode a captured A2DP packet fixture to PCM, and say whether it looks like audio.
//!
//! The seam test in `tests/bluetooth_audio_decodes.rs` asserts that a fixture decodes;
//! this is for the times somebody needs to *look* at what came out — or listen to it —
//! against a capture straight off a phone, where there is no reference tone to correlate
//! with and "did it decode" and "is it the music" are different questions.
//!
//! ```text
//! cargo run -p castaway --example decode_capture -- \
//!     crates/proto-bluetooth-audio/tests/fixtures/a2dp-ldac-96000-android.bin 96000 out.wav
//! ```

use bytes::Bytes;
use castaway_core::{AudioCodec, AudioFormat};
use pipeline::audio_decode::AudioDecoder;
use proto_bluetooth_audio::Depacketizer;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or("usage: decode_capture <fixture> [rate] [out.wav]")?;
    let rate: u32 = args.next().unwrap_or_else(|| "96000".into()).parse()?;
    let wav = args.next();

    let data = std::fs::read(&path)?;
    let mut packets = Vec::new();
    let mut at = 0usize;
    while at + 4 <= data.len() {
        let len = u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize;
        at += 4;
        if at + len > data.len() {
            break;
        }
        packets.push(Bytes::copy_from_slice(&data[at..at + len]));
        at += len;
    }
    println!("{} packets from {path}", packets.len());

    let mut depacketizer = Depacketizer::new(AudioCodec::Ldac, rate);
    let mut decoder = AudioDecoder::new(
        AudioCodec::Ldac,
        AudioFormat::from_hz(rate, 2).ok_or("bad format")?,
        None,
    )?;

    let mut pcm: Vec<f32> = Vec::new();
    for (n, packet) in packets.iter().enumerate() {
        let frame = depacketizer
            .push(packet.clone())
            .map_err(|e| format!("packet {n} did not depacketise: {e}"))?;
        decoder
            .decode(&frame, |b| pcm.extend_from_slice(&b.samples))
            .map_err(|e| format!("packet {n} did not decode: {e}"))?;
    }
    decoder.flush(|b| pcm.extend_from_slice(&b.samples))?;

    let frames = pcm.len() / 2;
    #[allow(clippy::cast_precision_loss)]
    let seconds = frames as f32 / rate as f32;
    println!("decoded {frames} sample frames ({seconds:.2}s at {rate} Hz, stereo)");
    println!("  samples per packet: {}", frames / packets.len().max(1));

    for (name, channel) in [
        ("left", pcm.iter().step_by(2).copied().collect::<Vec<_>>()),
        ("right", pcm.iter().skip(1).step_by(2).copied().collect()),
    ] {
        let peak = channel.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        #[allow(clippy::cast_precision_loss)]
        let rms = (channel.iter().map(|s| s * s).sum::<f32>() / channel.len() as f32).sqrt();
        #[allow(clippy::cast_precision_loss)]
        let dc = channel.iter().sum::<f32>() / channel.len() as f32;
        let clipped = channel.iter().filter(|s| s.abs() >= 0.999).count();
        let silent = channel.iter().filter(|s| s.abs() < 1e-6).count();
        #[allow(clippy::cast_precision_loss)]
        let silent_pct = 100.0 * silent as f32 / channel.len() as f32;
        println!(
            "  {name:5}: peak {peak:.4} ({:.1} dBFS)  rms {rms:.4} ({:.1} dBFS)  dc {dc:+.5}  clipped {clipped}  silent {silent_pct:.1}%",
            20.0 * peak.max(1e-9).log10(),
            20.0 * rms.max(1e-9).log10(),
        );
    }

    if let Some(out) = wav {
        write_wav(&out, &pcm, rate)?;
        println!("wrote {out}");
    }
    Ok(())
}

/// A 16-bit stereo WAV, so the result can be played by anything.
fn write_wav(path: &str, pcm: &[f32], rate: u32) -> std::io::Result<()> {
    use std::io::Write as _;
    let bytes = u32::try_from(pcm.len() * 2).unwrap_or(u32::MAX);
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + bytes).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&2u16.to_le_bytes())?; // stereo
    f.write_all(&rate.to_le_bytes())?;
    f.write_all(&(rate * 4).to_le_bytes())?; // byte rate
    f.write_all(&4u16.to_le_bytes())?; // block align
    f.write_all(&16u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&bytes.to_le_bytes())?;
    for s in pcm {
        #[allow(clippy::cast_possible_truncation)]
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        f.write_all(&v.to_le_bytes())?;
    }
    f.flush()
}
