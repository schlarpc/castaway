#!/usr/bin/env python3
"""Carve AirServer's builtin Cast credential database and its two KEK constants.

Run at build time against a pinned installer; nothing it produces is checked in.

The point of this file is that it hardcodes **no offsets and no vendor strings**.
PROVENANCE §3 records the addresses the constants sat at in 5.7.2 (`0x1549558`,
`0x1549588`) and where that build's database sat (`0xbcff24`), but those move on
every rebuild: across 5.7.0/5.7.1/5.7.2 the database alone sits at three different
offsets. So both are located structurally instead:

* **The database** is found by its own `SQLite format 3\\0` header, whose page size
  (`+16`, big-endian, `1` meaning 65536) and page count (`+28`) give its exact
  length. A candidate is accepted only after clearing every gate in `validate()` —
  the schema the reader needs, a full `PRAGMA integrity_check` over the b-tree, and
  secretbox-shaped BLOBs. The length check is the load-bearing one: a carve of the
  wrong size can still parse its first page, and only the b-tree walk catches it.

* **The constants** are found by candidate search confirmed with Poly1305. The
  search is anchored on `CompanyName` from the PE version resource — public
  metadata, chased through the resource tree rather than read from a fixed address —
  which is *not* either constant (it is `App Dynamic ehf`, 15 bytes; the
  personalisation is 16 and ends in a period) but does narrow ~5M candidate pairs to
  four. The database is then its own oracle: a wrong pair cannot forge a Poly1305
  tag, so this cannot silently emit the wrong answer. If the anchor ever stops
  matching, `find_constants` falls back to adjacent string pairs, and if that fails
  too the script exits non-zero rather than guessing.

Verified against AirServer 5.7.0, 5.7.1 and 5.7.2 — three different database
offsets, two different schemas, one identical constant pair.
"""
import argparse
import hashlib
import json
import re
import sqlite3
import struct
import sys
import tempfile
from pathlib import Path

import nacl.bindings as nb

# The tables without which the reader cannot produce a credential. `metadata` is
# deliberately absent: it arrived after the database did (5.7.0 has no such table,
# 5.7.1 does), it is the one column that is not a secretbox, and the reader already
# treats its `generated` field as optional. Gating on it rejects a valid older file.
CORE_TABLES = {"salt", "daily_cert", "daily_private", "device_cert_chain", "device_info"}
SQLITE_MAGIC = b"SQLite format 3\x00"
NONCE, TAG = 24, 16
MAX_PERSON, MAX_KEY = 16, 64  # BLAKE2b personalisation and key limits


def log(msg):
    print(f"[airserver-carve] {msg}", file=sys.stderr)


# ------------------------------------------------------------------ PE walking


def _sections(d):
    pe = struct.unpack("<I", d[0x3C:0x40])[0]
    if d[pe : pe + 4] != b"PE\0\0":
        raise SystemExit("not a PE image")
    nsec = struct.unpack("<H", d[pe + 6 : pe + 8])[0]
    optsz = struct.unpack("<H", d[pe + 20 : pe + 22])[0]
    opt = pe + 24
    out, s0 = [], opt + optsz
    for i in range(nsec):
        o = s0 + i * 40
        vsz, va, rsz, ptr = struct.unpack("<IIII", d[o + 8 : o + 24])
        out.append((va, vsz, ptr, rsz))
    return out


def company_name(d):
    """`CompanyName` from the version resource: a search anchor, not a constant."""
    key = "CompanyName".encode("utf-16-le")
    for m in re.finditer(re.escape(key), d):
        i = m.end()
        while i + 1 < len(d) and d[i : i + 2] == b"\x00\x00":  # terminator + padding
            i += 2
        end = i
        while end + 1 < len(d) and d[end : end + 2] != b"\x00\x00":
            end += 2
        try:
            s = d[i:end].decode("utf-16-le").strip()
        except UnicodeDecodeError:
            continue
        if s:
            return s.encode()
    return None


# ----------------------------------------------------------------- the database


