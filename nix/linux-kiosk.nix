# The full Linux kiosk build — renderer, browser, sound, Bluetooth, GameStream. This is
# `packages.default` on Linux, so it is what `nix run .` gives you.
#
# Every optional feature is on here. `electron` implies `render` and `hwaccel`;
# `audio-out` adds the A2DP decoders and a real PCM device; `bluetooth-socket` adds the
# `socket:N` transport beside the USB default; `gamestream` links moonlight-common-c so
# the GameStream client can actually stream — which makes *this artifact* GPL-3.0-bound
# (D37): fine for the panel it runs on, but it is why `castaway-portable` and the MIT
# source tree stay clean of it. `ldac` links Sony's `libldacBT` for the one A2DP codec
# ffmpeg cannot decode (#14) — Apache-2.0, so unlike GameStream it binds nothing.
#
# `ldac` used to be the one exception here, and the reason is worth keeping: the feature
# bound nothing, so a build with it on advertised a codec it would then fail to decode and
# turned a session that should have fallen back to SBC into silence (#14). That cannot
# recur — `can_decode` asks the library for a handle rather than reading the flag.
#
# Note what having the feature on does *not* do: advertise the endpoint. LDAC is in
# `bluetooth::OPT_IN`, so this artifact carries a working decoder and still offers senders
# the same four codecs as before until `codecs = ["ldac", "sbc"]` is in the config. It sits
# first in preference order, so switching it on by default would not add an option — it
# would change what every capable sender negotiates, on a panel nobody is watching, before
# one has ever streamed to it.
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
{ pkgs, craneLib, commonArgs, baseCargoArtifacts, depsOnlyFrom, gitRev, buildNumber, electron, widevineCdm, moonlightCommonC, ldacbt
, bluetoothFirmware }:

let
  # Everything these features drag in: the ffmpeg/bindgen set (render + hwaccel + the
  # audio decoders) and ALSA for the PCM device.
  kioskArgs = {
    pname = "castaway";
    # No `--features`: the default set *is* this list now (D55). Naming it here as well
    # would be two lists that have to agree, which is the drift this change exists to end.
    #
    # `castaway-browser-fd` is named as a second *package* rather than picked up as a
    # dependency, and that is the whole of #308: cargo emits a package's `cdylib` only
    # when the package is a build target, so `--package castaway` alone built the rlib
    # half (which the spawner uses for the soname constant) and no `.so` at all. Crane
    # then installed exactly what was built, and every nix-built deploy ran the
    # `pidfd_getfd` fallback — logging so, and nobody reading it.
    cargoExtraArgs = "--package castaway --package castaway-browser-fd";

    nativeBuildInputs = [
      pkgs.pkg-config
      pkgs.makeWrapper
    ];

    buildInputs = [
      # 7.x to match `ffmpeg-next`/`ffmpeg-sys-next` 7.1 (nixpkgs defaults to 8.x).
      pkgs.ffmpeg_7
      pkgs.alsa-lib
      # libpipewire for the native output backend (`audio-pipewire`), which is what
      # makes the settings screen's device list real sinks rather than ALSA shims.
      pkgs.pipewire
      # Sony's LDAC library (`ldac`). Ours rather than `pkgs.ldacbt`, which under this
      # nixpkgs pin is built encoder-only — see nix/ldacbt.nix.
      ldacbt
    ];

    # `ffmpeg-sys-next` generates bindings with bindgen, which dlopens libclang and needs
    # the libc headers pointed out explicitly in a Nix env.
    LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
    BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.glibc.dev}/include";

    # Where `moonlight-sys`'s build.rs finds the linked GameStream core (D37): the
    # library's own archives, and OpenSSL, whose libcrypto PlatformCrypto.c needs.
    MOONLIGHT_COMMON_C_LIB_DIR =
      "${moonlightCommonC}/lib:${pkgs.openssl.out}/lib";

    # Where `ldac-sys`'s build.rs finds `libldacBT` (#14). Without it the crate emits no
    # link directive at all, so this artifact would build and then fail to resolve
    # `ldacBT_decode` — which is the honest failure, but only if the variable is set here.
    LDACBT_LIB_DIR = "${ldacbt}/lib";
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
    pkgs.pipewire
    # moonlight-common-c's libcrypto. The linker's rpath already covers it; this keeps
    # the wrapper's view of the world complete if the store path moves under a copy.
    pkgs.openssl
    # Same for libldacBT, which unlike moonlight-common-c is linked dynamically.
    ldacbt
  ];
