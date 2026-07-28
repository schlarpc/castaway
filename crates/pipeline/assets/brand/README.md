# Brand assets

Taken from <https://dma.space> on 2026-07-28. **Provisional** — they were ripped from the
live site to unblock the app shell (D38) rather than handed over as a kit, so treat file
names and crops as ours and the artwork as theirs. If a canonical kit turns up, replace
these and keep the same file names.

| File | Source | What it is |
|---|---|---|
| `brand-logo.svg` | `/assets/brand-logo.svg` | The `dma` wordmark + spark, 583×128. Authored as flat white paths with no fill colour of its own — the site tints it with `filter: invert(1)` on dark backgrounds. |
| `mascot-inner.png` | `/assets/mascot-inner.png` | DMA-chan, inner layer, 808×1274. |
| `mascot-outer.png` | `/assets/mascot-outer.png` | DMA-chan, outer layer, 808×1274. The site stacks the two, so either can be animated against the other. |
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

**Attempted and blocked.** Both Inter and Spline Sans Mono are packaged only as *variable*
fonts (`InterVariable.ttf`, `Inter[opsz,wght].ttf`), and `ab_glyph` — the rasterizer this
crate uses — renders a variable font's default instance with no way to select a weight
axis. Loading Inter for both `regular` and `bold` would draw both at Regular, and the
screens lean on that contrast: every title, tile label and row heading is bold against
dim body text.

Getting there needs one of: a static-instance build of Inter vendored here (the upstream
release ships them, nixpkgs does not), or a rasterizer with variable-font axis support.
Neither is hard; both are a different piece of work from a palette. The palette landed
(`pipeline::theme`); the typeface is still DejaVu.
