# Brand assets

Taken from <https://dma.space> on 2026-07-28. **Provisional** — they were ripped from the
live site to unblock the app shell (D38) rather than handed over as a kit, so treat file
names and crops as ours and the artwork as theirs. If a canonical kit turns up, replace
these and keep the same file names.

**Except the app icon, which is ours.** `castaway-icon.svg` was authored in this repo
(a paper boat catching a signal, in the panel's own `theme.rs` colours) and is the
single source of truth for every raster of it:

- `icon/castaway-{16,24,32,48,64,128,256}.png` — generated, checked in. Regenerate
  with `cargo run -p pipeline --example icon_render --features render` after any edit
  to the SVG; never edit the PNGs by hand. The same SVG is rasterized at runtime for
  the winit window icon (`pipeline::icon`), so what the taskbar shows and what is
  installed on disk cannot drift.
- `crates/app/assets/castaway.ico` — the Windows `.exe` icon, assembled from these
  PNGs (see the README there).

nix/linux-kiosk.nix installs the PNGs as the hicolor theme icon and ships a
`castaway.desktop`, which is how a Wayland compositor — where windows carry no icon
property — finds it via the window's `app_id`.

| File | Source | What it is |
|---|---|---|
| `brand-logo.svg` | `/assets/brand-logo.svg` | The `dma` wordmark + spark, 583×128. Authored as flat white paths with no fill colour of its own — the site tints it with `filter: invert(1)` on dark backgrounds. |
| `mascot-inner.png` | `/assets/mascot-inner.png` | DMA-chan, inner layer, 808×1274. Her lower torso only — the rest of the frame is transparent, so on its own it looks like a stray shape. |
| `mascot-outer.png` | `/assets/mascot-outer.png` | DMA-chan, outer layer, 808×1274. Head, arms and sash. The site stacks the two, so either can be animated against the other; **inner goes underneath**, because her arms have to occlude the body rather than the other way round. Drawing only this one leaves her without a lower half. |
| `favicon.png` | `/favicon.png` | 96×96 mark. The only square-cropped form of the logo, so it is what a small tile or the home pill should use. |

## Palette

Lifted from the site's CSS custom properties (`/assets/styles.*.css`). The names are
theirs; the hex is what we render.

| Token | Value | Notes |
|---|---|---|
| `--b-color` | `#02abfc` | Blue. The site's default `--brand-color` and its link colour. |
| `--g-color` | `#56ba5b` | Green. |
| `--r-color` | `#f5615f` | Red/coral. |
| `--y-color` | `#d29400` | Gold. **Authored as `oklch(0.7079 0.1638 82.58)`, which is outside sRGB** — this hex is the gamut-clamped conversion, so it is duller than intended. If the panel ever renders wide-gamut, go back to the oklch value rather than this. |
| background | `#000000` / `#fafafa` | Dark and light. The panel is dark-only. |

Note our existing surfaces do *not* use this palette yet: the attract scene, now-playing
card and OSD all use their own gradient and accents chosen before these assets existed.
Reconciling them is part of #24 and deliberately not part of landing the shell.

## Fonts

The site uses **Inter** (body and headings) and **Spline Sans Mono** (monospace). This
crate embeds **DejaVu Sans / Sans-Bold** and every rendered surface is measured against
it, including golden-image tests.

**Deliberately not matched, and not worth chasing.** DejaVu stays.

The mechanical reason it was not a drop-in: both Inter and Spline Sans Mono are packaged
only as *variable* fonts, and `ab_glyph` renders a variable font's default instance with
no weight-axis selection — so Inter for both `regular` and `bold` would draw both at
Regular, and every title, tile label and row heading on these screens is bold against dim
body text.

The reason it stays that way is simpler: nobody reading a wall from across a room is
identifying the body face, and matching it would mean vendoring static instances and
re-checking every surface's metrics for a difference that is invisible at that distance.
The brand is carried by the palette, the mascot and the wordmark, which is where it is
actually legible.
