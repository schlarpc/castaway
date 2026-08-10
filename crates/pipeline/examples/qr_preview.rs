//! Render a QR pairing card to a PNG for previewing (#248).
//!
//! `cargo run -p pipeline --example qr_preview --features render -- [out.png] [payload]`
//!
//! The default payload is a sample `fcast://r/…` connection URL. Any string
//! works — a Matter `MT:…` code, a remote-control URL — since `qr` is the shared
//! component for all of them.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    use pipeline::qr::{self, QrMatrix, QrStyle};
    use pipeline::theme;

    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| "qr_preview.png".to_string());
    let payload = args.next().unwrap_or_else(|| {
        "fcast://r/eyJuYW1lIjoiZG1hLnNwYWNlL3NjcmVlbiIsImFkZHJlc3NlcyI6WyIxMC4wLjAuNSJdLCJzZXJ2aWNlcyI6W3sicG9ydCI6NDY4OTksInR5cGUiOjB9XSwidHh0Ijp7ImZwIjoiUXZycXZ2QnZLaW1Ndkl2SkVsc2lRZWl2aVNYdmVmcXBpWllWeEtYWk9XYz0iLCJ2IjoiNCJ9fQ==".to_string()
    });

    let (w, h) = (520u32, 620u32);
    let mut buf = vec![0u8; (w * h * 4) as usize];
    pipeline::text::fill_gradient(&mut buf, w, h, theme::BG_TOP, theme::BG_BOTTOM);
    pipeline::text::fill_rect(&mut buf, w, h, 20.0, 20.0, 480.0, 480.0, theme::PLATE);

    let matrix = QrMatrix::encode(&payload)?;
    qr::draw(
        &mut buf,
        w,
        h,
        &matrix,
        QrStyle {
            x: 40.0,
            y: 40.0,
            side: 440.0,
            dark: theme::WELL,
            light: [0xff, 0xff, 0xff, 0xff],
        },
    )?;

    let fonts = pipeline::text::fonts()?;
    pipeline::text::draw_text(
        &mut buf,
        w,
        h,
        40.0,
        555.0,
        "Scan in Grayjay to connect",
        30.0,
        theme::TEXT,
        &fonts.regular,
    );

    std::fs::write(&out, pipeline::attract::to_png(w, h, &buf)?)?;
    println!("wrote {out} ({}x{} modules)", matrix.size(), matrix.size());
    Ok(())
}
