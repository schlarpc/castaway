//! Print the active audio backend and the output devices it can offer — what the
//! settings screen will show, without a panel. Run it with the backend under test:
//!
//! ```sh
//! cargo run -p pipeline --example output_devices --features audio-pipewire
//! cargo run -p pipeline --example output_devices --features audio-out
//! ```
//!
//! `--beep [device id]` additionally opens a stream there (the default if no id) and
//! plays one second of A440 — the whole session path, minus a protocol.

fn main() {
    let backend = pipeline::audio_select::active_backend();
    println!("backend: {backend:?}");
    match pipeline::audio_select::list_output_devices() {
        Ok(devices) if devices.is_empty() => println!("no devices"),
        Ok(devices) => {
            for d in devices {
                println!("  {}  ({})", d.label, d.id);
            }
        }
        Err(e) => println!("error: {e}"),
    }

    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--beep") {
        return;
    }
    let selection = match args.next() {
        Some(id) => pipeline::audio_select::OutputSelection::Device(id),
        None => pipeline::audio_select::OutputSelection::SystemDefault,
    };
    beep(&selection);
}

#[cfg(feature = "audio")]
fn beep(selection: &pipeline::audio_select::OutputSelection) {
    println!("beeping to {selection:?}…");
    let mut out = pipeline::audio_out::selected_output(selection);
    if let Err(e) = out.start(48_000, 2) {
        println!("start failed: {e}");
        return;
    }
    // One second of A440, in ~10 ms blocks, like a decoder would hand over.
    for block in 0..100 {
        let samples: Vec<f32> = (0..480)
            .flat_map(|i| {
                let t = (block * 480 + i) as f32 / 48_000.0;
                let s = (t * 440.0 * std::f32::consts::TAU).sin() * 0.2;
                [s, s]
            })
            .collect();
        let pcm = pipeline::audio_decode::PcmBlock {
            sample_rate: 48_000,
            channels: 2,
            samples,
            pts: std::time::Duration::from_millis(block * 10),
        };
        if let Err(e) = out.write(&pcm) {
            println!("write failed: {e}");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    out.stop();
    println!("beeped.");
}

#[cfg(not(feature = "audio"))]
fn beep(_: &pipeline::audio_select::OutputSelection) {
    println!("this build has no audio path; rebuild with --features audio-out or audio-pipewire");
}
