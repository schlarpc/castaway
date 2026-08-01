# Deploying to the physical Windows box, over SSH.
#
# The C6522QT-attached box is the only place the Windows slice is real (docs/cross-build.md,
# "Testing matrix"). It has no Nix store, so the loop is: build the archive here, wipe what is
# there, copy, verify the bits, launch, watch the log. Two things about that box shape this
# file, and both were measured rather than assumed:
#
#   1. **An SSH login lands in session 0.** A process started from SSH runs on the services
#      desktop, so its window is nowhere — launching `castaway.exe` straight from `ssh` puts
#      no pixels on the panel and looks, from here, exactly like a successful launch. The
#      console session is where the panel is, and `schtasks /IT` is what reaches it.
#   2. **Nothing sets `CASTAWAY_ELECTRON` on Windows.** The Linux artifact is `wrapProgram`ped
#      (nix/linux-kiosk.nix); the Windows tree is a flat directory with no wrapper, so the
#      browser subprocess would be looked up as bare `electron` on `PATH` and not found. The
#      launcher below supplies both paths relative to itself.
#
# The host is deliberately *not* baked in: `CASTAWAY_WINDOWS_HOST=user@host`. It is one box on
# one LAN, and the repo is not the place for it.
{ pkgs }:
let
  inherit (pkgs) lib;

  surface = builtins.fromJSON (builtins.readFile ./network-surface.json);

  # `netsh`/`New-NetFirewallRule` take a range as one rule, so unlike the Linux script — which
  # punches a hole per port — the media ranges collapse to a single rule each.
  portSpec = p:
    if p ? fixed then toString p.fixed
    else if p ? config then toString p.default
    else "${toString p.default_first}-${toString p.default_last}";

  # One rule per (protocol, port-spec), owners folded together: the RAOP and Cast media
  # ranges are the same 32 ports, and two rules for them would just be two rules.
  holes = lib.attrValues (lib.foldl'
    (acc: l:
      let
        proto = lib.toUpper l.transport;
        spec = portSpec l.port;
        k = "${proto}:${spec}";
      in
      acc // {
        ${k} = {
          inherit proto spec;
          owners = lib.unique ((acc.${k}.owners or [ ]) ++ [ l.owner ]);
        };
      })
    { }
    surface.listeners);

  ruleFor = h:
    let label = "${lib.toLower h.proto}/${h.spec} (${lib.concatStringsSep "+" h.owners})";
    in ''
      New-NetFirewallRule -DisplayName 'castaway ${label}' -Group 'castaway' -Direction Inbound -Action Allow -Protocol ${h.proto} -LocalPort '${h.spec}' -Profile Any | Out-Null
      Write-Host '  allow inbound ${label}'
    '';

  # Generated from nix/network-surface.json, which crates/app/src/surface.rs regenerates and a
  # test fails on drift — so this cannot silently fall behind the code that binds the sockets.
  firewallPs1 = pkgs.writeText "castaway-firewall.ps1" ''
    $ErrorActionPreference = 'Stop'
    # Rules are grouped so the whole surface can be removed as a unit — and so re-running is
    # idempotent rather than accumulating duplicates.
    Remove-NetFirewallRule -Group 'castaway' -ErrorAction SilentlyContinue
    if ($args -contains '-Close') {
      Write-Host "castaway's surface is closed"
      exit 0
    }
    ${lib.concatMapStrings ruleFor holes}
    Write-Host "castaway's surface (${toString (builtins.length holes)} rules) is open"
    Write-Host "close it again with: castaway-windows-firewall --close"
  '';

  # The launcher. It lives beside the exe rather than being inlined into the scheduled task's
  # `/TR`, because a task action carrying `cmd /c` plus redirection plus two `set`s has to
  # survive bash, ssh, cmd and schtasks quoting in series, and every layer disagrees.
  runCmd = pkgs.writeText "castaway-run.cmd" ''
    @echo off
    setlocal
    rem `%~dp0` keeps a trailing backslash, which `cd /d "..."` chokes on — hence the `.`.
    cd /d "%~dp0."
    rem The Windows artifact has no wrapper (see the header): point the receiver at the
    rem browser tree that shipped beside it, or it looks for bare `electron` on PATH.
    set "CASTAWAY_ELECTRON=%~dp0browser\electron.exe"
    set "CASTAWAY_BROWSER_APP=%~dp0browser-host"
    if exist "%~dp0castaway.log" del /q "%~dp0castaway.log"
    if exist "%~dp0castaway.exit" del /q "%~dp0castaway.exit"
    castaway.exe > "%~dp0castaway.log" 2>&1
    rem Captured before the `echo`, which resets %errorlevel% to 0 on its way out.
    set "rc=%errorlevel%"
    echo [run.cmd] castaway.exe exited with code %rc% >> "%~dp0castaway.log"
    rem The exit status has to cross back to the Linux side somehow, and a scheduled task's
    rem is not worth reading: `schtasks /query` reports the *task*, which succeeded in
    rem starting the thing that failed.
    (echo %rc%)> "%~dp0castaway.exit"
  '';

  # Streaming the log back. `Get-Content -Wait` would do, except for two things it gets wrong
  # here: it takes a lock the writer can trip over, and it never returns — so a receiver that
  # died on startup leaves you staring at a prompt that looks like it is still working.
  # Opening with FileShare.ReadWrite fixes the first; polling for the process fixes the second.
  tailPs1 = pkgs.writeText "castaway-tail.ps1" ''
    $ErrorActionPreference = 'Stop'
    $log = Join-Path $PSScriptRoot 'castaway.log'
    $deadline = (Get-Date).AddSeconds(30)
    while (-not (Test-Path -LiteralPath $log)) {
      if ((Get-Date) -gt $deadline) { Write-Error 'castaway.log never appeared'; exit 1 }
      Start-Sleep -Milliseconds 200
    }
    $fs = [System.IO.File]::Open($log, 'Open', 'Read', 'ReadWrite')
    $sr = New-Object System.IO.StreamReader($fs)
    try {
      while ($true) {
        $line = $sr.ReadLine()
        if ($null -ne $line) { Write-Output $line; continue }
        if (-not (Get-Process -Name castaway -ErrorAction SilentlyContinue)) {
          # Drain whatever it managed to write between the last read and dying.
          while ($null -ne ($line = $sr.ReadLine())) { Write-Output $line }
          break
        }
        Start-Sleep -Milliseconds 150
      }
    } finally { $sr.Dispose(); $fs.Dispose() }

    # Hand the receiver's own exit code back, so a launch that died on startup is a failed
    # deploy rather than a successful one that happens to have printed an error.
    $codeFile = Join-Path $PSScriptRoot 'castaway.exit'
    $deadline = (Get-Date).AddSeconds(5)
    while (-not (Test-Path -LiteralPath $codeFile) -and (Get-Date) -lt $deadline) {
      Start-Sleep -Milliseconds 100
    }
    $rc = 0
    if (Test-Path -LiteralPath $codeFile) {
      $rc = [int](Get-Content -LiteralPath $codeFile -Raw).Trim()
    }
    Write-Output "--- castaway.exe exited with code $rc ---"
    exit $rc
  '';

  # Both scripts talk to the same box the same way.
  preamble = ''
    host="''${CASTAWAY_WINDOWS_HOST:-}"
    if [ -z "$host" ]; then
      cat >&2 <<'EOF'
    error: CASTAWAY_WINDOWS_HOST is unset.

      export CASTAWAY_WINDOWS_HOST=user@panel-address

    The address is not in the repo on purpose: it is one box on one LAN.
    EOF
      exit 1
    fi
    ssh_opts=(-o BatchMode=yes -o ConnectTimeout=10 -o LogLevel=ERROR)
    # OpenSSH-for-Windows hands the command string to cmd.exe, which does no POSIX
    # word-splitting — so these are single pre-quoted cmd command lines, not argv.
    # shellcheck disable=SC2029 # expanding here is the point: we build cmd command lines.
    on_box() { ssh "''${ssh_opts[@]}" "$host" "$1"; }
    # cmd.exe reports a caret-free CRLF; strip it or every captured value ends in \r.
    unix() { tr -d '\r'; }
    # `certutil -hashfile` prints a header line, then the digest — historically spaced
    # between bytes, contiguous on current builds, and lowercase where sha256sum agrees.
    # Normalising case here rather than at each comparison: a case mismatch does not look
    # like a bug, it looks like a corrupted transfer.
    remote_sha256() {
      on_box "certutil -hashfile \"$1\" SHA256 2>nul" \
        | unix | tr -d ' ' | tr '[:upper:]' '[:lower:]' | grep -Ex '[0-9a-f]{64}' || true
    }

    # Prove the box answers before anything else. `pipefail` turns an unreachable host into
    # a failed command substitution somewhere further down, and errexit then ends the script
    # with no output at all — which reads as "it worked".
    if ! ssh "''${ssh_opts[@]}" "$host" 'exit 0' >/dev/null 2>&1; then
      echo "error: $host did not answer over ssh" >&2
      exit 1
    fi
  '';

  firewall = pkgs.writeShellApplication {
    name = "castaway-windows-firewall";
    runtimeInputs = [ pkgs.openssh ];
    text = preamble + ''
      close=""
      case "''${1:-}" in
        --close) close=" -Close" ;;
        "") ;;
        *) echo "usage: castaway-windows-firewall [--close]" >&2; exit 1 ;;
      esac

      # Elevation is not optional here, and a non-elevated session fails per-rule with a
      # wall of PowerShell rather than one line — so check it up front.
      if ! on_box 'net session >nul 2>&1' ; then
        echo "error: the SSH session on $host is not elevated; New-NetFirewallRule will fail" >&2
        exit 1
      fi

      scp "''${ssh_opts[@]}" -q ${firewallPs1} "$host:castaway-firewall.ps1"
      on_box 'powershell -NoProfile -ExecutionPolicy Bypass -File castaway-firewall.ps1'"$close"
      on_box 'del /q castaway-firewall.ps1'
    '';
  };

  deploy = pkgs.writeShellApplication {
    name = "castaway-deploy-windows";
    runtimeInputs = [ pkgs.openssh pkgs.coreutils ];
    text = preamble + ''
      artifact="castaway-windows-electron"
      force=""
      no_launch=""
      for arg in "$@"; do
        case "$arg" in
          --force) force=1 ;;
          --no-launch) no_launch=1 ;;
          --*) echo "usage: castaway-deploy-windows [--force] [--no-launch] [ARTIFACT]" >&2
               exit 1 ;;
          *) artifact="$arg" ;;
        esac
      done

      say() { printf '\n==> %s\n' "$*"; }
      die() { printf '\nerror: %s\n' "$*" >&2; exit 1; }

      say "building .#$artifact"
      # The directory output and the zip are the same build; asking for both costs nothing
      # and gives us a local copy of castaway.exe to hash the deployed one against.
      tree=$(nix build --no-link --print-out-paths ".#$artifact")
      store=$(nix build --no-link --print-out-paths ".#$artifact.archive")
      zip="$store/$artifact.zip"
      [ -f "$zip" ] || die "no archive at $zip"
      [ -f "$tree/bin/castaway.exe" ] || die "$artifact has no bin/castaway.exe"
      zip_sha=$(sha256sum "$zip" | cut -d' ' -f1)
      exe_sha=$(sha256sum "$tree/bin/castaway.exe" | cut -d' ' -f1)
      printf '    %s (%s bytes)\n' "$zip" "$(stat -c%s "$zip")"

      home_dir=$(on_box 'echo %USERPROFILE%' | unix)
      home_dir="''${home_dir%"''${home_dir##*[![:space:]]}"}"
      [ -n "$home_dir" ] || die "could not resolve %USERPROFILE% on $host"
      user=$(on_box 'echo %USERNAME%' | unix)
      user="''${user%"''${user##*[![:space:]]}"}"
      root="$home_dir\\castaway"
      dest="$root\\$artifact"
      stamp="$dest\\.deployed-sha256"

      # Is the box already carrying exactly these bits? Two independent answers have to
      # agree: the stamp this script wrote after a verified extract, and a fresh hash of
      # the exe itself. The stamp alone would survive someone editing the tree; the exe
      # hash alone says nothing about the 235 MB of browser beside it.
      fresh=""
      if [ -z "$force" ]; then
        have_stamp=$(on_box "type \"$stamp\" 2>nul" | unix | tr -d '[:space:]' || true)
        have_exe=$(remote_sha256 "$dest\\castaway.exe")
        if [ "$have_stamp" = "$zip_sha" ] && [ "$have_exe" = "$exe_sha" ]; then
          fresh=1
          say "already deployed (matching stamp and castaway.exe) — skipping transfer"
          echo "    pass --force to re-copy anyway"
        fi
      fi

      say "stopping anything running on $host"
      # /T so the Electron children go with the parent; without them the tree stays locked
      # and the delete below fails in a way that looks like a permissions problem.
      # "not found" is the normal case on a box that is not currently running anything;
      # only what was actually killed is worth a line.
      # `2>&1` inside the cmd line, not outside: taskkill's "not found" goes to *its* stderr,
      # which ssh forwards straight to ours, where the filter below never sees it.
      on_box 'schtasks /end /TN castaway >nul 2>&1 & taskkill /F /T /IM castaway.exe 2>&1 & taskkill /F /T /IM electron.exe 2>&1' \
        | unix | grep -Ev '^(INFO: No tasks|ERROR: The process)' || true
      for _ in 1 2 3 4 5 6 7 8 9 10; do
        left=$(on_box 'tasklist /FI "IMAGENAME eq castaway.exe" /FO CSV /NH & tasklist /FI "IMAGENAME eq electron.exe" /FO CSV /NH' \
          | unix | grep -c '^"' || true)
        [ "$left" -eq 0 ] && break
        sleep 1
      done
      [ "$left" -eq 0 ] || die "$left castaway/electron process(es) survived taskkill on $host"

      if [ -z "$fresh" ]; then
        say "wiping $root"
        on_box "if exist \"$root\" rmdir /s /q \"$root\"" || true
        # rmdir reports success while leaving the tree behind if a handle is open, so the
        # only trustworthy check is asking again. A stale tree is the exact failure this
        # script exists to make impossible.
        on_box "if exist \"$root\" exit 1" \
          || die "$root still exists after rmdir — something holds a handle on it"

        say "copying $(basename "$zip") to $host"
        scp "''${ssh_opts[@]}" "$zip" "$host:castaway-deploy.zip"

        got=$(remote_sha256 'castaway-deploy.zip')
        [ "$got" = "$zip_sha" ] \
          || die "archive hash mismatch after transfer: box says '$got', built '$zip_sha'"

        say "extracting into $root"
        # bsdtar ships in Windows since 1809 and reads zip; there is no unzip on the box.
        on_box "mkdir \"$root\" && tar -xf castaway-deploy.zip -C \"$root\"" \
          || die "extract failed"
        on_box 'del /q castaway-deploy.zip' || true

        on_box "if not exist \"$dest\\castaway.exe\" exit 1" \
          || die "$dest\\castaway.exe missing after extract"
        got=$(remote_sha256 "$dest\\castaway.exe")
        [ "$got" = "$exe_sha" ] \
          || die "deployed castaway.exe hashes $got, built one hashes $exe_sha"

        # Written last, and only once everything above passed: the stamp means "this exact
        # archive extracted cleanly here", so a half-finished deploy leaves no stamp and the
        # next run cannot take the fast path.
        # Parenthesised because cmd binds a trailing digit to the redirect: `echo abc1> f`
        # writes "abc" and redirects handle 1, and a hex digest ends in a digit more often
        # than not — which would have made the fast path silently never match.
        on_box "(echo $zip_sha)> \"$stamp\"" || die "could not write $stamp"
        say "deployed and verified: $dest"
      fi

      # sftp mangles backslashes; it takes the same path with forward slashes, and so does
      # every Windows API the other side of it.
      dest_fwd="''${dest//\\//}"
      scp "''${ssh_opts[@]}" -q ${runCmd} "$host:$dest_fwd/run.cmd"
      scp "''${ssh_opts[@]}" -q ${tailPs1} "$host:$dest_fwd/tail.ps1"

      if [ -n "$no_launch" ]; then
        say "not launching (--no-launch); run.cmd is staged at $dest"
        exit 0
      fi

      say "launching on the console session"
      # /IT is the whole point: it runs the task with the interactive token of the logged-on
      # user, which is the only way from here to reach the desktop the panel is showing.
      # A plain `ssh ... run.cmd` lands in session 0 and renders to nothing.
      on_box "schtasks /create /TN castaway /TR \"$dest\\run.cmd\" /SC ONCE /ST 23:59 /RU $user /IT /F" \
        | unix | grep -v '^WARNING: Task may not run' || true
      on_box 'schtasks /run /TN castaway' | unix
      for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
        up=$(on_box 'tasklist /FI "IMAGENAME eq castaway.exe" /FO CSV /NH' | unix | grep -c '^"' || true)
        [ "$up" -gt 0 ] && break
        sleep 1
      done
      [ "$up" -gt 0 ] || die "castaway.exe never appeared; check $dest\\castaway.log"

      say "streaming $dest\\castaway.log  (Ctrl-C detaches; the panel keeps running)"
      echo
      # -t so Ctrl-C reaches the remote tail instead of orphaning a PowerShell on the box.
      ssh -t "''${ssh_opts[@]}" "$host" \
        "powershell -NoProfile -ExecutionPolicy Bypass -File \"$dest\\tail.ps1\""
    '';
  };
in
{ inherit deploy firewall; }
