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
#   2. **The launcher supplies no paths.** It used to have to: the Windows tree is flat with
#      no wrapper, so a receiver that relied on `$CASTAWAY_ELECTRON` could not find its own
#      browser. That is fixed in the receiver instead — it resolves the browser, the host app
#      and the CDM beside its own executable — and `run.cmd` is left bare so a regression
#      shows up here rather than being papered over by the deploy script.
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

  # `run.cmd` is gone with #346. It existed to redirect the receiver's output into a log and
  # carry its exit status back across four layers of quoting; the launcher (#342) does both,
  # and it is the only thing the scheduled task points at now. Its history is in this file's
  # git log if the redirection trick is ever wanted again.

  # Streaming the shared log back. `Get-Content -Wait` would do, except that it takes a lock
  # the writer can trip over; opening with FileShare.ReadWrite fixes that.
  #
  # It follows the *launcher's* lifetime, not the receiver's, and that is the change #346
  # makes to what a tail means. A receiver that dies on startup no longer ends the stream —
  # the launcher restarts it, forever, and says so. The exit code this used to hand back went
  # with `run.cmd`: the launcher's own lines carry it now (`version a1a1a1a exited with code
  # 3 after 0.4s`), which is a thing to read rather than a number to infer.
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
        if (-not (Get-Process -Name launcher -ErrorAction SilentlyContinue)) {
          # Drain whatever it managed to write between the last read and dying.
          while ($null -ne ($line = $sr.ReadLine())) { Write-Output $line }
          break
        }
        Start-Sleep -Milliseconds 150
      }
    } finally { $sr.Dispose(); $fs.Dispose() }

    # Reaching here means the *launcher* stopped, which on a kiosk is a failure by
    # definition: it restarts everything else, so nothing restarts it but a logon.
    Write-Output '--- the launcher is no longer running ---'
    exit 1
  '';

  # What a version directory on the box actually contains, file by file.
  #
  # The whole point of the incremental deploy: 561 MB of tree, of which `castaway.exe` is
  # 62 MB and everything else — Electron at 358 MB, the ffmpeg DLLs, the CDM — is
  # byte-identical from one iteration to the next. Sending the archive every time costs
  # 235 MB to deliver a change that is almost always in one file.
  #
  # Hashing the whole tree on the box takes a few seconds and answers that exactly, with
  # no bookkeeping to go stale: the box says what it has, and the difference against the
  # store tree is what travels. Paths come back relative and slash-separated so the two
  # sides can be compared as plain text.
  manifestPs1 = pkgs.writeText "castaway-manifest.ps1" ''
    param([Parameter(Mandatory = $true)][string]$Dir)
    $ErrorActionPreference = 'Stop'
    if (-not (Test-Path -LiteralPath $Dir)) { exit 2 }
    $root = (Resolve-Path -LiteralPath $Dir).Path.TrimEnd('\') + '\'
    $files = @(Get-ChildItem -LiteralPath $root -Recurse -File -Force)
    $done = 0
    foreach ($f in $files) {
      # A file that cannot be read — still flushing behind the copy that just made it, or
      # held by something — must not end the listing early. A manifest cut short is
      # indistinguishable from "the box has almost nothing", which reads as "send
      # everything": a silently wrong answer where a refusal was wanted.
      try {
        $hash = (Get-FileHash -LiteralPath $f.FullName -Algorithm SHA256).Hash.ToLower()
      } catch {
        continue
      }
      $rel = $f.FullName.Substring($root.Length).Replace('\', '/')
      "$hash $rel"
      $done++
    }
    # The trailer is the point. The reader checks it arrived *and* that the count matches
    # what it parsed, so a listing truncated anywhere — a dropped connection, a file that
    # could not be read — is caught rather than believed. Without it a partial manifest
    # cost a 235 MB transfer and looked like a considered decision.
    "MANIFEST-COMPLETE $done $($files.Count)"
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

  # Handing a USB device to castaway's own stack, which on Windows means taking it away
  # from Windows'. Two steps because they fail differently: installing the package is a
  # signing problem, binding it to the device is a *ranking* problem, and conflating them
  # made the first success look like a failure.
  winusb = pkgs.writeShellApplication {
    name = "castaway-windows-winusb";
    runtimeInputs = [ pkgs.openssh ];
    text = preamble + ''
      hwid="''${1:-USB\\VID_8087&PID_0033}"
      case "''${1:-}" in
        --undo)
          scp "''${ssh_opts[@]}" -q ${./winusb-bind.ps1} "$host:winusb-bind.ps1"
          on_box 'powershell -NoProfile -ExecutionPolicy Bypass -File winusb-bind.ps1 -Undo'
          exit 0 ;;
      esac

      if ! on_box 'net session >nul 2>&1'; then
        echo "error: the SSH session on $host is not elevated; driver install will fail" >&2
        exit 1
      fi

      scp "''${ssh_opts[@]}" -q ${./winusb-bind.ps1} "$host:winusb-bind.ps1"
      scp "''${ssh_opts[@]}" -q ${./winusb-force.ps1} "$host:winusb-force.ps1"
      # Step one installs the package and *may* bind it; step two forces the bind when
      # ranking refused, which for a self-signed package against an inbox WHQL driver is
      # always. Running both unconditionally is idempotent and one round trip cheaper than
      # deciding.
      on_box "powershell -NoProfile -ExecutionPolicy Bypass -File winusb-bind.ps1 -HardwareId '$hwid'" || true
      on_box "powershell -NoProfile -ExecutionPolicy Bypass -File winusb-force.ps1 -HardwareId '$hwid'"
    '';
  };

  # Everything that knows about the versioned layout (#346) shares these. The install root
  # moved from `%USERPROFILE%\castaway` to `%LOCALAPPDATA%\castaway` with the layout, which
  # is where `castaway-paths` already puts Windows state — one directory for the receiver's
  # files and the copies of the receiver itself, rather than two conventions.
  layout = ''
    resolve_root() {
      local base
      base=$(on_box 'echo %LOCALAPPDATA%' | unix)
      base="''${base%"''${base##*[![:space:]]}"}"
      [ -n "$base" ] || die "could not resolve %LOCALAPPDATA% on $host"
      root="$base\\castaway"
      user=$(on_box 'echo %USERNAME%' | unix)
      user="''${user%"''${user##*[![:space:]]}"}"
      [ -n "$user" ] || die "could not resolve %USERNAME% on $host"
    }

    # Stop the whole tree, launcher first. Order matters: killing the receiver while the
    # launcher is alive gets it restarted a second later, which then holds the very files
    # about to be replaced. Ending the *task* first is what makes this deterministic.
    stop_everything() {
      on_box 'schtasks /end /TN castaway >nul 2>&1 & taskkill /F /T /IM launcher.exe 2>&1 & taskkill /F /T /IM castaway.exe 2>&1 & taskkill /F /T /IM electron.exe 2>&1' \
        | unix | grep -Ev '^(INFO: No tasks|ERROR: The process)' || true
      local left
      for _ in 1 2 3 4 5 6 7 8 9 10; do
        left=$(on_box 'tasklist /FI "IMAGENAME eq launcher.exe" /FO CSV /NH & tasklist /FI "IMAGENAME eq castaway.exe" /FO CSV /NH & tasklist /FI "IMAGENAME eq electron.exe" /FO CSV /NH' \
          | unix | grep -c '^"' || true)
        [ "$left" -eq 0 ] && break
        sleep 1
      done
      [ "$left" -eq 0 ] || die "$left launcher/castaway/electron process(es) survived taskkill on $host"
    }

    # How many differing files, and how many bytes of them, before the incremental path
    # stops being the cheap one. A Rust change is one file; a browser or ffmpeg bump is
    # thousands, and scp pays a round trip per file. Past either bound the archive wins,
    # so the fast path hands back to the slow one rather than grinding.
    INCREMENTAL_MAX_FILES=200
    INCREMENTAL_MAX_BYTES=$((120 * 1024 * 1024))

    # Which version directory on the box to seed a new one from.
    #
    # `current.txt` first, because it is the tree most likely to differ from the new one by
    # only the receiver. Any other version is still a far better starting point than an
    # empty directory, so a box mid-rollback is not pushed onto the slow path.
    seed_version() {
      local seed
      seed=$(on_box "type \"$root\\current.txt\" 2>nul" | unix | tr -d '[:space:]' || true)
      if [ -n "$seed" ] && on_box "if not exist \"$root\\versions\\$seed\\castaway.exe\" exit 1"; then
        printf '%s' "$seed"
        return 0
      fi
      seed=$(on_box "dir /b \"$root\\versions\" 2>nul" | unix | tr -d '\r' | head -1 || true)
      if [ -n "$seed" ] && on_box "if not exist \"$root\\versions\\$seed\\castaway.exe\" exit 1"; then
        printf '%s' "$seed"
        return 0
      fi
      return 1
    }

    # Stage by copying a version the box already has and sending only what differs.
    #
    # Returns non-zero when it decides the archive would be cheaper — the caller then runs
    # `stage_version` as normal, so this is an optimisation with a fallback rather than a
    # second way for a deploy to fail. Every verify-don't-assume rule the archive path has
    # is kept: each transferred file is hashed *on the box* after it lands, and the stamp
    # is written last, so a half-finished incremental deploy leaves no stamp and the next
    # run cannot take any fast path off the back of it.
    stage_version_incremental() {
      local zip_sha="$1" exe_sha="$2" id="$3" tree="$4" scratch="$5"
      local dest="$root\\versions\\$id"

      local seed
      seed=$(seed_version) || { say "no version on the box to seed from; sending the archive"; return 1; }
      [ "$seed" != "$id" ] || { say "the box already has $id"; return 1; }

      say "seeding versions\\$id from versions\\$seed (on the box, no network)"
      on_box "if exist \"$dest\" rmdir /s /q \"$dest\"" || true
      on_box "if exist \"$dest\" exit 1" \
        || die "$dest still exists after rmdir — something holds a handle on it"
      # robocopy's exit codes are a bitmask where anything under 8 means it copied what it
      # was asked to; only >=8 is a failure. `exit /b 0` is what stops cmd reporting a
      # successful copy as an error.
      on_box "robocopy \"$root\\versions\\$seed\" \"$dest\" /E /NFL /NDL /NJH /NJS /NP /R:1 /W:1 & if errorlevel 8 (exit /b 1) else (exit /b 0)" \
        || die "could not seed $dest from $seed"

      say "comparing the box's copy against the build"
      scp "''${ssh_opts[@]}" -q ${manifestPs1} "$host:castaway-manifest.ps1"
      local remote="$scratch/remote.manifest" local_manifest="$scratch/local.manifest"
      local raw="$scratch/remote.raw"
      on_box "powershell -NoProfile -ExecutionPolicy Bypass -File castaway-manifest.ps1 -Dir \"$dest\"" \
        | unix | sed 's/[[:space:]]*$//' | grep -v '^$' > "$raw" || true
      # Trust the listing only if it says it finished and the arithmetic agrees. Anything
      # else falls back to the archive: a short manifest means "send everything" to the
      # comparison below, which is the expensive wrong answer arrived at confidently.
      local trailer hashed total lines
      trailer=$(grep -m1 '^MANIFEST-COMPLETE ' "$raw" || true)
      if [ -z "$trailer" ]; then
        say "the box's manifest did not complete; sending the archive instead"
        on_box "if exist \"$dest\" rmdir /s /q \"$dest\"" || true
        return 1
      fi
      hashed=$(printf '%s' "$trailer" | awk '{print $2}')
      total=$(printf '%s' "$trailer" | awk '{print $3}')
      grep -v '^MANIFEST-COMPLETE ' "$raw" | LC_ALL=C sort > "$remote"
      lines=$(grep -c . "$remote" || true)
      if [ "$hashed" != "$total" ] || [ "$lines" != "$hashed" ]; then
        say "the box listed $lines of $total file(s); sending the archive instead"
        on_box "if exist \"$dest\" rmdir /s /q \"$dest\"" || true
        return 1
      fi
      ( cd "$tree" && find . -type f -printf '%P\n' | LC_ALL=C sort | while IFS= read -r f; do
          printf '%s %s\n' "$(sha256sum "$f" | cut -d' ' -f1)" "$f"
        done ) | sort -k2 > "$local_manifest"

      # A line is "<sha256> <relative path>", so a whole-line difference is exactly
      # "this file is absent or has different content" — one comparison covering both,
      # with no per-file lookups to get wrong.
      local send="$scratch/send.list"
      LC_ALL=C comm -23 "$local_manifest" "$remote" | cut -d' ' -f2- > "$send"

      local count bytes
      count=$(grep -c . "$send" || true)
      bytes=0
      if [ "$count" -gt 0 ]; then
        bytes=$(cd "$tree" && while IFS= read -r f; do
          [ -n "$f" ] && stat -c%s "$f" 2>/dev/null || true
        done < "$send" | awk '{ total += $1 } END { print total + 0 }')
      fi
      say "$count file(s) differ, $((bytes / 1024 / 1024)) MiB"
      if [ "$count" -gt "$INCREMENTAL_MAX_FILES" ] || [ "$bytes" -gt "$INCREMENTAL_MAX_BYTES" ]; then
        say "that is more than the archive costs; sending the archive instead"
        on_box "if exist \"$dest\" rmdir /s /q \"$dest\"" || true
        return 1
      fi

      # Anything the box has and the build does not. A version directory is the build's
      # tree exactly, never a superset — a stale DLL left behind is the kind of thing that
      # loads in preference to the right one and is invisible until it is not.
      local stale
      stale=$(LC_ALL=C comm -13 \
        <(cut -d' ' -f2- "$local_manifest") <(cut -d' ' -f2- "$remote") || true)
      if [ -n "$stale" ]; then
        say "removing $(printf '%s\n' "$stale" | grep -c . ) file(s) the build does not have"
        printf '%s\n' "$stale" | while IFS= read -r f; do
          [ -n "$f" ] || continue
          on_box "del /q /f \"$dest\\''${f//\//\\}\" >nul 2>&1" || true
        done
      fi

      local dest_fwd="''${dest//\\//}"
      while IFS= read -r f; do
        [ -n "$f" ] || continue
        say "  sending $f"
        local dir="''${f%/*}"
        [ "$dir" = "$f" ] || on_box "if not exist \"$dest\\''${dir//\//\\}\" mkdir \"$dest\\''${dir//\//\\}\"" || true
        scp "''${ssh_opts[@]}" -q "$tree/$f" "$host:$dest_fwd/$f" \
          || die "could not send $f"
        local got
        got=$(remote_sha256 "$dest\\''${f//\//\\}")
        local want
        want=$(cd "$tree" && sha256sum "$f" | cut -d' ' -f1)
        [ "$got" = "$want" ] || die "$f hashes $got on the box, $want here"
      done < "$send"

      on_box "if not exist \"$dest\\castaway.exe\" exit 1" \
        || die "$dest\\castaway.exe missing after the incremental stage"
      local got
      got=$(remote_sha256 "$dest\\castaway.exe")
      [ "$got" = "$exe_sha" ] \
        || die "deployed castaway.exe hashes $got, built one hashes $exe_sha"

      # Same stamp the archive path writes, and written just as last: the two paths are
      # interchangeable from here on, and the next run's fast path cannot tell — or need
      # to tell — which one put the tree there.
      on_box "(echo $zip_sha)> \"$dest\\.deployed-sha256\"" \
        || die "could not write the deploy stamp"
      say "staged incrementally: $dest"
    }

    # Copy the archive over, check it arrived intact, and extract it into `versions\<id>\`
    # with the wrapper directory stripped — which is what makes `versions\<id>\castaway.exe`
    # true, and is exactly what the in-app updater does with the same archive (#345).
    stage_version() {
      local zip="$1" zip_sha="$2" exe_sha="$3" id="$4"
      local dest="$root\\versions\\$id"

      say "copying $(basename "$zip") to $host"
      scp "''${ssh_opts[@]}" "$zip" "$host:castaway-deploy.zip"
      local got
      got=$(remote_sha256 'castaway-deploy.zip')
      [ "$got" = "$zip_sha" ] \
        || die "archive hash mismatch after transfer: box says '$got', built '$zip_sha'"

      say "extracting into $dest"
      # A version directory is replaced wholesale, never merged: a tree half of which is
      # from another build is the one state neither the launcher nor a human can diagnose.
      on_box "if exist \"$dest\" rmdir /s /q \"$dest\"" || true
      on_box "if exist \"$dest\" exit 1" \
        || die "$dest still exists after rmdir — something holds a handle on it"
      # bsdtar ships in Windows since 1809 and reads zip; there is no unzip on the box.
      # `--strip-components=1` drops the archive's one wrapping directory.
      on_box "mkdir \"$dest\" && tar -xf castaway-deploy.zip --strip-components=1 -C \"$dest\"" \
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
      on_box "(echo $zip_sha)> \"$dest\\.deployed-sha256\"" \
        || die "could not write the deploy stamp"
    }

    # Move `current.txt`, keeping whatever it named as `previous.txt` — the same order the
    # in-app updater uses, and for the same reason: losing power between the two writes
    # costs one unavailable rollback rather than a pointer pair that names nothing.
    point_at() {
      local id="$1" was
      was=$(on_box "type \"$root\\current.txt\" 2>nul" | unix | tr -d '[:space:]' || true)
      if [ -n "$was" ] && [ "$was" != "$id" ]; then
        on_box "(echo $was)> \"$root\\previous.txt\"" || die "could not write previous.txt"
      fi
      on_box "(echo $id)> \"$root\\current.txt\"" || die "could not write current.txt"
    }
  '';

  # A hand deploy, onto the versioned layout (#346).
  #
  # What changed with the layout: a deploy no longer wipes the install root, because the
  # root is now where `previous.txt` and the other versions live and losing those is losing
  # the rollback. It replaces one version directory, moves the pointers, and restarts the
  # launcher — which is exactly what the in-app updater does, minus the part where it waits
  # for the panel to be idle.
  #
  # It also writes `hold`. A hand deploy means a human is driving, and the 4 a.m. updater
  # must not replace what they are looking at; deleting that file is the re-arm, and there
  # is no flag for it here on purpose — re-arming should be a thing somebody does knowingly.
  deploy = pkgs.writeShellApplication {
    name = "castaway-deploy-windows";
    runtimeInputs = [ pkgs.openssh pkgs.coreutils pkgs.unzip pkgs.findutils pkgs.gnugrep pkgs.gawk ];
    text = preamble + layout + ''
      artifact="castaway-windows-electron"
      force=""
      no_launch=""
      full=""
      for arg in "$@"; do
        case "$arg" in
          --force) force=1 ;;
          --no-launch) no_launch=1 ;;
          # Skip the incremental path and send the archive. For when the box's tree is
          # suspect and re-extracting from a verified zip is the point.
          --full) full=1 ;;
          --*) echo "usage: castaway-deploy-windows [--force] [--full] [--no-launch] [ARTIFACT]" >&2
               exit 1 ;;
          *) artifact="$arg" ;;
        esac
      done

      say() { printf '\n==> %s\n' "$*"; }
      die() { printf '\nerror: %s\n' "$*" >&2; exit 1; }

      say "building .#$artifact.archive"
      # Only the archive is built, and the reference hash for castaway.exe is read back out
      # of it. Building `.#$artifact` as well to get a local exe to compare against looks
      # free and is not: it is a second derivation, so it links the Windows binary a second
      # time and the two outputs are not bit-identical — which surfaced as a "deployed
      # castaway.exe hashes X, built one hashes Y" failure on a deploy that was fine.
      # Hashing the member of the zip is also the stronger check: it verifies the bits being
      # shipped rather than a parallel build of them.
      store=$(nix build --no-link --print-out-paths ".#$artifact.archive")
      zip="$store/$artifact.zip"
      [ -f "$zip" ] || die "no archive at $zip"
      zip_sha=$(sha256sum "$zip" | cut -d' ' -f1)
      exe_sha=$(unzip -p "$zip" "$artifact/castaway.exe" | sha256sum | cut -d' ' -f1)
      [ "''${#exe_sha}" -eq 64 ] || die "no $artifact/castaway.exe inside $zip"
      printf '    %s (%s bytes)\n' "$zip" "$(stat -c%s "$zip")"

      # The version id has to be forty lowercase hex characters, because that is what the
      # launcher and the updater both parse a directory name as — and `dirtyShortRev` is
      # neither. The archive's own digest, truncated, is the honest answer: it names *these
      # bits*, it never collides with the release sha of the commit they were built from,
      # and two deploys of the same tree land in the same directory.
      version="''${zip_sha:0:40}"

      resolve_root
      dest="$root\\versions\\$version"

      # Is the box already carrying exactly these bits? Two independent answers have to
      # agree: the stamp `stage_version` wrote after a verified extract, and a fresh hash of
      # the exe itself. The stamp alone would survive someone editing the tree; the exe
      # hash alone says nothing about the 235 MB of browser beside it.
      fresh=""
      if [ -z "$force" ]; then
        have_stamp=$(on_box "type \"$dest\\.deployed-sha256\" 2>nul" | unix | tr -d '[:space:]' || true)
        have_exe=$(remote_sha256 "$dest\\castaway.exe")
        if [ "$have_stamp" = "$zip_sha" ] && [ "$have_exe" = "$exe_sha" ]; then
          fresh=1
          say "already deployed (matching stamp and castaway.exe) — skipping transfer"
          echo "    pass --force to re-copy anyway"
        fi
      fi

      say "stopping anything running on $host"
      stop_everything

      if [ -z "$fresh" ]; then
        on_box "mkdir \"$root\\versions\" 2>nul" || true
        staged=""
        if [ -z "$full" ]; then
          # Unzipped locally rather than built as `.#$artifact`, and for the reason the
          # comment above gives: a second derivation relinks the binary and its output is
          # not bit-identical, so the tree to compare against has to be *these* bytes.
          # Unpacking 235 MB onto local disk costs seconds and keeps the exe hash honest.
          scratch=$(mktemp -d) || die "mktemp -d"
          # shellcheck disable=SC2064
          trap "rm -rf '$scratch'" EXIT
          unzip -q "$zip" -d "$scratch/tree" || die "could not unpack $zip"
          if stage_version_incremental "$zip_sha" "$exe_sha" "$version" \
               "$scratch/tree/$artifact" "$scratch"; then
            staged=1
            say "deployed and verified (incremental): $dest"
          fi
          rm -rf "$scratch"
          trap - EXIT
        fi
        if [ -z "$staged" ]; then
          stage_version "$zip" "$zip_sha" "$exe_sha" "$version"
          say "deployed and verified: $dest"
        fi
      fi

      # The launcher at the root is what the scheduled task points at, and it is *not*
      # replaced by an ordinary deploy — it is installed once by `windows-migrate`. Copying
      # it here anyway would mean a hand deploy could break the one thing that survives
      # everything else. If it is missing, the box has not been migrated and says so.
      on_box "if not exist \"$root\\launcher.exe\" exit 1" \
        || die "no launcher at $root\\launcher.exe — run 'nix run .#windows-migrate' first"

      say "pointing current.txt at $version"
      point_at "$version"
      # A human is driving. The updater stands down until somebody deletes this.
      on_box "(echo hand deploy)> \"$root\\hold\"" || die "could not write the hold file"

      # sftp mangles backslashes; it takes the same path with forward slashes, and so does
      # every Windows API the other side of it.
      root_fwd="''${root//\\//}"
      scp "''${ssh_opts[@]}" -q ${tailPs1} "$host:$root_fwd/tail.ps1"

      if [ -n "$no_launch" ]; then
        say "not launching (--no-launch); current.txt points at $version"
        exit 0
      fi

      say "starting the launcher on the console session"
      # The task already exists and already points at the launcher (`windows-migrate`), so
      # this only has to run it. `/IT` and the rest of that story live there.
      on_box 'schtasks /run /TN castaway' | unix
      for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
        up=$(on_box 'tasklist /FI "IMAGENAME eq castaway.exe" /FO CSV /NH' | unix | grep -c '^"' || true)
        [ "$up" -gt 0 ] && break
        sleep 1
      done
      [ "$up" -gt 0 ] || die "castaway.exe never appeared; check $root\\castaway.log"

      say "streaming $root\\castaway.log  (Ctrl-C detaches; the panel keeps running)"
      echo
      # -t so Ctrl-C reaches the remote tail instead of orphaning a PowerShell on the box.
      ssh -t "''${ssh_opts[@]}" "$host" \
        "powershell -NoProfile -ExecutionPolicy Bypass -File \"$root\\tail.ps1\""
    '';
  };

  # The one-time migration onto the layout (#346). Idempotent, so re-running it after a
  # launcher change or a certificate rotation is the supported way to do those.
  #
  # Three things, and only the third needs elevation:
  #
  #   1. build the install root and seed it from the current artifact;
  #   2. replace the scheduled task with an **ONLOGON** one pointing at `launcher.exe` —
  #      which also fixes an adjacent gap nobody had filed: today's `/SC ONCE /ST 23:59` is
  #      run by hand, so a Windows reboot left the panel dead until somebody re-ran it;
  #   3. trust the Authenticode certificate (#344) once.
  #
  # **Firewall: nothing to do, and that is a decision rather than an omission.** The rules
  # `nix run .#windows-firewall` writes are port-scoped from `nix/network-surface.json`, so
  # they survive version-directory flips untouched. Auto-update stays low-privilege and
  # never touches firewall rules or persistence; a port-config change still wants the
  # elevated one-shot, exactly as before.
  migrate = pkgs.writeShellApplication {
    name = "castaway-windows-migrate";
    runtimeInputs = [ pkgs.openssh pkgs.coreutils pkgs.unzip ];
    text = preamble + layout + ''
      artifact="castaway-windows-electron"

      say() { printf '\n==> %s\n' "$*"; }
      die() { printf '\nerror: %s\n' "$*" >&2; exit 1; }

      say "building .#$artifact.archive"
      store=$(nix build --no-link --print-out-paths ".#$artifact.archive")
      zip="$store/$artifact.zip"
      [ -f "$zip" ] || die "no archive at $zip"
      zip_sha=$(sha256sum "$zip" | cut -d' ' -f1)
      exe_sha=$(unzip -p "$zip" "$artifact/castaway.exe" | sha256sum | cut -d' ' -f1)
      [ "''${#exe_sha}" -eq 64 ] || die "no $artifact/castaway.exe inside $zip"
      version="''${zip_sha:0:40}"

      resolve_root
      say "install root: $root  (user $user)"

      say "stopping anything running on $host"
      stop_everything

      on_box "mkdir \"$root\" 2>nul & mkdir \"$root\\versions\" 2>nul" || true
      stage_version "$zip" "$zip_sha" "$exe_sha" "$version"

      # The launcher is installed at the *root*, from inside the version tree that carries
      # it — so the one binary the scheduled task names came out of the same archive as
      # everything else, rather than from a second build nobody can point at.
      say "installing the launcher at the root"
      on_box "copy /y \"$root\\versions\\$version\\launcher.exe\" \"$root\\launcher.exe\" >nul" \
        || die "no launcher.exe in the artifact — is this build older than #342?"

      point_at "$version"
      # Seeded, so the launcher has somewhere to roll back to from its first boot: itself.
      # A `previous.txt` naming the current version costs one unavailable rollback and is
      # strictly better than one naming nothing, which the launcher would read as "no
      # target" for as long as it takes the first update to land.
      on_box "if not exist \"$root\\previous.txt\" (echo $version)> \"$root\\previous.txt\"" \
        || die "could not seed previous.txt"
      # This is a *managed* install from here on, so the updater is armed.
      on_box "if exist \"$root\\hold\" del /q \"$root\\hold\"" || true

      root_fwd="''${root//\\//}"
      scp "''${ssh_opts[@]}" -q ${tailPs1} "$host:$root_fwd/tail.ps1"

      say "replacing the scheduled task with an ONLOGON launcher task"
      # ONLOGON rather than ONCE: the panel has to come back after a reboot by itself, and
      # nothing else on the box will start it. `/IT` is the load-bearing part and is
      # unchanged — it runs with the interactive token of the logged-on user, which is the
      # only way to reach the desktop the panel is showing. A task without it lands in
      # session 0 and renders to nothing (docs/cross-build.md).
      on_box "schtasks /create /TN castaway /TR \"$root\\launcher.exe\" /SC ONLOGON /RU $user /IT /F" \
        | unix | grep -v '^WARNING: Task may not run' || true

      # The one elevated step, and the only one. Skipped rather than failed where the
      # certificate does not exist yet (#348): an unsigned artifact still runs, it just has
      # no publisher, and refusing to migrate over that would be refusing over cosmetics.
      if grep -q 'BEGIN CERTIFICATE' ${./windows-codesign.crt}; then
        if on_box 'net session >nul 2>&1'; then
          say "trusting the castaway code-signing certificate"
          scp "''${ssh_opts[@]}" -q ${./windows-codesign.crt} "$host:castaway-codesign.crt"
          on_box 'certutil -addstore -f Root castaway-codesign.crt' | unix | tail -2
          on_box 'del /q castaway-codesign.crt' || true
        else
          echo "warning: the SSH session is not elevated, so the code-signing certificate" >&2
          echo "was not imported. Re-run this with an elevated session (#346 step 3)." >&2
        fi
      else
        echo "note: nix/windows-codesign.crt carries no certificate yet (#348), so there" >&2
        echo "is nothing to trust. Re-run this after the keygen." >&2
      fi

      say "starting the launcher on the console session"
      on_box 'schtasks /run /TN castaway' | unix
      for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
        up=$(on_box 'tasklist /FI "IMAGENAME eq castaway.exe" /FO CSV /NH' | unix | grep -c '^"' || true)
        [ "$up" -gt 0 ] && break
        sleep 1
      done
      [ "$up" -gt 0 ] || die "castaway.exe never appeared; check $root\\castaway.log"

      say "migrated. The box now runs $root\\launcher.exe at logon."
      echo "    versions\\$version is current, and the updater is armed."
      echo "    Acceptance, by hand: reboot (the panel comes back), taskkill castaway.exe"
      echo "    (the launcher relaunches it), and point current.txt at a tree that dies"
      echo "    on boot (the launcher rolls back within three attempts)."
    '';
  };

in
{ inherit deploy migrate firewall winusb; }
