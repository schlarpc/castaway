# The full Linux kiosk build — renderer, browser, sound, Bluetooth. This is
# `packages.default` on Linux, so it is what `nix run .` gives you.
#
# Every optional feature is on here except one. `cef` implies `render` and `hwaccel`;
# `audio-out` adds the A2DP decoders and a real PCM device; `bluetooth-socket` adds the
# `socket:N` transport beside the USB default. The exception is `ldac`, which is *not* a
# capability but an advertisement: the slot exists and the decoder does not (Q22), so a
# build with it on offers senders a codec it will then fail to decode, turning a session
# that would have fallen back to SBC into silence. It stays off until libldacdec lands.
#
# `packages.castaway-portable` is the old default: no renderer, no browser, nothing
# platform-specific. That is the right build for CI and for proving the protocol stack,
# but it is not a receiver you can cast YouTube to, because DIAL only *launches* an app
# and the app is a web page (D27).
#
# Two things have to agree between build and run, and both are set here rather than left
# to the environment:
#   - CEF_PATH, because `initialize()` reads it at *runtime* to find the .pak/ICU/locales
#     resources, not just at build time to link libcef.
#   - LD_LIBRARY_PATH, so the loader finds libcef.so and the Vulkan/Wayland/X11 libraries
#     that winit and wgpu dlopen. libGL is in there because cefDist's bundled libGLESv2.so
#     links libGL.so.1; without it CEF's GPU process dies and wgpu's GL probe segfaults.
#
# The Widevine CDM is *not* configured here. It ships inside `cefDist` (flake.nix), beside
# libcef.so, because that is the only directory Chromium looks in — see
# crates/pipeline/src/widevine.rs. This file used to set a `CASTAWAY_WIDEVINE_PATH` that
# fed a `--widevine-cdm-path` switch CEF does not have.
{ pkgs, craneLib, commonArgs, cefDist }:

let
  # Everything these features drag in: the ffmpeg/bindgen set (render + hwaccel + the
  # audio decoders), ALSA for the PCM device, and CEF's own build tooling.
  kioskArgs = {
    pname = "castaway";
    cargoExtraArgs = "--package castaway --features cef,audio-out,bluetooth-socket";

    nativeBuildInputs = [
      pkgs.pkg-config
      pkgs.makeWrapper
      # cef-dll-sys constructs a `cmake::Config` even where it builds no C++, so the
      # binaries have to exist. Its setup hook must not take over the build, though —
      # crane drives cargo, not cmake.
      pkgs.cmake
      pkgs.ninja
    ];

    buildInputs = [
      # 7.x to match `ffmpeg-next`/`ffmpeg-sys-next` 7.1 (nixpkgs defaults to 8.x).
      pkgs.ffmpeg_7
      pkgs.alsa-lib
    ];

    # `ffmpeg-sys-next` generates bindings with bindgen, which dlopens libclang and needs
    # the libc headers pointed out explicitly in a Nix env.
    LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
    BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.glibc.dev}/include";
    CEF_PATH = "${cefDist}";

    dontUseCmakeConfigure = true;
  };

  # dlopened at runtime, so they are wrapper-time rather than link-time inputs.
  runtimeLibs = pkgs.lib.makeLibraryPath [
    pkgs.vulkan-loader
    pkgs.libGL
    pkgs.wayland
    pkgs.libxkbcommon
    pkgs.libx11
    pkgs.libxcursor
    pkgs.libxi
    pkgs.libxrandr
    pkgs.ffmpeg_7
    pkgs.alsa-lib
  ];
in
craneLib.buildPackage (commonArgs // kioskArgs // {
  # Its own dependency build: the feature set differs from the portable package's, so it
  # cannot share those artifacts.
  cargoArtifacts = craneLib.buildDepsOnly (commonArgs // kioskArgs // {
    pname = "castaway-kiosk-deps";
  });

  # The render/browser tests want a GPU and a display; the sandbox has neither.
  doCheck = false;

  # CEF re-execs this same binary for its subprocesses, so the wrapper has to be what
  # runs — a subprocess started from an unwrapped path would come up without CEF_PATH.
  postInstall = ''
    wrapProgram $out/bin/castaway \
      --set-default CEF_PATH ${cefDist} \
      --prefix LD_LIBRARY_PATH : "${runtimeLibs}:${cefDist}"
  '';

  meta = commonArgs.meta or { } // {
    description = "castaway: the full Linux kiosk — renderer, CEF browser, audio out, Bluetooth";
    mainProgram = "castaway";
  };
})
