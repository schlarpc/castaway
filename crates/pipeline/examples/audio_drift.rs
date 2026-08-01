//! How fast the audio device's clock actually runs, in ppm against `CLOCK_MONOTONIC`.
//!
//! Stage 0 of #111: which clock the output stream's audio track should be on. If the
//! device's 48 kHz is really 48 000, the question is moot; if it is 48 002, the stream's
//! audio drifts against its wall-clock video by 40 ms an hour and something has to
//! reconcile them.
//!
//! ## Why this is not two timestamps far apart
//!
//! It cannot be read off the stream at all: `stream::audio` places blocks by wall clock
//! *by construction*, so the tap tracks wall clock no matter what the device does. It has
//! to be measured at the device.
//!
//! And the obvious way at the device does not work either. Counting frames a real
//! [`AudioOut`] accepts measures the device's consumption — a blocking write is accepted
//! only when the device has made room — but the *queue depth* is a few thousand frames of
//! noise against a signal of 2.4 frames a second at 50 ppm. Two samples an hour apart
//! would do it; nobody wants to run this for an hour.
//!
//! So it samples once a second and fits a line. The noise is uncorrelated between samples
//! and the slope's standard error falls as `1/(σ·N^1.5)`: ten minutes at 1 Hz resolves
//! single-digit ppm, which is far below what would matter.
//!
//! ## Reading it
//!
//! ```text
//! cargo run -p pipeline --features audio-pipewire --example audio_drift -- 600
//! cargo run -p pipeline --features audio-pipewire --example audio_drift -- 600 alsa_output.pci-0000_09_00.1.hdmi-stereo
//! ```
//!
//! Worth running on the HDMI sink *and* on a USB or Bluetooth one. HDMI audio is clocked
//! from the TMDS clock via N/CTS, so on an HDMI panel the audio and pixel clocks are the
//! same crystal and this should come back near zero; a USB DAC has its own and will not.

use std::time::{Duration, Instant};

use pipeline::audio_decode::PcmBlock;
use pipeline::audio_out::{selected_output, OutputSelection};

/// The rate the mixer would run at, and so the rate worth measuring.
const RATE: u32 = 48_000;
const CHANNELS: u16 = 2;

/// One write. Small enough that the queue is a fine-grained thing to be ±1 of, large
/// enough not to spend the whole run in call overhead.
const BLOCK: usize = 1024;

/// Skipped before the fit starts. While the device's queue is filling, writes are accepted
/// as fast as the loop offers them and say nothing about its clock — including that would
/// fit a line to a ramp.
const SETTLE: Duration = Duration::from_secs(3);

fn main() {
    let mut args = std::env::args().skip(1);
    let seconds: u64 = args
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(600)
        .max(SETTLE.as_secs() + 10);
    let selection = args.next().map_or(OutputSelection::SystemDefault, |name| {
        OutputSelection::Device(name)
    });

    let mut out = selected_output(&selection);
    if let Err(e) = out.start(RATE, CHANNELS) {
        eprintln!("the device refused {RATE} Hz x {CHANNELS}: {e}");
        std::process::exit(1);
    }
    eprintln!("measuring {selection:?} for {seconds}s at {RATE} Hz…");

    // Silence, so this can run against the panel's real sink without anyone hearing it.
    let block = PcmBlock {
        sample_rate: RATE,
        channels: CHANNELS,
        samples: vec![0.0; BLOCK * usize::from(CHANNELS)],
        pts: Duration::ZERO,
    };

    let began = Instant::now();
    let deadline = began + Duration::from_secs(seconds);
    let mut frames_written: u64 = 0;
    // (seconds since the fit's origin, frames since the fit's origin).
    let mut samples: Vec<(f64, f64)> = Vec::new();
    let mut origin: Option<(Instant, u64)> = None;
    let mut next_sample = began + SETTLE;

    while Instant::now() < deadline {
        if let Err(e) = out.write(&block) {
            eprintln!("the device stopped accepting after {frames_written} frames: {e}");
            break;
        }
        frames_written += BLOCK as u64;

        let now = Instant::now();
        if now < next_sample {
            continue;
        }
        next_sample = now + Duration::from_secs(1);
        let (at, from) = *origin.get_or_insert((now, frames_written));
        samples.push((
            now.saturating_duration_since(at).as_secs_f64(),
            (frames_written - from) as f64,
        ));
    }
    out.stop();

    // Least squares through the samples. The intercept absorbs the queue's standing depth,
    // which is exactly why this is a fit and not a division.
    let n = samples.len() as f64;
    if n < 10.0 {
        eprintln!("only {n} samples; run it for longer");
        std::process::exit(1);
    }
    let mean_t = samples.iter().map(|(t, _)| t).sum::<f64>() / n;
    let mean_f = samples.iter().map(|(_, f)| f).sum::<f64>() / n;
    let sxx: f64 = samples.iter().map(|(t, _)| (t - mean_t).powi(2)).sum();
    let sxy: f64 = samples
        .iter()
        .map(|(t, f)| (t - mean_t) * (f - mean_f))
        .sum();
    let slope = sxy / sxx;
    let intercept = mean_f - slope * mean_t;

    // The standard error of the slope, so the answer comes with a claim about how much of
    // it to believe.
    let residual: f64 = samples
        .iter()
        .map(|(t, f)| (f - (intercept + slope * t)).powi(2))
        .sum();
    let slope_error = (residual / (n - 2.0) / sxx).sqrt();

    let ppm = (slope - f64::from(RATE)) / f64::from(RATE) * 1e6;
    let ppm_error = slope_error / f64::from(RATE) * 1e6;

    println!("device            {selection:?}");
    println!(
        "samples           {n:.0} over {:.0}s",
        samples.last().map_or(0.0, |(t, _)| *t)
    );
    println!("measured rate     {slope:.3} Hz (nominal {RATE})");
    println!("drift             {ppm:+.1} ppm ± {ppm_error:.1}");
    println!(
        "against video     {:+.0} ms/hour, {:+.0} ms/10min",
        ppm * 3.6,
        ppm * 0.6
    );
    // ITU-R BT.1359: roughly +45 ms (audio early) to -125 ms (audio late) before anyone
    // notices. The tighter side is what a drift budget has to respect.
    let hour = (ppm * 3.6).abs();
    println!(
        "verdict           {}",
        if ppm.abs() < 3.0 * ppm_error.max(0.1) {
            "indistinguishable from zero at this run length"
        } else if hour < 45.0 {
            "under the perception threshold even after an hour; no correction needed"
        } else {
            "past the threshold within an hour; the stream needs one clock or the other"
        }
    );
}
