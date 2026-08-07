//! Does the mixer drain a 44.1 kHz source in real time against a *real* sound card?
//!
//! `mixer::tests::a_source_that_needs_resampling_is_drained_in_real_time` asks this with a
//! test device whose `frames_played` is derived from the wall clock — so that counter is
//! correct by construction, and the test is structurally unable to fail on it. This is the
//! same measurement with that assumption removed, which is the only difference between the
//! two and therefore the whole point of having both.
//!
//! ## Where this runs (#204)
//!
//! `checks.mixer-vm` — a nixosTest that boots `snd-dummy`, a kernel sound card whose
//! period wakeups come from an hrtimer, and runs this file with `--include-ignored`. See
//! `nix/mixer-vm-test.nix` for what a dummy card does and does not buy. It also runs
//! against whatever this box has: `cargo nextest run -p pipeline --run-ignored all -E
//! 'binary(mixer_real_device)'`.
//!
//! **The `#[ignore]` stays, and that is deliberate.** It was written as "hardware today"
//! and the header used to say it would come off when the VM landed. It must not:
//! `checks.test` builds in a sandbox with no session bus, no PipeWire and no card, so
//! without the attribute these two would be a pair of tests that can only fail there. The
//! attribute is what lets one file be dark in one check and the whole subject of another.
//!
//! Why #204 treats this as its headline rather than a nice-to-have: it is the only thing
//! in the tree that removes the correct-by-construction assumption underneath **every**
//! mixer pacing test, and the three live regressions in that blind spot (#174, #175,
//! #177) were all found by a person listening to the panel.
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
use pipeline::mixer::{AudioMixer, MixTap, MixerCounters, CHANNELS, RATE};

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

    let before = mixer.counters();
    let started = Instant::now();
    let mut accepted = 0u64;
    while started.elapsed() < WINDOW {
        input.write(&block).unwrap();
        accepted += BLOCK as u64;
    }
    let elapsed = started.elapsed();
    let window = mixer.counters().since(&before);

    let rate = accepted as f64 / elapsed.as_secs_f64();
    let share = rate / f64::from(SOURCE_RATE);
    println!(
        "accepted {rate:.0} frames/s of {SOURCE_RATE} ({:.1}%); {window:?}",
        share * 100.0
    );
    assert!(
        share > 0.97,
        "the input accepted {rate:.0} frames/s of a {SOURCE_RATE} Hz source ({:.1}%); \
         a live sender cannot be slowed down, so its queue fills at the difference and \
         then never drains again — a permanent drop rate and a permanent latency floor",
        share * 100.0
    );
    invented_is_negligible(&window);
    emission_is_real_time(&window, elapsed);
}

/// The share of the device's frames the mixer made up, asserted rather than inferred.
///
/// A saturating source cannot legitimately starve the mixer: it always has more, so every
/// frame counted here was invented at the floor against an input that was mid-stream and
/// had nothing to give. #175 is what that looks like at a third, and the reason this is a
/// *proportion* and not a duration is that the two tests which caught it first
/// (`ldac_decode`, `output_stream`) could only assert on how long a fixture took to play
/// — so they failed when the run was unlucky enough to push the span past a fixed band,
/// rather than in proportion to the defect.
fn invented_is_negligible(window: &MixerCounters) {
    let invented = window
        .invented()
        .expect("the mixer emitted nothing at all over the window");
    assert!(
        invented < 0.01,
        "{:.1}% of what the device was given was silence the mixer invented \
         ({} of {} frames); the source never ran out, so this is the mixer failing to \
         take what was there rather than a gap in the audio",
        invented * 100.0,
        window.starved,
        window.emitted,
    );
}

/// Did the mixer emit at real time *against the device's own counter*?
///
/// This is the term every in-crate pacing test holds correct by construction, and the
/// whole reason this file exists: `mixer::tests`' `Recorder` derives `frames_played` from
/// the wall clock, so a device whose clock ran fast or slow could not fail one of them.
///
/// The band is ±5% rather than #204's proposed 1% for two reasons, and the second is the
/// one worth knowing. The first is that this runs in a VM on a shared CI box, and a band
/// that goes red under load is a band that stops being believed (#156).
///
/// The second is that the measurement has a **systematic offset of about +2.4%**, and it
/// is not drift. `emitted` is sampled at two instants that fall wherever they fall inside
/// a pass, and the mixer runs [`DEVICE_LEAD`](pipeline::mixer) ahead of the speakers — so
/// a three-second window catches roughly one lead's worth of frames the device has been
/// given but has not played. Measured at 102.4% on the dev box's real card and 102.4% in
/// the VM's `snd-dummy`, which is what says it is the window and not either clock: two
/// devices with nothing in common read the same figure to a tenth of a percent.
///
/// So the effective band is about −7%/+2.6%, and the check is still sharp in the
/// direction that matters. A device counter running slow throttles emission
/// proportionally — the #175 reading was a third — and nothing near that survives here.
fn emission_is_real_time(window: &MixerCounters, elapsed: Duration) {
    #[allow(clippy::cast_precision_loss)]
    let emission = window.emitted as f64 / elapsed.as_secs_f64() / f64::from(RATE);
    assert!(
        (0.95..=1.05).contains(&emission),
        "the mixer emitted at {:.1}% of real time against the device's own counter; \
         that counter and the wall disagree, which every input-side figure would miss",
        emission * 100.0
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
    let before = mixer.counters();
    let started = Instant::now();
    std::thread::sleep(WINDOW);
    let elapsed = started.elapsed();
    let heard = tap.0.lock().unwrap()[from..].to_vec();
    let window = mixer.counters().since(&before);
    stop.store(true, Ordering::Relaxed);
    producer.join().unwrap();

    let frames = heard.chunks_exact(usize::from(CHANNELS));
    let total = frames.len();
    let audible = frames.filter(|f| f.iter().any(|s| *s != 0.0)).count();
    let carried = audible as f64 / total as f64;
    let emission = total as f64 / elapsed.as_secs_f64() / f64::from(RATE);
    println!(
        "{audible}/{total} device frames carried audio ({:.1}%); \
         emission at {:.1}% of real time; {window:?}",
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

    // The same two properties from the mixer's own books rather than the tap's. They are
    // worth having twice because they fail differently: the tap can only see zeros, and
    // zeros the *source* sent look identical to zeros the mixer made up. These name
    // which it was.
    invented_is_negligible(&window);
    emission_is_real_time(&window, elapsed);
    assert_eq!(
        window.shed, 0,
        "the mixer shed {} frames from a source delivering at exactly real time; \
         `Backpressure::Live` sheds when a sender runs past its budget, so a sender that \
         is not running ahead being shed means the drain is below real time — which is \
         #177's signature, from the other side",
        window.shed
    );
}
