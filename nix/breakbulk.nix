# The breakbulk port of the crane build: one derivation per Cargo unit.
#
# Experimental, beside crane rather than replacing it: `nix build .#bb.<attr>`.
# The attribute names at the bottom mirror flake.nix's packages/checks.
{ breakbulk
, system
, pkgs
, root
, rustToolchain
, gitRev
, buildNumber
, carveEnv
, ldacbt
, moonlightCommonC
, bluetoothFirmware
, electron
, widevineCdm
, windows
}:

let
  inherit (pkgs) lib;
  bb = breakbulk.lib;
  native = "x86_64-unknown-linux-gnu";
  msvc = "x86_64-pc-windows-msvc";

  bindgenEnv = {
    LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
    BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.glibc.dev}/include";
  };

  # castaway's own sysroot (nix/msvc-sysroot.nix), in the shape the windows preset
  # takes from `breakbulk.lib.xwinSdk`: the same xwin splat, so no second download.
  sdk = {
    includeDirs = [
      "${windows.sysroot}/crt/include"
      "${windows.sysroot}/sdk/include/ucrt"
      "${windows.sysroot}/sdk/include/um"
      "${windows.sysroot}/sdk/include/shared"
      "${windows.sysroot}/sdk/include/winrt"
    ];
    libDirs.x86_64 = [
      "${windows.sysroot}/crt/lib/x86_64"
      "${windows.sysroot}/sdk/lib/um/x86_64"
      "${windows.sysroot}/sdk/lib/ucrt/x86_64"
    ];
  };

  # The workspace members, for settings breakbulk only takes per package.
  members = [
    "castaway" "castaway-browser-fd" "castaway-bt-probe" "cast-registry" "cast-replay"
    "control-display" "castaway-core" "crypto-cast-auth" "crypto-fairplay"
    "crypto-playfair" "crypto-raop" "fcast-flatbuf" "hci-transport" "input-touch"
    "castaway-launcher" "ldac-sys" "moonlight-sys" "castaway-paths" "pipeline"
    "proto-airplay" "proto-bluetooth-audio" "proto-cast" "proto-dial" "proto-dlna"
    "proto-fcast" "proto-gamestream" "proto-matter" "proto-miracast" "proto-spotify"
    "sponsorblock" "substrate-hci" "substrate-l2cap" "substrate-mdns" "substrate-rtp"
    "substrate-rtsp" "substrate-sdp" "substrate-ssdp" "castaway-test-support"
    "castaway-update"
  ];

  # flake.nix `lavapipe` + `CASTAWAY_REQUIRE_FFMPEG`: a software GPU and ffmpeg on
  # PATH, with both tripwires set so a test that cannot get either fails.
  testEnv = {
    LD_LIBRARY_PATH = "${pkgs.vulkan-loader}/lib";
    WGPU_BACKEND = "vulkan";
    VK_DRIVER_FILES =
      "${pkgs.mesa}/share/vulkan/icd.d/lvp_icd.${pkgs.stdenv.hostPlatform.parsed.cpu.name}.json";
    CASTAWAY_REQUIRE_GPU = "1";
    CASTAWAY_REQUIRE_FFMPEG = "1";
  };

  repo = bb.mkRepository {
    inherit pkgs root;
    modules = [
      ({ repo, ... }: {
        workspaces.default = {
          toolchain = { rustc = rustToolchain; cargo = rustToolchain; };
          fmt.enable = true;
          test.runner = "nextest";

          targets.${native} = { };
          # The deploy artifact's three binaries, and nothing else: the rest of the
          # workspace has Linux-only dependency tables.
          targets.${msvc} = {
            packages = [ "castaway" "castaway-launcher" "castaway-bt-probe" ];
            # Windows binaries cannot run on the builder (crane: `doCheck = false`).
            tests = false;
          };

          variants = {
            # Everything on (D55), natively.
            default.targets = [ native ];
            # The Windows deploy artifact: Windows names what it wants rather than
            # subtracting (nix/windows.nix `mkCastaway`).
            windows = {
              targets = [ msvc ];
              noDefaultFeatures = true;
              features = [ "castaway/electron" "castaway/audio-out" ];
            };
            # The browser-less receiver the VM tests boot.
            portable = {
              targets = [ native ];
              packages = [ "castaway" ];
              noDefaultFeatures = true;
              checks = false;
            };
            clippy = {
              targets = [ native ];
              mode = "clippy";
              clippyArgs = [ "--deny" "warnings" ];
            };
          };
        };

        targets.${msvc} = {
          preset = bb.presets.windows { inherit sdk; crtStatic = true; };
          # nix/windows.nix `trailingFlags` and `CXXFLAGS`: third-party /WX code is
          # calibrated against MSVC's /W4, which clang-cl maps onto other warnings.
          cflags = [ "-Wno-error" "/EHsc" ];
        };

        packages = lib.genAttrs members (_: { test.env = testEnv; test.path = [ pkgs.ffmpeg_7 ]; })
          // {
          # Every env var below reaches only the named package's units — the thing
          # crane could not do, and why `CASTAWAY_GIT_REV` had to be kept off the deps
          # builds by hand.
          castaway = {
            env = {
              CASTAWAY_GIT_REV = gitRev;
              CASTAWAY_BUILD = buildNumber;
            };
            # Read by the app's tests (`include_bytes!`, the network-surface
            # freshness tests, the clippy.toml carve-out check).
            extraSources = [
              "clippy.toml"
              "docs/network-surface.md"
              "nix/network-surface.json"
              "crates/proto-airplay/tests/fixtures"
              "crates/proto-bluetooth-audio/tests/fixtures"
            ];
            test = { env = testEnv; path = [ pkgs.ffmpeg_7 ]; };

            # nix/linux-kiosk.nix's postInstall, as a stage after `install`: the fd
            # addon from its own product, the Electron/CDM wrapper, the browser host
            # app and the desktop entry.
            target.${native}.stages.kiosk = {
              after = "install";
              run = { input, pkgs, ... }:
                let
                  addon = repo.workspaces.default.target.${native}.packages.castaway-browser-fd;
                  runtimeLibs = lib.makeLibraryPath [
                    pkgs.vulkan-loader pkgs.libGL pkgs.wayland pkgs.libxkbcommon pkgs.libx11
                    pkgs.libxcursor pkgs.libxi pkgs.libxrandr pkgs.ffmpeg_7 pkgs.alsa-lib
                    pkgs.pipewire pkgs.openssl ldacbt
                  ];
                in
                pkgs.runCommand "castaway-kiosk" { nativeBuildInputs = [ pkgs.makeWrapper ]; } ''
                  cp -r --no-preserve=mode ${input} $out
                  chmod +x $out/bin/*
                  install -Dm755 ${addon}/lib/libcastaway_browser_fd.so $out/lib/libcastaway_browser_fd.so
                  wrapProgram $out/bin/castaway \
                    --set-default CASTAWAY_ELECTRON ${electron}/bin/electron \
                    --set-default CASTAWAY_BROWSER_APP $out/share/castaway/browser-host \
                    --set-default CASTAWAY_WIDEVINE_CDM ${widevineCdm} \
                    --set-default CASTAWAY_BROWSER_FD_ADDON $out/lib/libcastaway_browser_fd.so \
                    --prefix LD_LIBRARY_PATH : "${runtimeLibs}"
                  mkdir -p $out/share/castaway
                  cp -r --no-preserve=mode,ownership ${../browser-host} $out/share/castaway/browser-host
                '';
            };

            # nix/windows.nix `mkCastaway`'s postInstall: ffmpeg's DLLs, the launcher,
            # and the unmodified ECS tree. No `dontStrip` needed: breakbulk has no
            # implicit fixup, which is exactly the #348 bug crane had.
            target.${msvc} = {
              stages.deployTree = {
                after = "install";
                run = { input, pkgs, ... }:
                  let
                    launcher = repo.workspaces.default.target.${msvc}.variants.windows.packages.castaway-launcher;
                  in
                  pkgs.runCommand "castaway-windows-electron" { } (''
                    cp -r --no-preserve=mode ${input} $out
                    cp ${windows.ffmpeg}/bin/*.dll "$out/bin/"
                    cp ${launcher}/bin/launcher.exe "$out/bin/"
                  '' + windows.stageBrowser);
              };
              checks = {
                dll-closure.run = { input, ... }:
                  windows.mkBundleCheck (input // { pname = "castaway-windows-electron"; });
                authenticode.run = { input, ... }:
                  windows.mkAuthenticodeCheck (input // { pname = "castaway-windows-electron"; });
              };
            };
          };
          castaway-bt-probe.target.${msvc}.checks.dll-closure.run = { input, ... }:
            windows.mkBundleCheck (input // { pname = "castaway-bt-probe-windows"; });
          pipeline.test = {
            env = testEnv;
            path = [ pkgs.ffmpeg_7 ];
            sourceScope = [ "browser-host" ];
          };
          proto-cast.test = {
            env = testEnv;
            path = [ pkgs.ffmpeg_7 ];
            sourceScope = [ "browser-host" ];
          };

          hci-transport.env.CASTAWAY_FIRMWARE_DIR = bluetoothFirmware;
          cast-replay.env = {
            inherit (carveEnv)
              CASTAWAY_AIRSERVER_CARVE
              CASTAWAY_AIRSERVER_KEK_PERSON_FILE
              CASTAWAY_AIRSERVER_KEK_PASS_FILE
              CASTAWAY_AIRRECEIVER_CARVE;
          };
          crypto-raop.env.CASTAWAY_AIRPORT_KEY_FILE = carveEnv.CASTAWAY_AIRPORT_KEY_FILE;
          rs-matter.env.RS_MATTER_BUILD_MATTER_SECS = "0";

          ffmpeg-sys-next = {
            env.LIBCLANG_PATH = bindgenEnv.LIBCLANG_PATH;
            target.${native} = {
              nativeInputs = p: [ p.pkg-config ];
              inputs = p: [ p.ffmpeg_7 ];
              env.BINDGEN_EXTRA_CLANG_ARGS = bindgenEnv.BINDGEN_EXTRA_CLANG_ARGS;
            };
            target.${msvc}.env = {
              # The prebuilt LGPL DLLs; `ffmpeg-sys-next` takes this branch instead of
              # pkg-config.
              FFMPEG_DIR = windows.ffmpeg;
              # nix/windows.nix `hostCc`: cc-rs reads `crt-static` off the *Windows*
              # target even for the Linux helper this build script compiles and runs.
              "CC_${native}" = "${windows.hostCc}/bin/host-cc-no-static";
            };
          };
          # Both the -sys crates and the safe wrappers run pkg-config in build.rs.
          libspa = {
            nativeInputs = p: [ p.pkg-config ];
            inputs = p: [ p.pipewire ];
          };
          pipewire = {
            nativeInputs = p: [ p.pkg-config ];
            inputs = p: [ p.pipewire ];
          };
          libspa-sys = {
            nativeInputs = p: [ p.pkg-config ];
            inputs = p: [ p.pipewire ];
            env = bindgenEnv;
          };
          pipewire-sys = {
            nativeInputs = p: [ p.pkg-config ];
            inputs = p: [ p.pipewire ];
            env = bindgenEnv;
          };
          ldac-sys = {
            inputs = [ ldacbt ];
            env.LDACBT_LIB_DIR = "${ldacbt}/lib";
          };
          moonlight-sys = {
            inputs = p: [ p.openssl ];
            env.MOONLIGHT_COMMON_C_LIB_DIR = "${moonlightCommonC}/lib:${pkgs.openssl.out}/lib";
          };
        };
      })
    ];
  };

  ws = repo.workspaces.default;
  linux = ws.target.${native};
  win = ws.target.${msvc}.variants.windows.packages;
in
{
  inherit repo ws;
  inherit (repo) checks;

  castaway = linux.packages.castaway;
  castaway-portable = linux.variants.portable.packages.castaway;
  launcher = linux.packages.castaway-launcher;
  browser-fd = linux.packages.castaway-browser-fd;

  castaway-windows = win.castaway;
  launcher-windows = win.castaway-launcher;
  castaway-bt-probe-windows = win.castaway-bt-probe;
}
