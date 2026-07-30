# app assets

| File | Provenance |
|---|---|
| `castaway.rc` | Ours, hand-written. The Windows resource script `build.rs` embeds when the target is Windows: the `.exe` icon and deliberately nothing else (no manifest — see the comment in the file). |
| `castaway.ico` | Generated, checked in. Assembled from the icon PNGs in `crates/pipeline/assets/brand/icon/` (themselves rendered from `castaway-icon.svg`, the source of truth) with ImageMagick: |

```sh
cargo run -p pipeline --example icon_render --features render
magick crates/pipeline/assets/brand/icon/castaway-{16,24,32,48,64,128,256}.png \
    crates/app/assets/castaway.ico
```

Regenerate both steps whenever `castaway-icon.svg` changes; never edit the `.ico`
directly.
