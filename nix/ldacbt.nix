# libldacBT — Sony's own LDAC codec library, built with the **decoder** in it.
#
# The one A2DP codec libav cannot decode, and the reason the LDAC endpoint went
# unadvertised for as long as it did (OPEN-QUESTIONS Q22). `ldac-sys` links this;
# `pipeline::ldac_decode` is the safe wrapper.
#
# ## Why this is not just `pkgs.ldacbt`
#
# Because under the nixpkgs this flake pins, `pkgs.ldacbt` is **encoder-only** and would
# have looked like the right answer right up to the link error. It is EHfive/ldacBT
# 2.0.2.3, whose CMake build defines `_ENCODE_ONLY`: it installs `libldacBT_enc.so`, a
# header with no `ldacBT_decode` declaration in it at all, and a pkg-config file called
# `ldacBT-enc`. A newer nixpkgs replaced it with open-vela's fork at 2.0.72, which does
# build the full library — but reaching that would mean bumping the flake's nixpkgs input
# and with it ffmpeg, Electron and every other pin, for one codec.
#
# So the source is its own pinned input and the build is here, exactly as
# nix/moonlight-common-c.nix does for the GameStream core. Two things follow from that
# which are worth stating: the header `ldac-sys`'s bindings are generated from is the one
# belonging to *this* build rather than to whatever `pkgs.ldacbt` happens to be, and the
# `ldac-bindings` check therefore compares against the library we actually link.
#
# ## Licence
#
# Apache-2.0 (`MODULE_LICENSE_APACHE2`, and the `NOTICE` this installs). Unlike
# moonlight-common-c's GPL-3.0, that composes with this MIT tree without the quarantine
# D37 needed — linking it does not bind the artifact. The `ldac` feature is still off by
# default, but for a build-dependency reason rather than a licence one.
#
# ## The build
#
# Upstream ships a hand-written makefile rather than a build system, and its three
# configuration switches are set to `xTRUE` — i.e. off — which is what gives us the full
# encode+decode library. Left alone deliberately:
#
#   - `ENCODE_ONLY` / `DECODE_ONLY` off, so both halves are compiled. The encoder is not
#     dead weight: it is what generates the checked-in test vectors
#     (`pipeline/examples/ldac_fixtures.rs`), which is the only way the decode path gets
#     tested without a phone in the room.
#   - `FIXED_POINT` off, so the codec uses its floating-point path and needs `-lm`. The
#     fixed-point build exists for DSPs without an FPU; the panel is an x86 box.
#
# `ldaclib.c` and `ldacBT.c` are aggregate translation units that `#include` the other
# twenty-odd `.c` files, so "two object files" is upstream's design and not a truncated
# build.
{ pkgs, src }:

pkgs.stdenv.mkDerivation {
  pname = "ldacBT";
  # From LDACBT_LIB_VER_{MAJOR,MINOR,BRANCH} in src/ldacBT_api.c, which is also what
  # `ldacBT_get_version` returns and what upstream's makefile puts in the soname.
  version = "2.0.72";
  inherit src;

  # No cmake, no autotools: `gcc/libldacBT.mk` with relative paths into ../src and ../inc.
  buildPhase = ''
    runHook preBuild
    make -C gcc -f libldacBT.mk CC=$CC
    runHook postBuild
  '';

  # The makefile has no install target — it leaves its output in `gcc/`.
  installPhase = ''
    runHook preInstall
    mkdir -p $out/lib $out/include $out/share/doc/ldacBT
    cp -P gcc/libldacBT.so* $out/lib/
    cp gcc/ldacBT.a $out/lib/libldacBT.a 2>/dev/null || true
    # The header `ldac-sys`'s bindings are generated from. Installed rather than read out
    # of the source tree so the bindings check and the link target cannot disagree about
    # which version they mean.
    cp inc/ldacBT.h $out/include/
    cp LICENSE NOTICE $out/share/doc/ldacBT/
    runHook postInstall
  '';

  # A build that produced no shared object would otherwise "succeed" and fail much later
  # at link time, in a crate that has nothing to do with the cause. And a build that
  # produced one *without the decoder in it* is the specific failure this derivation
  # exists to avoid — `pkgs.ldacbt` under this pin is exactly that — so both the installed
  # header and the exported symbols are checked for `ldacBT_decode`, which is the one thing
  # an encoder-only build cannot have.
  postInstall = ''
    soname=$(echo $out/lib/libldacBT.so.*.*)
    if [ ! -f "$soname" ]; then
      echo "libldacBT built no shared object — did gcc/libldacBT.mk change?" >&2
      exit 1
    fi
    grep -q 'ldacBT_decode' $out/include/ldacBT.h || {
      echo "the installed header declares no ldacBT_decode." >&2
      exit 1
    }
    if ! ${pkgs.binutils-unwrapped}/bin/nm -D --defined-only "$soname" \
         | grep -q 'ldacBT_decode'; then
      echo "libldacBT exports no ldacBT_decode: this was built _ENCODE_ONLY." >&2
      echo "That is the whole reason this derivation exists instead of pkgs.ldacbt;" >&2
      echo "check that ENCODE_ONLY/DECODE_ONLY in gcc/libldacBT.mk are still 'xTRUE'." >&2
      echo "exported ldacBT_* symbols were:" >&2
      ${pkgs.binutils-unwrapped}/bin/nm -D --defined-only "$soname" \
        | grep 'ldacBT_' >&2 || echo "  (none at all)" >&2
      exit 1
    fi
  '';

  meta = {
    description = "LDAC Bluetooth codec (Sony), encoder and decoder";
    homepage = "https://github.com/open-vela/external_libldac";
    license = pkgs.lib.licenses.asl20;
  };
}
