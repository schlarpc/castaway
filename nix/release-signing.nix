# Signing a release, and making the keys that sign it.
#
# Two independent signatures, and they answer different questions:
#
#   * the **release manifest** (#343), which carries the build number that tells the panel
#     an offered release is *newer* than the one it is running — authenticated not by a
#     signature of its own but by GitHub's build provenance over it (D59);
#   * **Authenticode + castLabs VMP** on the Windows artifact (#344), which is what tells
#     *Windows* the .exe is not anonymous and tells *Widevine* that the media path is
#     verified.
#
# Neither substitutes for the other. A manifest signature says nothing to the CDM, and a
# VMP signature says nothing about which release is newer.
#
# All of it runs in CI so the release asset arrives complete and the panel holds no
# credentials. That is not a convenience: `vmp-sign.sh` used to run after a hand deploy,
# so the first unattended update would have replaced a signed tree with an unsigned one
# and killed DRM playback silently, with nobody standing at the panel.
#
# The Authenticode certificate's public half is checked into the tree
# (`nix/windows-codesign.crt`), which is what makes that trust anchor a property of the
# thing that checks it — the box's certificate store — rather than of whatever was
# downloaded alongside the artifact. The manifest needs no key of ours at all (D59).
#
# Why any of this exists: a commit sha identifies a tree without ordering it. The panel
# already knows which commit it is; what it cannot work out on its own is whether the
# release it was just offered is newer than that, and TLS cannot tell it either — a
# replayed old release is served correctly over a correct certificate. The manifest's
# monotonic build number is the ordering, and the signature is what stops the ordering
# from being rewritten in transit.
#
# Neither signature is one of ours, which is the point (ground rule 9's spirit: the artefact
# should be inspectable without our code). `gh attestation verify <artifact> --repo
# schlarpc/castaway` checks the provenance from any machine with `gh` on it, and
# `osslsigncode verify` reads the Authenticode signature.
{ pkgs }:

