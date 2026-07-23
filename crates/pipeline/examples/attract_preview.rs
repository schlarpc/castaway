//! Render the idle/attract scene to a PNG for previewing.
//!
//! `cargo run -p pipeline --example attract_preview --features render -- out.png [WxH]`

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let out = args
        .next()
        .unwrap_or_else(|| "attract_preview.png".to_string());
    let (w, h) = args
        .next()
        .and_then(|s| {
            let (a, b) = s.split_once('x')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        })
        .unwrap_or((1920u32, 1080u32));

    let scene = pipeline::attract::AttractScene::demo();
    let rgba = pipeline::attract::render(&scene, w, h)?;
    let png = pipeline::attract::to_png(w, h, &rgba)?;
    std::fs::write(&out, png)?;
    println!("wrote {out} ({w}x{h})");
    Ok(())
}
