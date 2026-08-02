# The AirPort Express private key, taken out of shairplay's source rather than kept
# in this tree.
#
# AirPlay 1's `Apple-Challenge` is answered by signing with the RSA key extracted from
# the original AirPort Express in 2011. Every AirPlay receiver implementation carries
# it — there is no other way to speak the protocol — and nixpkgs alone ships it in at
# least two: `shairplay` as a loose `airport.key`, and `shairport-sync` inlined into
# `common.c`.
#
# So this is a different case from the Cast identities. Those are live, revocable
# device credentials belonging to companies that are still using them; this is a
# fifteen-year-old key for discontinued hardware, published in every implementation
# that exists, and the protocol is unusable without it. Carving it buys consistency
# rather than safety: the key is not in our repository, and the one copy the build
# uses is the one nixpkgs already pins.
#
# `shairplay` rather than `shairport-sync` because it keeps the key as a standalone
# PEM file, so this is a copy rather than a C-string scrape. The key material is
# identical either way — the two differ only in PEM line wrapping, which is why the
# hash below is over the *DER*, not the file.
{
  lib,
  stdenvNoCC,
  openssl,
  shairplaySrc,
}:

stdenvNoCC.mkDerivation {
  pname = "airport-express-key";
  version = "1";

  src = shairplaySrc;
  dontUnpack = true;

  nativeBuildInputs = [ openssl ];

  buildPhase = ''
    runHook preBuild

    key=$src/airport.key
    if [ ! -f "$key" ]; then
      echo "shairplay's source has no airport.key; it has been reorganised" >&2
      exit 1
    fi

    mkdir -p "$out"

    # Normalise through DER and back, so the output does not depend on whichever line
    # wrapping the upstream project happened to use.
    #
    # `-traditional` is load-bearing: without it OpenSSL 3 writes PKCS#8
    # (`BEGIN PRIVATE KEY`), and the crate parses PKCS#1 (`BEGIN RSA PRIVATE KEY`).
    # The mismatch is silent — the key simply fails to parse and every
    # `Apple-Challenge` is answered with `KeyUnavailable`.
    openssl rsa -in "$key" -outform DER -out key.der 2>/dev/null
    openssl rsa -in key.der -inform DER -outform PEM -traditional \
      -out "$out/airport.pem" 2>/dev/null

    head -1 "$out/airport.pem" | grep -q 'BEGIN RSA PRIVATE KEY' || {
      echo "expected a PKCS#1 PEM; openssl wrote $(head -1 "$out/airport.pem")" >&2
      exit 1
    }

    # The key is the one thing this derivation is for, so prove it rather than
    # trusting the path: this modulus is the AirPort Express's, and a different key
    # would answer every Apple-Challenge with a signature senders reject.
    got=$(openssl rsa -in "$out/airport.pem" -noout -modulus | sha256sum | cut -d' ' -f1)
    want=0b7c37913729bc9828e1f562f8063ce89cc1236fe338c5e7423d070c47b3127c
    if [ "$got" != "$want" ]; then
      echo "airport.key is not the AirPort Express key (modulus sha256 $got)" >&2
      exit 1
    fi

    runHook postBuild
  '';

  installPhase = "true";

  meta = {
    description = "The AirPort Express RSA key, from shairplay's source";
    license = lib.licenses.unfree;
    platforms = lib.platforms.all;
  };
}
