# Carve AirServer's builtin Cast credential database and its two KEK constants out of
# the pinned installer, at build time.
#
# Why this exists: `cast-replay` needs two BLAKE2b constants to open an AirServer
# credential database (PROVENANCE §3). They used to be string literals in
# `src/airserver_db.rs`, which meant this repository redistributed App Dynamic's
# constants. It does not any more — they are recovered here, from an installer the
# builder fetches from the vendor, and reach the crate as build-time environment
# variables. Nothing carved is written into the source tree.
#
# The carver is deliberately offset-free; see the module docstring in
# ./airserver-carve.py for how the database and the constants are each located
# structurally and confirmed (a b-tree integrity check, and a Poly1305 tag).
#
# This is the same carve-out shape as the Widevine CDM and `vmp-sign.sh`: the
# unfree/third-party artifact is fetched, never vendored, and the build fails closed
# without it rather than falling back to something that looks like it worked.
{
  lib,
  stdenvNoCC,
  python3,
  p7zip,
  airserverMsi,
}:

stdenvNoCC.mkDerivation {
  pname = "airserver-cast-carve";
  version = "5.7.2";

  src = airserverMsi;
  dontUnpack = true;

  nativeBuildInputs = [
    p7zip
    (python3.withPackages (ps: [ ps.pynacl ps.cryptography ]))
  ];

  buildPhase = ''
    runHook preBuild

    # The MSI is a WiX package: the payload is a cab stored as a stream beside the
    # MSI tables, so this is two unpacks rather than one.
    7z x -bso0 -bsp0 -omsi "$src"
    cab=$(ls msi/*.cab | head -1)
    if [ -z "$cab" ]; then
      echo "no cab in the MSI payload — did the installer format change?" >&2
      exit 1
    fi
    7z x -bso0 -bsp0 -ocab "$cab"

    # Pick the largest AirServer.exe rather than assuming a path: the cab is flat in
    # current builds but the console binary shares the prefix.
    exe=$(find cab -iname 'AirServer.exe' -printf '%s\t%p\n' | sort -rn | head -1 | cut -f2)
    if [ -z "$exe" ]; then
      echo "no AirServer.exe in the cab payload" >&2
      exit 1
    fi

    python3 ${./airserver-carve.py} "$exe" -o "$out"

    runHook postBuild
  '';

  # The carver writes straight to $out.
  installPhase = "true";

  meta = {
    description = "Cast credential database and KEK constants carved from a pinned AirServer installer";
    # Not redistributable: this derivation's *output* is App Dynamic's material, and
    # exists only so the constants stop living in our source tree.
    license = lib.licenses.unfree;
    platforms = lib.platforms.all;
  };
}
