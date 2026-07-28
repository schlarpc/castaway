# The full Linux kiosk build — renderer, browser, sound, Bluetooth. This is
# `packages.default` on Linux, so it is what `nix run .` gives you.
#
# Every optional feature is on here except one. `electron` implies `render` and `hwaccel`;
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
# What has to agree between build and run is now just *where the browser is*: the wrapper
# pins CASTAWAY_ELECTRON and CASTAWAY_BROWSER_APP so the receiver finds its subprocess
# without a devshell, and CASTAWAY_WIDEVINE_CDM so it can pre-stage a CDM into the
# browser profile on first run (G46's offline property, under D36's mechanism).
#
# LD_LIBRARY_PATH is still set, but only for *our* binary now — the Vulkan/Wayland/X11
# libraries winit and wgpu dlopen. The browser brings its own, because it is a separate
# process with its own wrapper.
{ pkgs, craneLib, commonArgs, electron, widevineCdm }:

let
  # Everything these features drag in: the ffmpeg/bindgen set (render + hwaccel + the
  # audio decoders), ALSA for the PCM device, and CEF's own build tooling.
  kioskArgs = {
    pname = "castaway";
    cargoExtraArgs = "--package castaway --features electron,audio-out,bluetooth-socket";

    nativeBuildInputs = [
      pkgs.pkg-config
      pkgs.makeWrapper
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
      --set-default CASTAWAY_ELECTRON ${electron}/bin/electron \
      --set-default CASTAWAY_BROWSER_APP $out/share/castaway/browser-host \
      --set-default CASTAWAY_WIDEVINE_CDM ${widevineCdm} \
      --prefix LD_LIBRARY_PATH : "${runtimeLibs}"

    # The browser host app travels with the binary. It is ours and dependency-free, so
    # this is a copy rather than a node_modules tree.
    mkdir -p $out/share/castaway
    cp -r ${../browser-host} $out/share/castaway/browser-host
  '';

  meta = commonArgs.meta or { } // {
    description = "castaway: the full Linux kiosk — renderer, CEF browser, audio out, Bluetooth";
    mainProgram = "castaway";
  };
})