let
  # The Authenticode certificate, public half. Checked in for two readers: the box, which
  # imports it into its trust store once (#346), and CI, which verifies its own output
  # against it — a different claim from verifying against the key it just signed with,
  # because that one catches a secret rotated without the tree being updated.
  codesignCertPath = "nix/windows-codesign.crt";

  # Does a checked-in file actually carry a key, or is it the comment-only placeholder
  # this tree ships before anyone has run the keygen? Asked in four places, and each
  # answer is a positive match on the payload's own marker rather than "not a comment":
  # a stub that reads as a key is how a release ships looking signed.
  #
  # PEM says what it is on the tin.
  hasCert = path: ''[ -f "${path}" ] && grep -q 'BEGIN CERTIFICATE' "${path}"'';

  # `nix run .#windows-authenticode -- <exe>...` — sign each in place.
  #
  # Ours only. The staged Electron distribution is *not* signed here and must not be:
  # castLabs signs and VMP-covers those exact bytes, and re-signing electron.exe with our
  # certificate would modify a file whose VMP signature is a hash of it (D36,
  # nix/windows.nix `stageBrowser`). What Authenticode buys on our own executables is
  # narrow and worth stating: SmartScreen reputation is nil for a self-signed certificate,
  # so this is not about warnings going away. It is about the panel's one autostart
  # binary having a publisher at all — most of the not-looking-like-malware story #26
  # cares about — and about the box being able to say "this is the same publisher as last
  # time" across an unattended update.
  #
  # No RFC-3161 timestamp, deliberately. A timestamp's value is that the signature
  # outlives the certificate; the certificate here is self-signed, ten years long, and
  # trusted by exactly one machine that we also control. Buying that with a third-party
  # network dependency on every release is the wrong trade — but `CASTAWAY_CODESIGN_TSA`
  # turns it on for anyone who disagrees or who moves to a real CA (Azure Trusted Signing
  # is the upgrade path named in #344).
  authenticode = pkgs.writeShellApplication {
    name = "castaway-windows-authenticode";
    runtimeInputs = [ pkgs.osslsigncode pkgs.openssl pkgs.coreutils pkgs.gnugrep ];
    text = ''
      [ "$#" -gt 0 ] || { echo "usage: castaway-windows-authenticode <exe>..." >&2; exit 1; }

      if [ -z "''${CASTAWAY_CODESIGN_PFX:-}" ]; then
        echo "CASTAWAY_CODESIGN_PFX is unset: no Authenticode certificate." >&2
        echo "Generate one with 'nix run .#windows-codesign-keygen'." >&2
        exit 1
      fi

      work=$(mktemp -d)
      trap 'rm -rf "$work"' EXIT
      # Base64 because a PKCS#12 is binary and an Actions secret is a string. `-d` reads
      # the wrapped lines GitHub's secret editor produces as happily as one long one.
      printf '%s' "$CASTAWAY_CODESIGN_PFX" | base64 -d > "$work/cert.pfx"
      pass="''${CASTAWAY_CODESIGN_PASSWORD:-}"

      # The signing certificate, pulled back out of the PKCS#12 so the verification below
      # has something to chain to. A self-signed certificate is its own CA file.
      openssl pkcs12 -in "$work/cert.pfx" -passin "pass:$pass" -nokeys -clcerts -nodes \
        -out "$work/cert.pem" 2>/dev/null \
        || { echo "could not open the PKCS#12 — wrong CASTAWAY_CODESIGN_PASSWORD?" >&2; exit 1; }

      # And the same certificate as the box will trust (#346). Comparing fingerprints
      # rather than trusting either side: a secret rotated without the tree being updated
      # produces releases the box refuses to recognise, and that failure is invisible from
      # here unless it is asserted here.
      # Overridable for the same reason as the release key's path: relative to the
      # caller's working directory, which is the repository root in CI and a sandbox in a
      # check.
      trusted="''${CASTAWAY_CODESIGN_CERT:-${codesignCertPath}}"
      if ${hasCert "$trusted"}; then
        mine=$(openssl x509 -in "$work/cert.pem" -noout -fingerprint -sha256)
        theirs=$(openssl x509 -in "$trusted" -noout -fingerprint -sha256)
        if [ "$mine" != "$theirs" ]; then
          echo "the signing certificate is not the one in $trusted:" >&2
          echo "  signing with $mine" >&2
          echo "  box trusts   $theirs" >&2
          echo "Every panel would refuse to recognise the publisher. Refusing to sign." >&2
          exit 1
        fi
      else
        echo "warning: $trusted carries no certificate, so nothing checked" >&2
        echo "that this is the publisher the box trusts." >&2
      fi

      ts=()
      [ -n "''${CASTAWAY_CODESIGN_TSA:-}" ] && ts=(-ts "$CASTAWAY_CODESIGN_TSA")

      for exe in "$@"; do
        [ -f "$exe" ] || { echo "no such file: $exe" >&2; exit 1; }
        # osslsigncode cannot sign in place; it reads one file and writes another.
        osslsigncode sign \
          -pkcs12 "$work/cert.pfx" -pass "$pass" \
          -h sha256 \
          -n castaway \
          -i https://github.com/schlarpc/castaway \
          ''${ts[@]+"''${ts[@]}"} \
          -in "$exe" -out "$work/signed" >/dev/null
        mv "$work/signed" "$exe"

        # Assert rather than assume, because an unsigned .exe looks exactly like a signed
        # one from the outside and the difference only shows up on the box.
        osslsigncode verify -in "$exe" -CAfile "$work/cert.pem" >"$work/verify" 2>&1 \
          || { echo "signed $exe, and the signature does not verify:" >&2
               cat "$work/verify" >&2
               exit 1; }
        echo "authenticode: $(basename "$exe")"
      done
    '';
  };