in
craneLib.buildPackage (commonArgs // kioskArgs // {
  # Its own dependency tree: the feature set differs from the portable package's, so the
  # crates the features drag in (ffmpeg-sys, bindgen, pipewire, …) cannot come from those
  # artifacts — but everything shared can, so this extends them instead of starting from
  # an empty target dir.
  cargoArtifacts = depsOnlyFrom baseCargoArtifacts (commonArgs // kioskArgs // {
    pname = "castaway-kiosk";
  });

  # The revision the idle screen's footer shows. On the final build only — in
  # `kioskArgs`/`commonArgs` it would reach the deps build above and invalidate every
  # compiled dependency at each commit.
  CASTAWAY_GIT_REV = gitRev;
  # The ordering half of the same identity (#343): which commit, and where it sits in
  # the history. Final build only, for the same reason as the revision.
  CASTAWAY_BUILD = buildNumber;

  # Same gap as the Windows artifact had: the firmware directory was named only in the
  # devShell, so the kiosk that ships embedded none of it (architecture 11.3b).
  CASTAWAY_FIRMWARE_DIR = bluetoothFirmware;

  # The render/browser tests want a GPU and a display; the sandbox has neither.
  doCheck = false;

  # The receiver locates Electron, the host app, and the CDM through these env vars, so
  # the wrapper has to be what runs — an unwrapped start would come up browser-less.
  postInstall = ''
    # The fd addon the host app dlopens to hand frame descriptors back over the control
    # socket (#271). Crane installs cdylibs into $out/lib, so this is a check that the
    # build really produced one rather than a step that makes it appear: without it the
    # receiver degrades to `pidfd_getfd` *silently enough* — one debug line — that this
    # went unnoticed through a whole release (#308). A rename or a dropped `--package`
    # fails the build here instead.
    addon=$out/lib/libcastaway_browser_fd.so
    test -f "$addon" || {
      echo "the fd addon is not in this build: $addon missing (see #308)" >&2
      exit 1
    }

    wrapProgram $out/bin/castaway \
      --set-default CASTAWAY_ELECTRON ${electron}/bin/electron \
      --set-default CASTAWAY_BROWSER_APP $out/share/castaway/browser-host \
      --set-default CASTAWAY_WIDEVINE_CDM ${widevineCdm} \
      --set-default CASTAWAY_BROWSER_FD_ADDON "$addon" \
      --prefix LD_LIBRARY_PATH : "${runtimeLibs}"

    # The browser host app travels with the binary. It is ours and dependency-free, so
    # this is a copy rather than a node_modules tree.
    mkdir -p $out/share/castaway
    # `--no-preserve=mode` plus a chmod: files copied straight from the store arrive
    # read-only, and the toolchain-reference stripper rewrites in place, so it fails on
    # them with a permission error that names a temp file rather than the cause.
    cp -r --no-preserve=mode,ownership ${../browser-host} $out/share/castaway/browser-host
    chmod -R u+w $out/share/castaway/browser-host

    # The icon, the way Wayland actually delivers one: the kiosk window sets
    # app_id "castaway" (pipeline::kiosk), the compositor looks for a desktop
    # entry of that name, and the entry's Icon= resolves through the hicolor
    # theme. X11 gets the icon off the window itself, but keeps WM_CLASS
    # "castaway" so the same entry matches there too. The rasters are checked
    # in, generated from castaway-icon.svg by the pipeline `icon_render`
    # example — this only installs them.
    for size in 16 24 32 48 64 128 256; do
      install -Dm444 crates/pipeline/assets/brand/icon/castaway-$size.png \
        $out/share/icons/hicolor/''${size}x''${size}/apps/castaway.png
    done
    install -Dm444 crates/pipeline/assets/brand/castaway-icon.svg \
      $out/share/icons/hicolor/scalable/apps/castaway.svg

    mkdir -p $out/share/applications
    cat > $out/share/applications/castaway.desktop <<'EOF'
    [Desktop Entry]
    Type=Application
    Name=castaway
    Comment=Universal cast receiver
    Exec=castaway
    Icon=castaway
    Categories=AudioVideo;Video;
    StartupWMClass=castaway
    EOF
  '';

  # The marker the NixOS module keys its hardening on (#246): this artifact carries
  # Chromium, whose sandbox model needs named allowances (`pkey_*`, `chroot`,
  # user namespaces, a real /tmp for the X socket) that a browser-less package
  # must not be granted. The module reads `passthru.castawayBrowser` rather than
  # guessing from the package name, so a custom kiosk build gets the same treatment.
  passthru = (commonArgs.passthru or { }) // {
    castawayBrowser = true;
  };

  meta = commonArgs.meta or { } // {
    description = "castaway: the full Linux kiosk — renderer, Electron browser, audio out, Bluetooth";
    mainProgram = "castaway";
  };
})
