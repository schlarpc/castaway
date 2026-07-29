//! Dump the shell's screens to PNGs for a human to look at.
//!
//! `cargo run -p pipeline --features render --example shell_preview -- outdir [WxH]`
//!
//! The same job `attract_preview` and `card_preview` do for their surfaces: layout and
//! hit-testing are unit-tested, but whether a screen is *readable from across a room* is
//! not something a test can answer.
#![allow(clippy::unwrap_used)]

fn main() -> Result<(), Box<dyn std::error::Error>> {
    use pipeline::attract::{AttractScene, ServiceDetail, TileGlyph};
    use pipeline::picker::{Picker, PickerItem};
    use pipeline::service::ServiceScreen;

    let mut args = std::env::args().skip(1);
    let dir = args.next().unwrap_or_else(|| ".".to_string());
    let (w, h) = args
        .next()
        .and_then(|s| {
            let (a, b) = s.split_once('x')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        })
        .unwrap_or((1920u32, 1080u32));
    std::fs::create_dir_all(&dir)?;

    let write = |name: &str, rgba: Vec<u8>| -> Result<(), Box<dyn std::error::Error>> {
        let path = format!("{dir}/{name}.png");
        std::fs::write(&path, pipeline::attract::to_png(w, h, &rgba)?)?;
        println!("wrote {path} ({w}x{h})");
        Ok(())
    };

    let home = AttractScene::demo();
    write("home", pipeline::attract::render(&home, w, h)?)?;

    // A service screen, as pressing the Cast tile would produce it.
    let tile = home
        .tiles
        .iter()
        .find(|t| t.id == "cast")
        .cloned()
        .expect("the demo scene has a cast tile");
    let detail = tile.detail.clone().expect("the cast tile explains itself");
    write(
        "service",
        pipeline::service::render(&ServiceScreen { tile, detail }, w, h)?,
    )?;

    // A picker mid-flight, and one that found nothing.
    let hosts = Picker::loading("Moonlight", "Looking for hosts…")
        .with_subtitle("Moonlight hosts on this network")
        .with_items(
            vec![
                PickerItem::new("a", "somepc").with_detail("10.0.0.7  ·  paired"),
                PickerItem::new("b", "loungebox").with_detail("10.0.0.9  ·  not paired"),
                PickerItem::new("c", "basement-rig").with_detail("10.0.0.22  ·  paired"),
            ],
            "No hosts found on this network",
        );
    write("picker", pipeline::picker::render(&hosts, w, h)?)?;

    let empty = Picker::loading("Moonlight", "…").with_items(
        vec![],
        "No hosts found. Is Sunshine running, and on this network?",
    );
    write("picker-empty", pipeline::picker::render(&empty, w, h)?)?;

    // Silence the unused-import warning when the glyph list changes.
    let _ = TileGlyph::Moonlight;
    let _ = ServiceDetail {
        headline: String::new(),
        steps: vec![],
        advertised: None,
    };
    Ok(())
}
