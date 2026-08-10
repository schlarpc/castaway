//! Render the idle/attract scene to a PNG for previewing.
//!
//! `cargo run -p pipeline --example attract_preview --features render -- out.png [WxH] [palette]`
//!
//! `palette` wears a seasonal look out of season so a human can judge it (#263): a
//! season's config name (`pride`, `trans`, `halloween`, …) or `plain` for the panel's
//! own ramp. Left out, the demo scene's own default stands.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let out = args
        .next()
        .unwrap_or_else(|| "attract_preview.png".to_string());
    // The trailing arguments are told apart by shape rather than position, so
    // `attract_preview out.png pride` works without a size to hold its place.
    let mut scene = pipeline::attract::AttractScene::demo();
    let (mut w, mut h) = (1920u32, 1080u32);
    for arg in args {
        if let Some((a, b)) = parse_size(&arg) {
            (w, h) = (a, b);
        } else {
            scene.season = parse_season(&arg)?;
        }
    }
    let rgba = pipeline::attract::render(&scene, w, h)?;
    let png = pipeline::attract::to_png(w, h, &rgba)?;
    std::fs::write(&out, png)?;
    println!("wrote {out} ({w}x{h})");
    Ok(())
}

/// `WxH`, if that is what this argument is.
fn parse_size(arg: &str) -> Option<(u32, u32)> {
    let (a, b) = arg.split_once('x')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// A season by its config-file name, or `plain` for none — the same spellings
/// `castaway.toml`'s `theme` key takes, so a preview can be reproduced on the panel.
fn parse_season(name: &str) -> Result<Option<pipeline::theme::Season>, Box<dyn std::error::Error>> {
    use pipeline::theme::Season;
    if name == "plain" {
        return Ok(None);
    }
    Season::ALL
        .into_iter()
        .find(|s| s.name() == name)
        .map(Some)
        .ok_or_else(|| {
            let known: Vec<&str> = Season::ALL.iter().map(|s| s.name()).collect();
            format!(
                "unknown palette '{name}'; try plain or one of: {}",
                known.join(", ")
            )
            .into()
        })
}
