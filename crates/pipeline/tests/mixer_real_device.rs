//! Does the mixer drain a 44.1 kHz source in real time against a *real* sound card?
//!
//! `mixer::tests::a_source_that_needs_resampling_is_drained_in_real_time` asks this with a
//! test device whose `frames_played` is derived from the wall clock — so that counter is
//! correct by construction, and the test is structurally unable to fail on it. This is the
//! same measurement with that assumption removed, which is the only difference between the
//! two and therefore the whole point of having both.
//!
//! Hardware today, so `#[ignore]` per ground rule 6: it needs a session bus, a running
//! PipeWire and a sink to open. Run it with `--run-ignored all`.
//!
//! ## The decision, and when it gets revisited (#183)
//!
//! **This should run in CI, and does not need hardware to.** That is the one call among
//! the five `#[ignore]`d files that goes against the file's own header: a nixosTest with a
//! dummy PipeWire or ALSA sink gives a device whose clock is not derived from ours, which
//! is the entire property under test. `bluetooth-vm` already boots a `services.pipewire`
//! block for a `tester` user, so the recipe is nearly written.
//!
//! Until that exists this is the only thing in the tree that removes the
//! correct-by-construction assumption underneath **every** mixer pacing test, and it runs
//! nowhere — which is why #204 treats it as the headline rather than as a nice-to-have,
//! and why the three live regressions in that blind spot (#174, #175, #177) were all found
//! by a person listening to the panel.
//!
//! Revisit: when #204's nixosTest lands. Then the `#[ignore]` comes off and this file is
//! the check.
//!
//! The tone is at -80 dBFS. It has to be non-zero to prove samples are flowing, and it has
//! to be inaudible because the panel this runs on is usually playing something.

#![cfg(all(feature = "audio", feature = "audio-pipewire"))]
#![allow(clippy::unwrap_used)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pipeline::audio_decode::PcmBlock;
use pipeline::audio_out::output_factory;
use pipeline::audio_select::{OutputSelection, OutputSelector};
use pipeline::mixer::{AudioMixer, MixTap, CHANNELS, RATE};

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
    let mut input = mixer.input(pipeline::mixer::Backpressure::Pull);
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

/// The saturating test above is flow-controlled by the thing it measures, so it can only
/// see the average rate. This is the honest producer #175 demanded — one on its own
/// deadline clock, delivering in packets at AirPlay's ALAC cadence — against the *real*
/// device counter, which is the one term the in-crate tests hold correct by construction.
///
/// A tap records exactly what the device was given, which catches both ways the device's
/// clock can lie: a counter running fast drains the ring ahead of arrival and the output
/// grows holes (audible share falls); a counter running slow throttles emission below
/// real time (the tap's frame rate falls, and a live ring sheds against it).
#[test]
#[ignore = "needs a real PipeWire sink"]
fn a_packet_source_on_its_own_clock_survives_a_real_device() {
    /// One ALAC packet, as AirPlay sends it.
    const PACKET: usize = 352;

    #[derive(Default)]
    struct Collect(Mutex<Vec<f32>>);
    impl MixTap for Collect {
        fn mixed(&self, _at: Instant, stereo: &[f32]) {
            self.0.lock().unwrap().extend_from_slice(stereo);
        }
    }

    let selector = OutputSelector::new(OutputSelection::SystemDefault);
    let mixer = AudioMixer::new(output_factory(selector));
    let tap = Arc::new(Collect::default());
    mixer.add_tap(Arc::clone(&tap) as Arc<dyn MixTap>);
    let mut input = mixer.input(pipeline::mixer::Backpressure::Live);
    input.format(SOURCE_RATE, CHANNELS).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let producer = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let block = PcmBlock {
                sample_rate: SOURCE_RATE,
                channels: CHANNELS,
                samples: vec![AMPLITUDE; PACKET * usize::from(CHANNELS)],
                pts: Duration::ZERO,
            };
            let period =
                Duration::from_nanos(1_000_000_000u64 * PACKET as u64 / u64::from(SOURCE_RATE));
            let mut next = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                input.write(&block).unwrap();
                next += period;
                if let Some(wait) = next.checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
            }
        })
    };

    std::thread::sleep(WARMUP);
    let from = tap.0.lock().unwrap().len();
    let started = Instant::now();
    std::thread::sleep(WINDOW);
    let elapsed = started.elapsed();
    let heard = tap.0.lock().unwrap()[from..].to_vec();
    stop.store(true, Ordering::Relaxed);
    producer.join().unwrap();

    let frames = heard.chunks_exact(usize::from(CHANNELS));
    let total = frames.len();
    let audible = frames.filter(|f| f.iter().any(|s| *s != 0.0)).count();
    let carried = audible as f64 / total as f64;
    let emission = total as f64 / elapsed.as_secs_f64() / f64::from(RATE);
    println!(
        "{audible}/{total} device frames carried audio ({:.1}%); emission at {:.1}% of real time",
        carried * 100.0,
        emission * 100.0
    );
    assert!(
        carried > 0.97,
        "only {:.1}% of what the device was given carried audio against a real-time \
         packet source; the device's clock is running ahead of the room, or the drain is \
         inventing silence it does not need",
        carried * 100.0
    );
    assert!(
        (0.95..=1.05).contains(&emission),
        "the mixer emitted at {:.1}% of real time against the device's own counter; \
         that counter and the wall disagree, which every input-side figure would miss",
        emission * 100.0
    );
}
