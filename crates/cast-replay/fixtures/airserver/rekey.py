#!/usr/bin/env python3
"""Re-key the trimmed AirServer test fixtures from App Dynamic's KEK to a test one.

The two `db_trimmed*.sqlite` files in this directory are a test corpus, not credentials
the receiver uses, and they are sealed under `airserver_db::TEST_KEK` so that
`cargo nextest run` needs nothing from App Dynamic. This is what put them there.

Every BLOB that authenticates under the real key is decrypted and re-sealed under the
test key; anything that does not authenticate (the salt row itself, the empty
`jwt_token`) is left exactly as it was, so only the KEK changes. Page size is preserved,
which matters — the 512-byte variant exists precisely to span pages.

The real constants are not in this tree: recover them with `nix build .#airserver-carve`
and pass them in. See ../../PROVENANCE.md §3.

    python3 rekey.py db_trimmed.sqlite db_trimmed_512.sqlite
"""
import hashlib
import os
import sqlite3
import sys
from pathlib import Path

import nacl.bindings as nb
import nacl.utils

# Deliberately not written down — the whole point of the exercise. Point this at a
# `nix build .#airserver-carve` result and it reads them from there, the same way
# `cast-replay/build.rs` does.
CARVE_ENV = "CASTAWAY_AIRSERVER_CARVE"

# Must match `airserver_db::TEST_KEK`.
TEST_PERSON = b"castaway-test"
TEST_PASS = b"castaway offline fixture key"
NONCE = 24


def real_kek():
    carve = os.environ.get(CARVE_ENV)
    if not carve:
        raise SystemExit(
            f"set {CARVE_ENV} to a `nix build .#airserver-carve` result; the real\n"
            "constants are not in this tree (see ../../PROVENANCE.md §3)"
        )
    d = Path(carve)
    return (d / "kek_person.bin").read_bytes(), (d / "kek_pass.bin").read_bytes()


def derive(salt, person, passwd):
    return hashlib.blake2b(
        b"", digest_size=32, key=passwd,
        salt=bytes(salt)[:16].ljust(16, b"\0"),
        person=person[:16].ljust(16, b"\0"),
    ).digest()


def main(paths):
    real_person, real_pass = real_kek()
    for p in paths:
        c = sqlite3.connect(p)
        salt = c.execute("SELECT data FROM salt LIMIT 1").fetchone()[0]
        old = derive(salt, real_person, real_pass)
        new = derive(salt, TEST_PERSON, TEST_PASS)

        tables = [r[0] for r in c.execute("SELECT name FROM sqlite_master WHERE type='table'")]
        touched = kept = 0
        for t in tables:
            cols = [(r[1], r[2]) for r in c.execute(f"pragma table_info({t})")]
            blobs = [n for n, ty in cols if ty.upper() == "BLOB"]
            pk = cols[0][0]
            if not blobs:
                continue
            for col in blobs:
                rows = c.execute(f"SELECT {pk}, {col} FROM {t}").fetchall()
                for rid, blob in rows:
                    if blob is None or len(blob) < NONCE + 16:
                        kept += 1
                        continue
                    b = bytes(blob)
                    try:
                        clear = nb.crypto_secretbox_open(b[NONCE:], b[:NONCE], old)
                    except Exception:
                        kept += 1  # not a secretbox under the real key: salt, empties
                        continue
                    nonce = nacl.utils.random(NONCE)
                    sealed = nonce + nb.crypto_secretbox(clear, nonce, new)
                    c.execute(f"UPDATE {t} SET {col}=? WHERE {pk}=?", (sealed, rid))
                    touched += 1
        c.commit()
        c.execute("VACUUM")
        c.close()
        print(f"{Path(p).name}: re-keyed {touched} blobs, left {kept} untouched")

    # The vector the KDF test asserts, recomputed under the test constants.
    salt = bytes.fromhex("a8c8de87cdfc203a9cae9f361f82e253")
    print(f"\nKDF test vector under the test KEK:\n  salt   {salt.hex()}")
    print(f"  expect {derive(salt, TEST_PERSON, TEST_PASS).hex()}")


if __name__ == "__main__":
    main(sys.argv[1:])
