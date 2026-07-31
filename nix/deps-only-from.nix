# `craneLib.buildDepsOnly`, except it *extends* an existing artifact tree instead of
# starting from an empty target dir. Crane hardcodes `cargoArtifacts = null` inside
# `buildDepsOnly` — its output is meant to be the base of every chain — so a deps build
# that itself starts from another deps build has to compose the same public pieces:
# dummy sources (manifests and stubbed-out crate bodies, so real source changes cannot
# invalidate it), the shared vendor dir, inherited artifacts in, compressed artifacts
# out. The command sequence mirrors `buildDepsOnly`'s: `check` first to warm cargo's
# fingerprints, then `build`; `test --no-run` in the check phase so dev-dependencies
# land in the cache too.
#
# Used by the feature-set dependency trees (kiosk, audio, hwaccel, the Windows feature
# variants), which differ from their base tree only by what the features drag in —
# ffmpeg-sys, bindgen and friends — and which previously recompiled every shared
# dependency from scratch. Cargo recompiles anything whose unified feature set differs
# from the base tree's; everything else is reused as-is.
{ craneLib, lib }:

baseArtifacts: args:
let
  doCheck = args.doCheck or true;
  cargoExtraArgs = args.cargoExtraArgs or "--locked";
in
craneLib.mkCargoDerivation (builtins.removeAttrs args [ "cargoExtraArgs" ] // {
  src = craneLib.mkDummySrc args;
  cargoArtifacts = baseArtifacts;
  cargoVendorDir = args.cargoVendorDir or (craneLib.vendorCargoDeps args);
  pnameSuffix = "-deps";
  inherit doCheck;
  doInstallCargoArtifacts = true;
  buildPhaseCargoCommand = ''
    cargoWithProfile check ${cargoExtraArgs}${lib.optionalString doCheck " --all-targets"}
    cargoWithProfile build ${cargoExtraArgs}
  '';
  checkPhaseCargoCommand = "cargoWithProfile test ${cargoExtraArgs} --no-run";
  env = (args.env or { }) // { CRANE_BUILD_DEPS_ONLY = 1; };
})
