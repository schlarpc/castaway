//! How fast the media clock actually advances against the wall, on a real file.
//!
//! Written for #52: an iPhone casting VP8 1080p60 + Vorbis through VLC played at roughly
//! two frames a second, with the video decode thread *waiting* rather than losing —
//! sampling it showed every stack in `drain_paced`'s pacing wait. So the video was obeying
//! a clock that was crawling, and the question is which side of `observe_audio` the crawl
//! is on.
//!
//! This runs the whole media-URL path — `decode_av` feeding the real `run_pcm`, paced to
//! real time, driving the real `MediaClock` — and reports the clock's advance against wall
//! time. Anything near 1.0 is healthy. The reported symptom is about 1/30.
//!
//! ```text
//! cargo run -p pipeline --features ffmpeg,render,audio --example media_clock_rate -- <file>
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pipeline::audio_session::PacedSession;
use pipeline::clock::MediaClock;
use pipeline::ffmpeg_decode::{decode_av, MediaLayout};
use pipeline::HwPreference;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: media_clock_rate <file> [seconds]");
        std::process::exit(2);
    });
    let budget: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    let clock = Arc::new(MediaClock::new());
    let seek = Arc::new(pipeline::seek::SeekControl::default());
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = sync_channel::<castaway_core::PcmFrame>(64);

    // The real playback thread, paced to real time, through a mixer over a null sink so
    // this runs anywhere. The mixer treats a null device exactly as a real one for timing
    // — it keeps time on the wall — which is what makes this measure the clock rather
    // than the sound card.
    let mixer = pipeline::mixer::AudioMixer::new(Arc::new(|| {
        Box::new(pipeline::audio_out::NullAudioOut::new())
    }));
    pipeline::audio_session::spawn_pcm(
        rx,
        mixer.input(pipeline::mixer::Backpressure::Pull),
        Arc::clone(&stop),
        Some(PacedSession {
            clock: Arc::clone(&clock),
            seek: Arc::clone(&seek),
        }),
    );

    // A sampler, so the rate is measured rather than inferred from the end state.
    let sampler_clock = Arc::clone(&clock);
    let sampler_stop = Arc::clone(&stop);
    let sampler = std::thread::spawn(move || {
        let mut samples: Vec<(Duration, Duration)> = Vec::new();
        let start = Instant::now();
        while !sampler_stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(250));
            if let Some(media) = sampler_clock.now() {
                samples.push((start.elapsed(), media));
            }
        }
        samples
    });

    let started = Instant::now();
    let mut layout = MediaLayout::default();
    let mut frames = 0usize;
    let mut first_frame_at = None;
    let deadline = started + Duration::from_secs(budget);
    let give_up = || Instant::now() >= deadline;

    let outcome = decode_av(
        &path,
        HwPreference::SoftwareOnly,
        &clock,
        Some(&seek),
        Some(tx),
        &give_up,
        |l| layout = l.clone(),
        |_frame| {
            frames += 1;
            first_frame_at.get_or_insert_with(Instant::now);
            true
        },
    );
    let wall = started.elapsed();
    stop.store(true, Ordering::Relaxed);
    let samples = sampler.join().unwrap_or_default();

    println!("file          {path}");
    println!("layout        {layout:?}");
    println!("outcome       {outcome:?}");
    println!("wall          {:.2}s", wall.as_secs_f64());
    println!("video frames  {frames}");
    #[allow(clippy::cast_precision_loss)]
    if let Some(first) = first_frame_at {
        let presenting = started.elapsed().saturating_sub(first - started);
        println!(
            "frame rate    {:.2} fps presented",
            frames as f64 / presenting.as_secs_f64().max(1e-9)
        );
    }

    // The number this exists for: media seconds per wall second, from the first sample
    // that had a clock to the last.
    match (samples.first(), samples.last()) {
        (Some(first), Some(last)) if last.0 > first.0 => {
            let dw = (last.0 - first.0).as_secs_f64();
            let dm = last.1.as_secs_f64() - first.1.as_secs_f64();
            println!("clock advance {dm:.3}s of media in {dw:.3}s of wall");
            println!("clock rate    {:.4}x", dm / dw);
        }
        _ => println!("clock advance  <the clock never started>"),
    }
    for (at, media) in samples.iter().take(40) {
        println!(
            "  t={:>6.2}s  clock={:>7.3}s",
            at.as_secs_f64(),
            media.as_secs_f64()
        );
    }
}
