# Carve the CKS identity and backend credentials out of `libAirReceiver.so`.
#
# The mirror of nix/airserver-carve.nix, for the other borrowed identity. Both used
# to be checked in under `crates/cast-replay/fixtures/`; both are other companies'
# Google-issued Cast device credentials, private keys included, so neither belongs in
# this tree.
#
# Two barriers, two techniques — see the module docstring in ./airreceiver-carve.py:
# the credentials are behind a string obfuscator whose key is brute-forced and a
# relocation table anchored on a public header name, and the identity is inside a
# `dbio` container whose KEK is computed at runtime by a digest implemented as a
# bytecode interpreter, which is why unicorn is a build input.
#
# The carve verifies all 900 windows' receiver-auth signatures against the shipped
# device certificate before writing anything, so a wrong key or a mis-parsed
# directory fails here rather than as a receiver that cannot authenticate.
{
  lib,
  stdenvNoCC,
  python3,
  airreceiverSrc,
}:

stdenvNoCC.mkDerivation {
  pname = "airreceiver-cast-carve";
  version = "5.1.7";

  dontUnpack = true;

  nativeBuildInputs = [
    (python3.withPackages (ps: [ ps.unicorn ps.cryptography ]))
  ];

  buildPhase = ''
    runHook preBuild

    # `airreceiver-carve.py` imports the emulator harness as a sibling module.
    cp ${./airreceiver-carve.py} carve.py
    cp ${./airreceiver_armemu.py} airreceiver_armemu.py

    python3 carve.py ${airreceiverSrc} -o "$out"

    runHook postBuild
  '';

  installPhase = "true";

  meta = {
    description = "Cast identity and CKS backend credentials carved from libAirReceiver.so";
    # The output is SoftMedia's material; it exists so this tree does not carry it.
    license = lib.licenses.unfree;
    platforms = lib.platforms.all;
  };
}