def validate(blob):
    """Every structural gate a carve must clear. Returns ((tables, rows), None) or (None, why)."""
    with tempfile.NamedTemporaryFile(suffix=".sqlite", delete=False) as f:
        f.write(blob)
        p = f.name
    c = None
    try:
        c = sqlite3.connect(f"file:{p}?mode=ro", uri=True)
        names = {r[0] for r in c.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        if not CORE_TABLES <= names:
            return None, f"missing tables {sorted(CORE_TABLES - names)}"
        if c.execute("PRAGMA integrity_check").fetchone()[0] != "ok":
            return None, "integrity_check failed"

        def blobs(t):
            return [r[1] for r in c.execute(f"pragma table_info({t})") if r[2].upper() == "BLOB"]

        if not blobs("salt") or not blobs("daily_cert"):
            return None, "no BLOB columns where the reader expects them"
        if c.execute("SELECT count(*) FROM salt").fetchone()[0] != 1:
            return None, "salt is not exactly one row"
        rows = c.execute("SELECT count(*) FROM daily_cert").fetchone()[0]
        if rows == 0:
            return None, "daily_cert is empty"
        col = blobs("daily_cert")[0]
        smallest = c.execute(f"SELECT min(length({col})) FROM daily_cert").fetchone()[0]
        if smallest is None or smallest < NONCE + TAG:
            return None, f"daily_cert blobs are too short to be secretboxes ({smallest})"
        return (sorted(names), rows), None
    except sqlite3.Error as e:
        return None, f"sqlite: {e}"
    finally:
        if c is not None:
            c.close()
        Path(p).unlink(missing_ok=True)


def find_db(d):
    out = []
    for m in re.finditer(re.escape(SQLITE_MAGIC), d):
        off = m.start()
        if off + 32 > len(d):
            continue
        ps = struct.unpack(">H", d[off + 16 : off + 18])[0]
        ps = 65536 if ps == 1 else ps
        if ps < 512 or (ps & (ps - 1)) != 0:
            log(f"  reject 0x{off:x}: page size {ps} is not a power of two in 512..65536")
            continue
        pc = struct.unpack(">I", d[off + 28 : off + 32])[0]
        if pc == 0 or off + ps * pc > len(d):
            log(f"  reject 0x{off:x}: {pc} pages of {ps} does not fit the image")
            continue
        blob = d[off : off + ps * pc]
        ok, why = validate(blob)
        if ok is None:
            log(f"  reject 0x{off:x}: {why}")
            continue
        out.append((off, blob, *ok))
    return out


def db_oracle(blob):
    """The salt and the smallest secretbox in the file — the cheapest trial available."""
    with tempfile.NamedTemporaryFile(suffix=".sqlite", delete=False) as f:
        f.write(blob)
        p = f.name
    try:
        c = sqlite3.connect(f"file:{p}?mode=ro", uri=True)

        def blobs(t):
            return [r[1] for r in c.execute(f"pragma table_info({t})") if r[2].upper() == "BLOB"]

        salt = c.execute(f"SELECT {blobs('salt')[0]} FROM salt LIMIT 1").fetchone()[0]
        col = blobs("daily_cert")[0]
        row = c.execute(f"SELECT {col} FROM daily_cert ORDER BY length({col}) ASC LIMIT 1").fetchone()
        c.close()
        return bytes(salt), bytes(row[0])
    finally:
        Path(p).unlink(missing_ok=True)


# ---------------------------------------------------------------- the constants


def _strings(d, minlen=1):
    for m in re.finditer(rb"[\x20-\x7e]{%d,}" % minlen, d):
        yield m.start(), m.group()


def derive(salt, person, passwd):
    return hashlib.blake2b(
        b"",
        digest_size=32,
        key=passwd,
        salt=salt[:16].ljust(16, b"\0"),
        person=person[:16].ljust(16, b"\0"),
    ).digest()


def verifies(key, blob):
    try:
        nb.crypto_secretbox_open(blob[NONCE:], blob[:NONCE], key)
        return True
    except Exception:
        return False


def find_constants(d, salt, sample):
    items = list(_strings(d, 1))

    anchor = company_name(d)
    log(f"  version-resource CompanyName: {anchor!r}")
    if anchor:
        persons = [s for _, s in items if len(s) <= MAX_PERSON and anchor in s]
        passes = [s for _, s in items if len(s) <= MAX_KEY and anchor in s]
        log(f"  anchored search: {len(persons)} x {len(passes)} candidates")
        n = 0
        for ps in persons:
            for pw in passes:
                n += 1
                if verifies(derive(salt, ps, pw), sample):
                    return ps, pw, f"anchored, {n} trials"

    # The two constants sit next to each other in the data section, so walking
    # consecutive strings is linear rather than quadratic.
    log("  anchor exhausted; trying adjacent string pairs")
    n = 0
    for i in range(len(items) - 1):
        a, b = items[i][1], items[i + 1][1]
        for ps, pw in ((a, b), (b, a)):
            if len(ps) > MAX_PERSON or len(pw) > MAX_KEY:
                continue
            n += 1
            if verifies(derive(salt, ps, pw), sample):
                return ps, pw, f"adjacent, {n} trials"
    return None, None, f"exhausted ({n} adjacent trials)"


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("exe", type=Path, help="AirServer.exe from the installer payload")
    ap.add_argument("-o", "--out", type=Path, required=True, help="output directory")
    args = ap.parse_args()

    d = args.exe.read_bytes()
    log(f"{args.exe} ({len(d)} bytes, sha256 {hashlib.sha256(d).hexdigest()})")

    dbs = find_db(d)
    if not dbs:
        raise SystemExit("no credential database in this image — is it an AirServer build?")
    if len(dbs) > 1:
        log(f"  note: {len(dbs)} databases cleared every gate; taking the first")
    off, blob, tables, rows = dbs[0]
    log(f"  database @0x{off:x}: {len(blob)} bytes, {len(tables)} tables, {rows} windows")

    salt, sample = db_oracle(blob)
    person, passwd, how = find_constants(d, salt, sample)
    if person is None:
        raise SystemExit(f"could not recover the KEK constants ({how})")
    log(f"  constants recovered ({how}): personalisation {len(person)}B, key {len(passwd)}B")

    args.out.mkdir(parents=True, exist_ok=True)
    (args.out / "castdb.sqlite").write_bytes(blob)
    (args.out / "kek_person.bin").write_bytes(person)
    (args.out / "kek_pass.bin").write_bytes(passwd)
    (args.out / "carve.json").write_text(
        json.dumps(
            {
                "source_sha256": hashlib.sha256(d).hexdigest(),
                "source_bytes": len(d),
                "db_offset": off,
                "db_bytes": len(blob),
                "db_sha256": hashlib.sha256(blob).hexdigest(),
                "db_tables": tables,
                "db_windows": rows,
                "how": how,
            },
            indent=2,
        )
        + "\n"
    )
    log(f"  wrote {args.out}")


if __name__ == "__main__":
    main()
