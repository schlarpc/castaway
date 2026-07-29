# Tile glyphs

One SVG per `TileGlyph` variant, rasterised at draw time into a coverage mask and tinted
with the tile's accent (see `attract::draw_tile_glyph`). Only the alpha channel is used,
so the artwork is a stencil — whatever colour is in the file is discarded.

**They replaced hand-rolled distance fields.** Every glyph that looked wrong was a
geometry bug in `attract.rs` rather than in any artwork: the Bluetooth rune's two
diagonals met on the spine instead of crossing it, the gear's teeth were sized with a
radius where `shape::disc` wanted a diameter so they hung off the rim as detached blobs,
and DLNA and Spotify were generic approximations of marks that actually exist. Vector art
is also the only way the brand marks are the brands' own rather than our impression of
them.

| File | Source | Licence |
|---|---|---|
| `cast.svg` | Material Design Icons, `cast` | Apache-2.0 |
| `dlna.svg` | Material Design Icons, `dlna` | Apache-2.0 |
| `spotify.svg` | Material Design Icons, `spotify` | Apache-2.0 |
| `youtube.svg` | Material Design Icons, `youtube` | Apache-2.0 |
| `bluetooth.svg` | Material Design Icons, `bluetooth` | Apache-2.0 |
| `folder.svg` | Material Design Icons, `folder` | Apache-2.0 |
| `miracast.svg` | Material Design Icons, `video-wireless` | Apache-2.0 |
| `gear.svg` | Material Design Icons, `cog` | Apache-2.0 |
| `airplay.svg` | Simple Icons, `airplayvideo` | CC0-1.0 |
| `moonlight.svg` | moonlight-stream/moonlight-qt, `app/res/moonlight.svg` | GPL-3.0 (the project's) |
| `matter.svg` | Wikimedia Commons, *Logo of Matter connectivity standard* | trademark, see below |

Material Design Icons is <https://github.com/Templarian/MaterialDesign-SVG>; Simple Icons
is <https://github.com/simple-icons/simple-icons>. AirPlay comes from the second one only
because MDI has no AirPlay mark.

**Two are edited**, because neither was usable as a stencil as shipped:

- `moonlight.svg` is drawn as three stacked layers — a grey disc, a white disc on top of
  it, and the burst over that. Flattened to one colour it is a solid circle. Here the
  outer ring is a stroked circle (`r=112`, 32 wide, so the annulus is the original 96–128)
  and the burst path is unchanged, which reproduces the mark rather than reinterpreting
  it.
- `matter.svg` is the full lockup, symbol and "matter" wordmark in a single path. Column-
  profiling the rasterised ink puts the gap between them at x=74.51, so the viewBox is
  cropped there. The wordmark is still in the path data and simply falls outside the
  viewport — it cannot be deleted without splitting a path that was authored as one.

The **licences above cover the files**, not the marks. Cast, AirPlay, DLNA, Spotify,
YouTube, Bluetooth, Moonlight and Matter are trademarks of their owners and are used here
to name the protocol each tile speaks, which is what they are for. Matter's is a CSA
certification mark with its own usage rules: this is nominative use on a tile labelled
"Matter Cast", not a claim of certification, and if the panel is ever certified that is a
question for the CSA rather than for this file.

`matter.svg` is checked in ahead of any Matter Cast support — the mark was findable today
and the protocol is coming.

Taken on 2026-07-28. To replace one, drop a new SVG in with the same file name — nothing
reads these but the `TileGlyph::svg` match, and nothing in the renderer knows how any
particular mark is drawn.
