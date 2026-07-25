//! The audio session: encoded frames in one end, sound out the other.
//!
//! Joins [`crate::audio_decode`] to [`crate::audio_out`] on a dedicated OS thread, for
//! the same reason video decode gets one — decoding blocks, and a blocked runtime worker
//! stalls every protocol adapter sharing it (ground rule 4).
//!
//! The codec is not a parameter. It arrives on the first frame, because the A2DP adapter
//! already negotiated it and stamps every [`EncodedFrame`] with it, and taking it from
//! the stream rather than from a second configuration path removes the chance of the two
//! disagreeing.

use castaway_core::{AudioCodec, EncodedFrame};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::audio_decode::{decode_audio_stream, AudioStreamFormat};
use crate::audio_out::AudioOut;
#[cfg(any(test, not(feature = "audio-out")))]
use crate::audio_out::NullAudioOut;

/// The output a session uses when the caller expresses no preference.
///
/// Real device when the `audio-out` feature is on, accounting-only otherwise — so a
/// headless CI box runs the whole path and a kiosk makes noise, from the same code.
#[must_use]
pub fn default_output() -> Box<dyn AudioOut> {
    #[cfg(feature = "audio-out")]
    {
        Box::new(crate::audio_out::CpalAudioOut::new())
    }
    #[cfg(not(feature = "audio-out"))]
    {
        Box::new(NullAudioOut::new())
    }
}

/// Run a decode → output session on a dedicated thread until the source ends.
pub fn spawn(frames: mpsc::Receiver<EncodedFrame>, output: Box<dyn AudioOut>) {
    std::thread::spawn(move || run(frames, output));
}

/// Drive one audio session to completion. Blocking; call it on its own thread.
pub fn run(mut frames: mpsc::Receiver<EncodedFrame>, mut output: Box<dyn AudioOut>) {
    // Wait for the first frame so the codec and rate come from the stream itself.
    let Some(first) = frames.blocking_recv() else {
        return;
    };
    let Some(codec) = first.audio_codec else {
        warn!("audio session: first frame names no codec");
        return;
    };

    // aptX and aptX HD carry no header, so the decoder must be told the format. The
    // adapter negotiated it; until that is threaded through as configuration, the
    // A2DP-universal default is the right guess and a wrong one is audible immediately
    // (the stream plays at the wrong pitch) rather than silently wrong.
    let format = AudioStreamFormat::default();

    let mut started = false;
    let mut pending = Some(first);

    let result = decode_audio_stream(
        codec,
        format,
        || pending.take().or_else(|| frames.blocking_recv()),
        |block| {
            if !started {
                if let Err(e) = output.start(block.sample_rate, block.channels) {
                    warn!(error = %e, "audio session: output refused the stream");
                    return false;
                }
                info!(
                    ?codec,
                    rate = block.sample_rate,
                    channels = block.channels,
                    "audio session: playing"
                );
                started = true;
            }
            match output.write(&block) {
                Ok(()) => true,
                Err(e) => {
                    warn!(error = %e, "audio session: output failed");
                    false
                }
            }
        },
    );

    if let Err(e) = result {
        // The most likely cause is a codec we advertised but cannot decode, which is
        // the failure Q22 exists to prevent — so name it loudly.
        warn!(error = %e, ?codec, "audio session ended with an error");
        crate::audio_decode::warn_undecodable(codec);
    }
    output.stop();
}

/// Which codecs this build can actually decode.
///
/// The adapter's endpoint table must be built from this. Advertising a codec we cannot
/// decode means the sender picks it and the session is silence rather than a clean
/// fallback to one we can (Q22).
#[must_use]
pub fn decodable_codecs() -> Vec<AudioCodec> {
    [
        AudioCodec::Ldac,
        AudioCodec::AptXHd,
        AudioCodec::AptX,
        AudioCodec::Aac,
        AudioCodec::Sbc,
    ]
    .into_iter()
    .filter(|c| crate::audio_decode::can_decode(*c))
    .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_session_with_no_frames_exits_instead_of_parking() {
        // The phone connected and never sent anything. The thread must not leak.
        let (tx, rx) = mpsc::channel::<EncodedFrame>(1);
        drop(tx);
        run(rx, Box::new(NullAudioOut::new()));
    }

    #[test]
    fn a_frame_without_a_codec_is_refused_rather_than_guessed() {
        let (tx, rx) = mpsc::channel(1);
        tx.blocking_send(EncodedFrame {
            video_codec: None,
            audio_codec: None,
            pts: Duration::ZERO,
            keyframe: true,
            data: bytes::Bytes::from_static(&[0; 8]),
        })
        .unwrap();
        drop(tx);
        // Guessing SBC here would decode noise; exiting is the honest answer.
        run(rx, Box::new(NullAudioOut::new()));
    }

    #[test]
    fn sbc_is_always_decodable_because_every_sender_falls_back_to_it() {
        // If this ever fails, the endpoint table loses its guaranteed floor and some
        // phone ends up with no codec in common with us at all.
        let codecs = decodable_codecs();
        assert!(
            codecs.contains(&AudioCodec::Sbc),
            "SBC must be decodable; got {codecs:?}"
        );
    }

    #[test]
    fn ldac_appears_only_when_its_feature_is_built() {
        let has_ldac = decodable_codecs().contains(&AudioCodec::Ldac);
        assert_eq!(
            has_ldac,
            cfg!(feature = "ldac"),
            "the advertised table must follow the build, not optimism (Q22)"
        );
    }
}
