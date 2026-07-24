//! Preview the composited screen: the attract scene with an OSD banner over it (what a
//! viewer sees the moment a cast starts).
//!
//! `cargo run -p pipeline --example screen_preview --features render -- out.png [WxH] [msg]`
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let out = args
        .next()
        .unwrap_or_else(|| "screen_preview.png".to_string());
    let (w, h) = args
        .next()
        .and_then(|s| {
            let (a, b) = s.split_once('x')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        })
        .unwrap_or((1920u32, 1080u32));
    let msg = args
        .next()
        .unwrap_or_else(|| "Now casting from Chrome (laptop)".to_string());

    // Background: the idle attract scene.
    let scene = pipeline::attract::AttractScene::demo();
    let mut frame = pipeline::attract::render(&scene, w, h)?;

    // Overlay: the OSD banner, alpha-composited over the background (as the GPU would).
    // The banner is a tight image placed by a transform, so blit it row by row at the
    // transform's pixel origin — the same texel-per-pixel mapping the compositor does.
    let banner = pipeline::osd::render_banner(&msg, w, h)?;
    let ox = (banner.transform.offset_x * w as f32).round() as u32;
    let oy = (banner.transform.offset_y * h as f32).round() as u32;
    for row in 0..banner.height.min(h.saturating_sub(oy)) {
        let cols = banner.width.min(w.saturating_sub(ox)) as usize;
        let src = (row * banner.width) as usize * 4;
        let dst = ((oy + row) * w + ox) as usize * 4;
        let fg_row = &banner.rgba[src..src + cols * 4];
        let bg_row = &mut frame[dst..dst + cols * 4];
        for (bg, fg) in bg_row.chunks_exact_mut(4).zip(fg_row.chunks_exact(4)) {
            let a = f32::from(fg[3]) / 255.0;
            for c in 0..3 {
                bg[c] = (f32::from(fg[c]) * a + f32::from(bg[c]) * (1.0 - a)).round() as u8;
            }
        }
    }

    let png = pipeline::attract::to_png(w, h, &frame)?;
    std::fs::write(&out, png)?;
    println!("wrote {out} ({w}x{h})");
    Ok(())
}
