# castLabs ECS for Linux: the upstream prebuilt, made runnable on NixOS.
#
# D36 pins the same Electron on both platforms, so this is the Linux half of that pin
# rather than a convenience — nixpkgs' own `electron` is a different Chromium major, and
# using it for development would mean testing a browser we do not ship.
#
# The work is patchelf plus a library path, which is exactly what nixpkgs' `electron-bin`
# does for upstream Electron; this differs only in taking our pinned archive. Chromium
# dlopens a good deal of what it needs (Vulkan, Wayland, X11, GTK for dialogs), so the
# wrapper sets `LD_LIBRARY_PATH` rather than relying on the interpreter rewrite alone.
{ pkgs, src }:

let
  # Link-time and dlopen-time both. Kept as one list because Chromium moves libraries
  # between the two across versions, and a library that is only needed at dlopen time
  # fails as a *feature* silently disappearing rather than as a load error.
  runtimeLibs = pkgs.lib.makeLibraryPath (with pkgs; [
    alsa-lib
    at-spi2-atk
    at-spi2-core
    atk
    cairo
    cups
    dbus
    expat
    glib
    libdrm
    libgbm
    libglvnd
    libxkbcommon
    nspr
    nss
    pango
    stdenv.cc.cc.lib
    systemd # libudev, for device enumeration
    vulkan-loader
    wayland
    gtk3
    libx11
    libxcomposite
    libxdamage
    libxext
    libxfixes
    libxrandr
    libxcb
    libxcursor
    libxi
    libxtst
    libxscrnsaver
  ]);
in
pkgs.stdenv.mkDerivation {
  pname = "electron-ecs";
  version = "43.0.0+wvcus";

  inherit src;

  # A `file+https://` flake input arrives as an extensionless store path, so stdenv's
  # unpacker cannot guess the format from the name. Naming it explicitly is also the
  # honest thing: the archive *is* a zip regardless of what the store calls it.
  unpackPhase = ''
    runHook preUnpack
    unzip -q $src
    runHook postUnpack
  '';
  sourceRoot = ".";

  nativeBuildInputs = [ pkgs.unzip pkgs.autoPatchelfHook pkgs.makeWrapper ];
  buildInputs = with pkgs; [
    alsa-lib
    at-spi2-atk
    at-spi2-core
    atk
    cairo
    cups
    dbus
    expat
    glib
    libdrm
    libgbm
    libxkbcommon
    nspr
    nss
    pango
    stdenv.cc.cc.lib
    gtk3
    libx11
    libxcomposite
    libxdamage
    libxext
    libxfixes
    libxrandr
    libxcb
  ];

  # `chrome-sandbox` is setuid-root in a normal install and cannot be here — a Nix store
  # file cannot carry setuid. Chromium falls back to the *namespace* sandbox, which needs
  # no helper and is available on this kernel; see G86 for why running the browser
  # unsandboxed is not an option we are keeping.
  #
  # autoPatchelf would otherwise fail on the sandbox helper and the crashpad handler,
  # neither of which we invoke through a path it can rewrite.
  dontWrapGApps = true;

  installPhase = ''
    runHook preInstall

    mkdir -p $out/libexec/electron
    cp -r ./* $out/libexec/electron/
    chmod +x $out/libexec/electron/electron

    mkdir -p $out/bin
    makeWrapper $out/libexec/electron/electron $out/bin/electron \
      --prefix LD_LIBRARY_PATH : "${runtimeLibs}"

    runHook postInstall
  '';

  meta = with pkgs.lib; {
    description = "castLabs Electron for Content Security (Widevine-enabled Electron)";
    homepage = "https://github.com/castlabs/electron-releases";
    license = licenses.mit;
    platforms = [ "x86_64-linux" ];
    mainProgram = "electron";
  };
}
