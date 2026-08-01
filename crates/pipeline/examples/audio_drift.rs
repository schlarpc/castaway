//! How fast the audio device's clock actually runs, in ppm against `CLOCK_MONOTONIC`.
//!
//! Stage 0 of #111: which clock the output stream's audio track should be on. If the
//! device's 48 kHz is really 48 000, the question is moot; if it is 48 002, the stream's
//! audio drifts against its wall-clock video by 40 ms an hour and something has to
//! reconcile them.
//!
//! ## What has to be counted, and what must not be
//!
//! It cannot be read off the stream: `stream::audio` places blocks by wall clock *by
//! construction*, so the tap tracks wall clock whatever the device does.
//!
//! It cannot be read off writes either, and the first version of this tried. `AudioOut::write`
//! **never blocks** — both real backends `try_send` and drop the newest block on a full
//! queue rather than back the decode thread up into the adapter — so frames written
//! measure the *caller's* loop and nothing about the device. Run that way this reported
//! 4.59 GHz, which is a satisfying kind of wrong: it was measuring how fast a `for` loop
//! can call `try_send`.
//!
//! What advances on the device's clock is the audio callback, so the count comes from
//! [`AudioOut::frames_played`]. It is sampled once a second and fitted, rather than
//! divided from two endpoints, because the callback's period quantises each reading by up
//! to one buffer; the noise is uncorrelated between samples and the slope's standard error
//! falls as `1/(σ·N^1.5)`, so ten minutes at 1 Hz resolves single-digit ppm.
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

    if out.frames_played().is_none() {
        eprintln!("this backend cannot report what the device consumed; nothing to measure");
        std::process::exit(1);
    }

    // Silence, so this can run against the panel's real sink without anyone hearing it.
    let block = PcmBlock {
        sample_rate: RATE,
        channels: CHANNELS,
        samples: vec![0.0; BLOCK * usize::from(CHANNELS)],
        pts: Duration::ZERO,
    };

    let began = Instant::now();
    let deadline = began + Duration::from_secs(seconds);
    // (seconds since the fit's origin, frames the device consumed since it).
    let mut samples: Vec<(f64, f64)> = Vec::new();
    let mut origin: Option<(Instant, u64)> = None;
    let mut next_sample = began + SETTLE;
    // Keep roughly this much queued. Writes do not block, so the loop has to pace itself
    // or it spins; and if it falls behind, the device underruns and the callback consumes
    // silence — which still advances the clock being measured, but is worth not doing.
    let mut submitted = Duration::ZERO;

    while Instant::now() < deadline {
        let elapsed = Instant::now().saturating_duration_since(began);
        if let Some(ahead) = submitted.checked_sub(elapsed) {
            if let Some(excess) = ahead.checked_sub(Duration::from_millis(200)) {
                std::thread::sleep(excess);
            }
        }
        if let Err(e) = out.write(&block) {
            eprintln!("the device stopped accepting: {e}");
            break;
        }
        submitted +=
            Duration::from_nanos((BLOCK as u64).saturating_mul(1_000_000_000) / u64::from(RATE));

        let now = Instant::now();
        if now < next_sample {
            continue;
        }
        next_sample = now + Duration::from_secs(1);
        let Some(played) = out.frames_played() else {
            break;
        };
        let (at, from) = *origin.get_or_insert((now, played));
        samples.push((
            now.saturating_duration_since(at).as_secs_f64(),
            (played - from) as f64,
        ));
    }
    let underran = out.frames_played().unwrap_or(0);
    out.stop();
    let _ = underran;

    // Least squares through the samples. The intercept absorbs the callback's phase,
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
    // ITU-R BT.1359, and the direction is half the answer: audio *early* stops being
    // acceptable at about +45 ms, audio *late* not until about -125 ms. A device running
    // slow — negative ppm — makes its audio late, which is the forgiving side, so
    // comparing the magnitude against the tight threshold would call a tolerable drift
    // intolerable.
    let ms_per_hour = ppm * 3.6;
    let budget = if ms_per_hour < 0.0 { 125.0 } else { 45.0 };
    println!(
        "audio runs        {}",
        if ppm < 0.0 {
            "late (the forgiving direction)"
        } else {
            "early (the tight direction)"
        }
    );
    println!(
        "verdict           {}",
        if ppm.abs() < 3.0 * ppm_error.max(0.1) {
            "indistinguishable from zero at this run length".to_string()
        } else {
            let hours = budget / ms_per_hour.abs();
            format!("perceptible after about {hours:.1} h of watching (ITU-R BT.1359, {budget:.0} ms this way)")
        }
    );
}
