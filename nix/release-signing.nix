# Signing a release, and making the keys that sign it.
#
# Two halves of #343, deliberately in one file because they are two ends of the same
# key: `release-keygen` makes the pair once, by hand, and `release-manifest` uses the
# secret half in CI on every push. The public half is checked into the tree at
# crates/update/release-key.pub and compiled into the receiver, which is what makes the
# trust anchor a property of the *binary* rather than of whatever the panel downloaded.
#
# Why any of this exists: a commit sha identifies a tree without ordering it. The panel
# already knows which commit it is; what it cannot work out on its own is whether the
# release it was just offered is newer than that, and TLS cannot tell it either — a
# replayed old release is served correctly over a correct certificate. The manifest's
# monotonic build number is the ordering, and the signature is what stops the ordering
# from being rewritten in transit.
#
# The tool is minisign rather than something of ours (ground rule 9's spirit: the
# artefact should be inspectable without our code). `minisign -Vm manifest.json -p
# crates/update/release-key.pub` verifies a release from any machine with minisign on it.
{ pkgs }:

let
  # Where the receiver reads the public half from. One string, so the keygen writes to
  # exactly the path `include_str!` reads.
  publicKeyPath = "crates/update/release-key.pub";
in
{
  # `nix run .#release-manifest -- <zip> <commit-sha> <build-number> <outdir>`
  #
  # Writes `<outdir>/manifest.json` and `<outdir>/manifest.json.minisig`. The secret key
  # arrives in `CASTAWAY_RELEASE_SECRET_KEY` as the literal contents of a minisign secret
  # key file — the shape a GitHub Actions secret holds — and is written to a private temp
  # file because minisign takes a path, never a value.
  #
  # Refuses to write an unsigned manifest. An unsigned manifest is not a weaker manifest,
  # it is a file the receiver ignores entirely, and shipping one would make "the release
  # carries a manifest" true while the property it exists for is false.
  manifest = pkgs.writeShellApplication {
    name = "castaway-release-manifest";
    runtimeInputs = [ pkgs.minisign pkgs.jq pkgs.coreutils pkgs.gnugrep ];
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
      # `git rev-list --count` on a *shallow* clone answers 1, and a build number of 1
      # would make every subsequent release look like a downgrade to a panel that had
      # already taken a real one. Catching it here is the difference between a failed
      # release job and an auto-updater that silently stops.
      if [ "$build" -lt 2 ]; then
        echo "build number is $build — is this a shallow clone? release.yml needs" >&2
        echo "fetch-depth: 0 for 'git rev-list --count HEAD' to mean anything." >&2
        exit 1
      fi

      if [ -z "''${CASTAWAY_RELEASE_SECRET_KEY:-}" ]; then
        echo "CASTAWAY_RELEASE_SECRET_KEY is unset: no release signing key." >&2
        echo "Generate one with 'nix run .#release-keygen' and put the secret half in" >&2
        echo "the repository's RELEASE_SIGNING_KEY Actions secret. Refusing to write an" >&2
        echo "unsigned manifest, which the receiver would ignore anyway." >&2
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

      key=$(mktemp)
      trap 'rm -f "$key"' EXIT
      chmod 600 "$key"
      printf '%s\n' "$CASTAWAY_RELEASE_SECRET_KEY" > "$key"

      # The trusted comment is the one string in the exchange an attacker cannot write,
      # so it says what the receiver would most want to read back in a log line.
      minisign -S -m "$outdir/manifest.json" -s "$key" \
        -c 'castaway release manifest' \
        -t "castaway build $build $commit"

      # Verify what we just produced against the public half the *receiver* carries, not
      # against the secret we just used. That is a different claim: it catches a secret
      # rotated without the tree being updated, which would otherwise ship releases no
      # panel can install and look entirely healthy from here.
      if [ -f "${publicKeyPath}" ] && grep -qv '^untrusted comment:' "${publicKeyPath}"; then
        minisign -V -m "$outdir/manifest.json" -p "${publicKeyPath}" -q \
          || { echo "the manifest does not verify against ${publicKeyPath}:" >&2
               echo "the signing secret and the key compiled into the receiver disagree." >&2
               exit 1; }
      else
        echo "warning: ${publicKeyPath} carries no key, so the signature was not checked" >&2
        echo "against what the receiver trusts. Run 'nix run .#release-keygen'." >&2
      fi

      echo "manifest: build $build, $name ($size bytes, sha256 $sha)"
    '';
  };

  # `nix run .#release-keygen` — once, by hand, on a machine the operator trusts.
  #
  # Writes the public half into the tree (commit it) and prints the secret half for the
  # Actions secret. The secret is printed rather than written anywhere: a file is a thing
  # that gets committed by accident, and this one only ever needs to be pasted once.
  keygen = pkgs.writeShellApplication {
    name = "castaway-release-keygen";
    runtimeInputs = [ pkgs.minisign pkgs.coreutils pkgs.gnugrep ];
    text = ''
      out="''${1:-${publicKeyPath}}"
      if [ -f "$out" ] && grep -qv '^untrusted comment:' "$out"; then
        echo "$out already carries a key." >&2
        echo >&2
        echo "Replacing it makes every panel running an older build refuse every future" >&2
        echo "release, because the key it was compiled with no longer signs anything." >&2
        echo "Rotation is therefore a deploy, not a keygen: generate elsewhere, commit," >&2
        echo "ship that build to the panel by hand, and only then switch the secret." >&2
        exit 1
      fi

      secret=$(mktemp)
      trap 'rm -f "$secret"' EXIT
      # `-W` for an unencrypted secret key: CI cannot type a passphrase, and a passphrase
      # stored beside the key it protects protects nothing.
      minisign -G -W -p "$out" -s "$secret" \
        -c 'castaway release signing key (#343)' >/dev/null

      echo "wrote the public half to $out — commit it."
      echo
      echo "Put this in the repository's RELEASE_SIGNING_KEY Actions secret, whole,"
      echo "both lines. It does not need to be kept anywhere else: losing it costs one"
      echo "keygen and one hand-deployed build, and having it stolen costs a panel."
      echo
      cat "$secret"
      echo
      echo "Then: gh secret set RELEASE_SIGNING_KEY < the-file-you-pasted-it-into"
    '';
  };
}
