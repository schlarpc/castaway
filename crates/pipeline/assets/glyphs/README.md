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
| `gamepad.svg` | Material Design Icons, `microsoft-xbox-controller` | Apache-2.0 |
| `folder.svg` | Material Design Icons, `folder` | Apache-2.0 |
| `miracast.svg` | Material Design Icons, `video-wireless` | Apache-2.0 |
| `gear.svg` | Material Design Icons, `cog` | Apache-2.0 |
| `airplay.svg` | Simple Icons, `airplayvideo` | CC0-1.0 |

Material Design Icons is <https://github.com/Templarian/MaterialDesign-SVG>; Simple Icons
is <https://github.com/simple-icons/simple-icons>. AirPlay comes from the second one only
because MDI has no AirPlay mark.

The **licences above cover the files**, not the marks. Cast, AirPlay, DLNA, Spotify,
YouTube and Bluetooth are trademarks of their owners and are used here to name the
protocol each tile speaks, which is what they are for.

Taken on 2026-07-28. To replace one, drop a new SVG in with the same file name — nothing
reads these but the `TileGlyph::svg` match, and nothing in the renderer knows how any
particular mark is drawn.