in
{
  # `nix run .#release-manifest -- <zip> <commit-sha> <build-number> <outdir>`
  #
  # Writes `<outdir>/manifest.json`, and nothing else. It carries no signature of its own
  # any more: `release.yml` attests it with `actions/attest-build-provenance`, and the
  # receiver verifies *that* — a Sigstore bundle signed by a certificate bound to the
  # workflow identity, with no secret in the repository for anyone to steal (D59).
  #
  # So the manifest is authenticated by something outside itself, which is why this script
  # no longer refuses to run without a key: there is no key. What the file still does is
  # the part a signature never did — carry the monotonic build number that *orders* two
  # releases, and the digest that binds the artifact to it.
  manifest = pkgs.writeShellApplication {
    name = "castaway-release-manifest";
    runtimeInputs = [ pkgs.jq pkgs.coreutils pkgs.gnugrep ];
    text = ''
      zip="''${1:?usage: castaway-release-manifest <zip> <commit-sha> <build-number> <outdir>}"
      commit="''${2:?commit sha}"
      build="''${3:?build number}"
      outdir="''${4:?output directory}"

      [ -f "$zip" ] || { echo "no such artifact: $zip" >&2; exit 1; }
      # The full sha, not the short one the tag carries: a prefix is not an identifier.
      printf '%s' "$commit" | grep -qEx '[0-9a-f]{40}' \
        || { echo "commit must be a full lowercase 40-character sha, got '$commit'" >&2; exit 1; }
      printf '%s' "$build" | grep -qEx '[0-9]+' \
        || { echo "build must be a positive integer, got '$build'" >&2; exit 1; }
      # A floor, not a shallow-clone detector — release.yml asks git whether the checkout
      # is shallow, which is the exact question and catches every depth rather than only
      # depth 1. This is the backstop for a caller that is not release.yml.
      if [ "$build" -lt 2 ]; then
        echo "build number is $build, which no real release has. If this is CI, the" >&2
        echo "checkout is shallow and 'git rev-list --count' counted what it could see." >&2
        exit 1
      fi

      mkdir -p "$outdir"
      name=$(basename "$zip")
      sha=$(sha256sum "$zip" | cut -d' ' -f1)
      size=$(stat -c%s "$zip")

      # jq rather than printf: the artifact name ends up in JSON a signature will cover,
      # and hand-rolled quoting is how a release becomes unparseable at 4 a.m.
      jq -n \
        --argjson schema 1 \
        --arg commit "$commit" \
        --argjson build "$build" \
        --arg artifact "$name" \
        --arg sha256 "$sha" \
        --argjson size "$size" \
        '{schema: $schema, commit: $commit, build: $build, artifact: $artifact, sha256: $sha256, size: $size}' \
        > "$outdir/manifest.json"

      echo "manifest: build $build, $name ($size bytes, sha256 $sha)"
    '';
  };

  inherit authenticode;

  # `nix run .#windows-codesign-keygen` — once, by hand, like the release key.
  #
  # A self-signed certificate, decided 2026-08-11 (#344): the box trusts it once, over
  # the elevated SSH we already hold for one-time steps. Ten years, because rotating it
  # means visiting the box, and a certificate that expires unattended is a certificate
  # that expires at the worst moment.
  codesignKeygen = pkgs.writeShellApplication {
    name = "castaway-windows-codesign-keygen";
    runtimeInputs = [ pkgs.openssl pkgs.coreutils pkgs.gnugrep ];
    text = ''
      out="''${1:-${codesignCertPath}}"
      if ${hasCert "$out"}; then
        echo "$out already carries a certificate." >&2
        echo >&2
        echo "Replacing it means re-importing the new one into the box's trust store," >&2
        echo "so it is a visit to the panel rather than a keygen. Generate elsewhere if" >&2
        echo "that is what you mean to do." >&2
        exit 1
      fi

      work=$(mktemp -d)
      trap 'rm -rf "$work"' EXIT
      # A password on the PKCS#12 even though it lives beside nothing: Actions secrets are
      # two separate values, so the archive and the password to open it are stolen
      # separately. Generated rather than chosen, because a human-chosen one here would be
      # reused from somewhere.
      pass=$(openssl rand -hex 24)

      # `codeSigning` extended key usage, critical: without it Windows will not accept the
      # certificate for this purpose at all, and the failure is a signature that verifies
      # cryptographically and is refused anyway.
      openssl req -x509 -newkey rsa:3072 -sha256 -days 3650 -noenc \
        -keyout "$work/key.pem" -out "$out" \
        -subj '/CN=castaway kiosk (self-signed)/O=castaway' \
        -addext 'basicConstraints=critical,CA:false' \
        -addext 'keyUsage=critical,digitalSignature' \
        -addext 'extendedKeyUsage=critical,codeSigning' 2>/dev/null

      openssl pkcs12 -export -out "$work/cert.pfx" \
        -inkey "$work/key.pem" -in "$out" \
        -name 'castaway code signing' -passout "pass:$pass"

      echo "wrote the certificate to $out — commit it; #346 imports it on the box."
      echo
      echo "gh secret set WINDOWS_CODESIGN_PFX --body '$(base64 -w0 < "$work/cert.pfx")'"
      echo
      echo "gh secret set WINDOWS_CODESIGN_PASSWORD --body '$pass'"
      echo
      echo "The private half exists nowhere else once this shell exits. Losing it costs a"
      echo "new certificate and one trip to the panel to import it."
    '';
  };

  # `nix run .#sign-windows -- <in.zip> <out.zip>` — the whole artifact, in order.
  #
  # Authenticode first, then castLabs VMP. That ordering is castLabs' own rule and it is
  # load-bearing in the general case: VMP hashes the bytes it signs, so Authenticode after
  # VMP silently invalidates the VMP signature and the failure surfaces as a licence
  # refusal from the service rather than as an error here. It happens to be vacuous for
  # this tree today — we Authenticode only our own executables, and VMP covers only the
  # Electron distribution beside them — and it is kept in this order anyway, because the
  # day those sets overlap is not a day anyone will remember this comment.
  #
  # Outside the derivation, deliberately, and the same carve-out `vmp-sign.sh` already
  # documents: EVS is a network service signing exact bytes, so inside the sandbox this
  # would be non-reproducible and would fail closed on any machine without credentials.
  # The re-zip is the honest consequence — the release asset is no longer the store zip
  # bit for bit, and what binds its bytes instead is the signed manifest (#343).
  signWindows = pkgs.writeShellApplication {
    name = "castaway-sign-windows";
    # Not python3: `vmp-sign.sh` runs `python3 -m castlabs_evs`, and the interpreter that
    # has the client installed is the caller's (a venv, a `pip install --user`). Putting
    # one on PATH here would shadow it with an interpreter that has no EVS at all.
    runtimeInputs = [ authenticode pkgs.unzip pkgs.zip pkgs.coreutils pkgs.findutils pkgs.gnugrep ];
    text = ''
      src="''${1:?usage: castaway-sign-windows <in.zip> <out.zip>}"
      dst="''${2:?output zip}"
      [ -f "$src" ] || { echo "no such archive: $src" >&2; exit 1; }

      work=$(mktemp -d)
      trap 'rm -rf "$work"' EXIT
      unzip -q "$src" -d "$work/tree"
      # The archive holds exactly one top-level directory (nix/windows.nix `mkArchive`),
      # named for the artifact. Reading it rather than hardcoding keeps this working for
      # whatever the artifact is called.
      root=$(find "$work/tree" -mindepth 1 -maxdepth 1 -type d)
      [ -d "$root" ] || { echo "$src does not hold a single top-level directory" >&2; exit 1; }

      # Ours, and only the ones that are there: `launcher.exe` joins the tree with #342
      # and this should not need editing on the day it does.
      exes=()
      for name in castaway.exe launcher.exe; do
        [ -f "$root/$name" ] && exes+=("$root/$name")
      done
      [ "''${#exes[@]}" -gt 0 ] || { echo "no castaway executables in $src" >&2; exit 1; }
      castaway-windows-authenticode "''${exes[@]}"

      browser="$root/browser"
      if [ -d "$browser" ]; then
        # `vmp-sign.sh` travels *inside* the artifact rather than being called out of the
        # repository: it is the copy that shipped with these bytes, so a tree signed here
        # and a tree signed by hand on a deploy box go through the same script.
        "$root/vmp-sign.sh" "$browser"
        # The credential-free half, which its own comments say was built for CI. Point 3
        # of #344: assert, do not assume. A release whose .sig files are missing plays
        # against Widevine UAT and fails against production, which is the worst shape a
        # failure can have — invisible here, silent on the panel, and only visible as a
        # licence refusal to whoever is standing in front of it.
        "$root/vmp-sign.sh" --check "$browser"
      else
        echo "no browser/ in $src, so nothing to VMP-sign" >&2
      fi

      # Repacked the way `mkArchive` packs: sorted names, DOS-epoch mtimes, `-X` for no
      # extra fields. The signatures inside make this archive non-reproducible whatever we
      # do — Authenticode carries a signing time, EVS signs on its own server — so this is
      # not a claim of bit-for-bit rebuildability. It keeps the *diff* between a signed and
      # an unsigned artifact down to the signatures, which is what makes one inspectable.
      name=$(basename "$root")
      ( cd "$work/tree" \
        && find "$name" -exec touch -d '1980-01-01 00:00:00 UTC' {} + \
        && find "$name" | sort | zip -qX "$work/out.zip" -@ )
      mv "$work/out.zip" "$dst"
      echo "signed artifact: $dst ($(stat -c%s "$dst") bytes)"
    '';
  };
}
