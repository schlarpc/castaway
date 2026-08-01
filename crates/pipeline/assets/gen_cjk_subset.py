"""Generate the bundled CJK fallback fonts from Noto Sans CJK.

Output:
  crates/pipeline/assets/NotoSansCJK-Regular-subset.otf
  crates/pipeline/assets/NotoSansCJK-Bold-subset.otf

DejaVu — the primary face — has never shipped CJK, so a Japanese track title drew as
blanks and its width was measured from `.notdef` advances, which put the layout out too
(#88). A full Noto Sans CJK is ~16 MB per weight against an artifact where size is already
a live concern (docs/cross-build.md), so what is bundled is a subset: the coverage that
real track metadata actually uses, at a fraction of the bytes.

What is included, and why each block:

  - JIS X 0208 ku 1-8       punctuation, kana, fullwidth alphanumerics, Greek, Cyrillic
  - JIS X 0208 ku 16-47     level 1 kanji (2965), which is the "common use" set
  - GB 2312 level 1         the 3755 most common simplified hanzi
  - KS X 1001 hangul        the 2350 precomposed syllables that encoding carries
  - Latin punctuation Noto has and DejaVu does not reach for in CJK text

This is deliberately *not* complete CJK. Level 2 kanji, rare hanzi and the other ~8800
hangul syllables are absent, so an unusual name can still miss. The fallback chain in
text.rs is what makes that recoverable — another font can be appended without any layout
code changing — and the honest statement is that this covers common metadata, not
everything.

Run it with:

    nix shell nixpkgs#python3Packages.fonttools nixpkgs#noto-fonts-cjk-sans \
      --command python3 crates/pipeline/assets/gen_cjk_subset.py

The source font is pinned by the nixpkgs revision in flake.lock, so the same command on
the same tree produces the same bytes.
"""

import io
import os
import pathlib
import subprocess
import sys

from fontTools import subset
from fontTools.ttLib import TTCollection
from fontTools.varLib import instancer

# The JP subfont of the pan-CJK collection. All five subfonts share one glyph set and
# differ in which variant each codepoint maps to; JP is the right default for a receiver
# whose CJK metadata is overwhelmingly Japanese track titles, and the shared Han glyphs
# still render for Chinese.
JP_SUBFONT = 0

SOURCE = "share/fonts/opentype/noto-cjk/NotoSansCJK-VF.otf.ttc"

OUT_DIR = pathlib.Path(__file__).resolve().parent


def find_source() -> pathlib.Path:
    """Locate the collection in whatever nix shell handed us."""
    for root in os.environ.get("XDG_DATA_DIRS", "").split(":"):
        if not root:
            continue
        candidate = pathlib.Path(root).parent / SOURCE
        if candidate.exists():
            return candidate
    # `nix shell` puts the package's bin on PATH; walk up from a store path on it.
    for entry in os.environ.get("PATH", "").split(":"):
        p = pathlib.Path(entry)
        if p.name == "bin" and (p.parent / SOURCE).exists():
            return p.parent / SOURCE
    raise SystemExit(
        "cannot find NotoSansCJK-VF.otf.ttc; run this inside\n"
        "  nix shell nixpkgs#python3Packages.fonttools nixpkgs#noto-fonts-cjk-sans"
    )


def jis_x_0208(ku_ranges) -> set:
    """Codepoints in the given JIS X 0208 ku (row) ranges, via EUC-JP.

    EUC-JP encodes a JIS cell as (0xA0 + ku, 0xA0 + ten), which makes enumerating a row
    range a decode rather than a table this file would have to carry and keep correct.
    """
    out = set()
    for lo, hi in ku_ranges:
        for ku in range(lo, hi + 1):
            for ten in range(1, 95):
                raw = bytes([0xA0 + ku, 0xA0 + ten])
                try:
                    out.add(ord(raw.decode("euc_jp")))
                except (UnicodeDecodeError, TypeError):
                    pass
    return out


