//! Regenerate the checked-in icon rasters from the SVG source of truth.
//!
//! `cargo run -p pipeline --example icon_render --features render -- [outdir]`
//!
//! Writes `castaway-{side}.png` for every size the platforms ask for: the
//! hicolor theme install in nix/linux-kiosk.nix and the Windows `.ico`
//! (assembled from these — see `crates/app/assets/README.md`). Run it after any
//! edit to `assets/brand/castaway-icon.svg` and commit what changed; the PNGs
//! are never edited by hand.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let outdir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "crates/pipeline/assets/brand/icon".to_string());
    std::fs::create_dir_all(&outdir)?;
    for side in [16u32, 24, 32, 48, 64, 128, 256] {
        let rgba = pipeline::icon::rasterize(side).ok_or("icon failed to rasterize")?;
        let png = pipeline::attract::to_png(side, side, &rgba)?;
        let path = format!("{outdir}/castaway-{side}.png");
        std::fs::write(&path, png)?;
        println!("wrote {path}");
    }
    Ok(())
}
