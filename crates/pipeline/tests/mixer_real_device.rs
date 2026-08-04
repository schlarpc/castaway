//! Does the mixer drain a 44.1 kHz source in real time against a *real* sound card?
//!
//! `mixer::tests::a_source_that_needs_resampling_is_drained_in_real_time` asks this with a
//! test device whose `frames_played` is derived from the wall clock — so that counter is
//! correct by construction, and the test is structurally unable to fail on it. This is the
//! same measurement with that assumption removed, which is the only difference between the
//! two and therefore the whole point of having both.
//!
//! Hardware, so `#[ignore]` per ground rule 6: it needs a session bus, a running PipeWire
//! and a sink to open. Run it with `--run-ignored all`.
//!
//! The tone is at -80 dBFS. It has to be non-zero to prove samples are flowing, and it has
//! to be inaudible because the panel this runs on is usually playing something.

#![cfg(all(feature = "audio", feature = "audio-pipewire"))]
#![allow(clippy::unwrap_used)]

use std::time::{Duration, Instant};

use pipeline::audio_decode::PcmBlock;
use pipeline::audio_out::output_factory;
use pipeline::audio_select::{OutputSelection, OutputSelector};
use pipeline::mixer::{AudioMixer, CHANNELS};

/// A2DP's rate, and the one that needs conversion to the mix rate.
const SOURCE_RATE: u32 = 44_100;
/// One AAC frame, which is what an A2DP packet carries.
const BLOCK: usize = 1024;
/// Long enough to fill the budget and open the device, so what follows is steady state.
const WARMUP: Duration = Duration::from_millis(1500);
const WINDOW: Duration = Duration::from_millis(3000);
/// -80 dBFS. Audible to the assertion, not to the room.
const AMPLITUDE: f32 = 1e-4;

#[test]
#[ignore = "needs a real PipeWire sink"]
fn a_44_1k_source_is_drained_in_real_time_by_a_real_device() {
    let selector = OutputSelector::new(OutputSelection::SystemDefault);
    let mixer = AudioMixer::new(output_factory(selector));
    let mut input = mixer.input();
    input.format(SOURCE_RATE, CHANNELS).unwrap();
    let block = PcmBlock {
        sample_rate: SOURCE_RATE,
        channels: CHANNELS,
        samples: vec![AMPLITUDE; BLOCK * usize::from(CHANNELS)],
        pts: Duration::ZERO,
    };

    let warm_until = Instant::now() + WARMUP;
    while Instant::now() < warm_until {
        input.write(&block).unwrap();
    }

    let started = Instant::now();
    let mut accepted = 0u64;
    while started.elapsed() < WINDOW {
        input.write(&block).unwrap();
        accepted += BLOCK as u64;
    }
    let elapsed = started.elapsed();

    let rate = accepted as f64 / elapsed.as_secs_f64();
    let share = rate / f64::from(SOURCE_RATE);
    println!(
        "accepted {rate:.0} frames/s of {SOURCE_RATE} ({:.1}%)",
        share * 100.0
    );
    assert!(
        share > 0.97,
        "the input accepted {rate:.0} frames/s of a {SOURCE_RATE} Hz source ({:.1}%); \
         a live sender cannot be slowed down, so its queue fills at the difference and \
         then never drains again — a permanent drop rate and a permanent latency floor",
        share * 100.0
    );
}
