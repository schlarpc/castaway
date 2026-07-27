# The Linux kiosk build with the browser in it — the one that can actually play YouTube.
#
# `packages.default` is the portable build: no renderer, no browser. That is the right
# default for CI and for proving the protocol stack, but it is not a receiver you can cast
# YouTube to, because DIAL only *launches* an app and the app is a web page (D27). Without
# this package the only way to get a browser on Linux was `cargo build --features cef`
# inside the devShell, which is not a thing you can deploy — while the Windows side has
# shipped `castaway-windows-cef` all along.
#
# Two things have to agree between build and run, and both are set here rather than left
# to the environment:
#   - CEF_PATH, because `initialize()` reads it at *runtime* to find the .pak/ICU/locales
#     resources, not just at build time to link libcef.
#   - LD_LIBRARY_PATH, so the loader finds libcef.so and the Vulkan/Wayland/X11 libraries
#     that winit and wgpu dlopen. libGL is in there because cefDist's bundled libGLESv2.so
#     links libGL.so.1; without it CEF's GPU process dies and wgpu's GL probe segfaults.
#   - CASTAWAY_WIDEVINE_PATH, when the CDM is available, so DRM-gated video plays. Also a
#     runtime lookup, and also silent when wrong — which is why the Rust side checks for
#     `manifest.json` rather than trusting the variable.
{ pkgs, craneLib, commonArgs, cefDist }:

let
  # The Widevine CDM, so DRM-gated YouTube content plays instead of failing silently.
  #
  # Unfree and non-redistributable, so this is a `tryEval`: a Nixpkgs without
  # `allowUnfree` still builds a working receiver, it just cannot play protected streams.
  # The alternative — a hard dependency — would make the whole package unbuildable for
  # anyone who has not accepted Google's terms, over a feature most casts never touch.
  #
  # nixpkgs lays it out the way Chrome does, so the directory holding `manifest.json` and
  # `_platform_specific/` is what Chromium's `--widevine-cdm-path` wants.
  widevine =
    let
      attempt = builtins.tryEval (pkgs.widevine-cdm.outPath or null);
    in
    if attempt.success && attempt.value != null then
      "${attempt.value}/share/google/chrome/WidevineCdm"
    else
      null;
  # Everything the `cef` feature drags in: it implies `render` and `hwaccel`, so this is
  # the ffmpeg/bindgen set plus CEF's own build tooling.
  cefArgs = {
    pname = "castaway-cef";
    cargoExtraArgs = "--package castaway --features cef";

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
  # Kept on one line with CEF_PATH rather than on its own continuation: an empty
  # `optionalString` between two backslash-continued lines leaves a blank line, which ends
  # the command and turns the next flag into a command of its own.
  widevineArg =
    if widevine == null then "" else "--set-default CASTAWAY_WIDEVINE_PATH ${widevine}";
in
craneLib.buildPackage (commonArgs // cefArgs // {
  # Its own dependency build: the feature set differs from the default package's, so it
  # cannot share those artifacts.
  cargoArtifacts = craneLib.buildDepsOnly (commonArgs // cefArgs // {
    pname = "castaway-cef-deps";
  });

  # The render/browser tests want a GPU and a display; the sandbox has neither.
  doCheck = false;

  # CEF re-execs this same binary for its subprocesses, so the wrapper has to be what
  # runs — a subprocess started from an unwrapped path would come up without CEF_PATH.
  postInstall = ''
    wrapProgram $out/bin/castaway \
      --set-default CEF_PATH ${cefDist} ${widevineArg} \
      --prefix LD_LIBRARY_PATH : "${runtimeLibs}:${cefDist}"
  '';

  meta = commonArgs.meta or { } // {
    description = "castaway with the CEF kiosk browser (the build that plays YouTube)";
    mainProgram = "castaway";
  };
})