def gb2312_level1() -> set:
    """The 3755 level-1 simplified hanzi, via EUC-CN rows 16-55."""
    out = set()
    for ku in range(16, 56):
        for ten in range(1, 95):
            raw = bytes([0xA0 + ku, 0xA0 + ten])
            try:
                out.add(ord(raw.decode("gb2312")))
            except (UnicodeDecodeError, TypeError):
                pass
    return out


def ksx1001_hangul() -> set:
    """The 2350 precomposed hangul syllables KS X 1001 carries, via EUC-KR rows 16-40."""
    out = set()
    for ku in range(16, 41):
        for ten in range(1, 95):
            raw = bytes([0xA0 + ku, 0xA0 + ten])
            try:
                out.add(ord(raw.decode("euc_kr")))
            except (UnicodeDecodeError, TypeError):
                pass
    return out


def wanted() -> set:
    cps = set()
    # Punctuation, kana, fullwidth forms, Greek, Cyrillic.
    cps |= jis_x_0208([(1, 8)])
    # Level 1 kanji.
    cps |= jis_x_0208([(16, 47)])
    cps |= gb2312_level1()
    cps |= ksx1001_hangul()
    # Blocks the encodings above do not fully cover but that CJK text uses constantly.
    for lo, hi in [
        (0x3000, 0x303F),  # CJK symbols and punctuation
        (0x3040, 0x309F),  # hiragana
        (0x30A0, 0x30FF),  # katakana
        (0x31F0, 0x31FF),  # katakana phonetic extensions
        (0xFF00, 0xFFEF),  # halfwidth and fullwidth forms
    ]:
        cps |= set(range(lo, hi + 1))
    return cps


def build(source: pathlib.Path, weight: float, out_name: str, codepoints: set) -> None:
    collection = TTCollection(str(source), lazy=False)
    font = collection.fonts[JP_SUBFONT]

    # Pin the variable axis to one weight and drop the axis: a static instance is smaller
    # and ab_glyph reads one outline rather than interpolating at every draw.
    font = instancer.instantiateVariableFont(font, {"wght": weight}, inplace=True)

    # ...and then CFF2 down to plain CFF, which is the part that is load-bearing rather
    # than an optimisation. `ab_glyph` reaches CFF2 outlines only through ttf-parser's
    # `variable-fonts` feature, which it does not enable by default — so a CFF2 file
    # resolves glyph *ids* perfectly, reports no error, and outlines every one of them to
    # nothing. That is the same silent-blank failure #88 is about, arriving by a different
    # route, and it is why `japanese_actually_rasterises` asserts on drawn pixels rather
    # than on glyph coverage.
    # Returns a new font rather than mutating in place — it round-trips through memory
    # twice because the conversion renames glyphs.
    font = instancer.downgradeCFF2ToCFF(font)

    options = subset.Options()
    # Layout features are dropped: the renderer in text.rs is a per-character rasterizer
    # with kerning from the font's own tables, so shaping tables would be carried and
    # never read.
    options.layout_features = []
    options.drop_tables += ["DSIG"]
    options.name_IDs = ["*"]
    options.notdef_outline = True
    options.recalc_bounds = True

    subsetter = subset.Subsetter(options=options)
    subsetter.populate(unicodes=sorted(codepoints))
    subsetter.subset(font)

    out = OUT_DIR / out_name
    font.save(str(out))
    covered = len(set(font.getBestCmap()) & codepoints)
    print(
        f"{out_name}: {out.stat().st_size / 1024:.0f} KiB, "
        f"{covered} of {len(codepoints)} requested codepoints"
    )


def main() -> None:
    source = find_source()
    print(f"source: {source}")
    codepoints = wanted()
    build(source, 400, "NotoSansCJK-Regular-subset.otf", codepoints)
    build(source, 700, "NotoSansCJK-Bold-subset.otf", codepoints)


if __name__ == "__main__":
    main()
