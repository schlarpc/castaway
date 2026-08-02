#!/usr/bin/env python3
"""Carve AirReceiver's CKS backend credentials out of `libAirReceiver.so`.

Run at build time against an operator-supplied library; nothing it produces is
checked in. Companion to `airserver-carve.py`, and the same discipline: **no
hardcoded offsets and no vendor strings**.

## What has to be defeated

Unlike AirServer's, these constants are not in the binary as text in any encoding
— not ASCII, not UTF-16, not raw bytes, not single-byte XOR. AirReceiver stores
its sensitive strings obfuscated: each source byte is split into two nibbles,
each nibble stored as a byte in 0x80..0x8f, and the packed result encrypted with
a custom RC4-like stream cipher under a 20-byte key in `.rodata`. The cipher
feeds both ciphertext and produced plaintext back into its state, so it is
self-synchronising — which is why a known-plaintext XOR sweep finds nothing. The
first decrypted byte is a per-string salt and is dropped.

The constructor then reaches each string through a pointer table in
`.data.rel.ro` whose slots are filled by `R_AARCH64_RELATIVE` relocations, so the
file bytes are zero and the real target is the relocation *addend*.

## How this stays structural

* **The cipher key** is recovered, not written down: every 20-byte window in the
  file is tried until one decodes a sample encoded string to printable ASCII.
* **The strings** are found by scanning for the encoded-byte pattern, not by
  address.
* **Which 32-hex constant is which** is the real problem — there are three in
  5.1.7, and content cannot tell them apart. So the table itself disambiguates:
  find the slot whose decoded string is exactly `x-api-key` (a public HTTP header
  name, not a secret), and the API key and `sig` secret are the next two slots.
  That adjacency is a property of the constructor's layout, not an address.

Every result is shape-checked (32 lowercase hex) and the surrounding table is
sanity-checked against the endpoint URL before anything is emitted. There is no
offline oracle for these two constants — unlike the AirServer carve, whose
answer a Poly1305 tag proves — so this fails loudly on anything unexpected
rather than guessing.
"""
import argparse
import hashlib
import json
import re
import struct
import sys
from multiprocessing import Pool, cpu_count
from pathlib import Path

# --------------------------------------------------------------- the obfuscation

I, J, K, C, A = 0x100, 0x101, 0x102, 0x103, 0x104
ENCODED = re.compile(rb"(?:[\x80-\x8f]{2}){3,}\x00")
KEY_LEN = 20


def _shuffle_index(S, n, key, state):
    mask = 1
    while mask < n:
        mask = mask * 2 + 1
    v, r = state[0], 0
    for _ in range(12):
        b = S[v & 0xFF]
        kp = state[1]
        state[1] = kp + 1
        v = key[kp] + b
        state[0] = v & 0xFF
        if state[1] >= len(key):
            state[1] = 0
            v = state[0] + len(key)
            state[0] = v & 0xFF
        r = v & mask & 0xFF
        if r <= n:
            return r & 0xFF
    return r & 0xFF


def init(key):
    """Keyed Fisher-Yates shuffle of the permutation, then seed the state."""
    S = list(range(256)) + [0] * 5
    state = [0, 0]
    for n in range(255, 0, -1):
        idx = _shuffle_index(S, n, key, state)
        S[n], S[idx] = S[idx], S[n]
    S[I], S[J], S[K], S[C], S[A] = S[1], S[3], S[5], S[7], S[state[0]]
    return S


def _step(S):
    i = S[I]
    t = S[S[A]]
    S[I] = (i + 1) & 0xFF
    S[J] = (S[i] + S[J]) & 0xFF
    S[S[A]] = S[S[J]]
    S[S[J]] = S[S[C]]
    S[S[C]] = S[S[I]]
    S[S[I]] = t
    S[K] = (S[t] + S[K]) & 0xFF


def decrypt_byte(S, c):
    _step(S)
    b1 = S[(S[S[I]] + S[S[J]]) & 0xFF]
    b2 = S[S[(S[S[K]] + S[S[C]] + S[S[A]]) & 0xFF]]
    S[A] = c
    out = b1 ^ c ^ b2
    S[C] = out
    return out


def decode(raw, key):
    if len(raw) <= 2 or len(raw) & 1:
        return None
    packed = bytes(
        ((raw[2 * i] & 0x0F) << 4) | (raw[2 * i + 1] & 0x0F) for i in range(len(raw) // 2)
    )
    S = init(key)
    return bytes(decrypt_byte(S, c) for c in packed)[1:]


def _try_windows(args):
    data, sample, lo, hi = args
    for o in range(lo, hi):
        S = init(data[o : o + KEY_LEN])
        good, n_ok = True, 0
        for n, c in enumerate(sample):
            p = decrypt_byte(S, c)
            if n:
                if not 32 <= p < 127:
                    good = False
                    break
                n_ok += 1
        if good and n_ok >= 8:
            return o
    return None


def recover_key(data):
    """Brute-force the cipher key: the 20-byte window that decodes a sample string."""
    cands = [data[m.start() : m.end() - 1] for m in ENCODED.finditer(data)]
    if not cands:
        return None
    raw = sorted(cands, key=len)[len(cands) // 2][:60]
    sample = bytes(
        ((raw[2 * i] & 0x0F) << 4) | (raw[2 * i + 1] & 0x0F) for i in range(len(raw) // 2)
    )
    workers = max(1, cpu_count() - 2)
    step = len(data) // workers + 1
    jobs = [
        (data, sample, i, min(i + step, len(data) - KEY_LEN))
        for i in range(0, len(data), step)
    ]
    with Pool(workers) as p:
        for r in p.imap_unordered(_try_windows, jobs):
            if r is not None:
                p.terminate()
                return data[r : r + KEY_LEN]
    return None


# --------------------------------------------------------------------- the ELF


class Elf:
    def __init__(self, data):
        self.data = data
        if data[:4] != b"\x7fELF":
            raise SystemExit("not an ELF image")
        e_phoff, e_shoff = struct.unpack_from("<QQ", data, 0x20)
        e_phentsize, e_phnum = struct.unpack_from("<HH", data, 0x36)
        e_shentsize, e_shnum, e_shstrndx = struct.unpack_from("<HHH", data, 0x3A)

        self.segments = []
        for i in range(e_phnum):
            off = e_phoff + i * e_phentsize
            p_type = struct.unpack_from("<I", data, off)[0]
            p_offset, p_vaddr, _, p_filesz, _, _ = struct.unpack_from("<QQQQQQ", data, off + 8)
            if p_type == 1:  # PT_LOAD
                self.segments.append((p_vaddr, p_offset, p_filesz))

        self.sections = {}
        shstr_off = struct.unpack_from("<Q", data, e_shoff + e_shstrndx * e_shentsize + 0x18)[0]
        for i in range(e_shnum):
            off = e_shoff + i * e_shentsize
            sh_name = struct.unpack_from("<I", data, off)[0]
            sh_addr, sh_offset, sh_size = struct.unpack_from("<QQQ", data, off + 0x10)
            end = data.index(b"\0", shstr_off + sh_name)
            self.sections[data[shstr_off + sh_name : end].decode()] = (sh_addr, sh_offset, sh_size)

        # R_AARCH64_RELATIVE: the slot's file bytes are zero; the target is the addend.
        self.relative = {}
        for name in (".rela.dyn", ".rela.plt"):
            if name not in self.sections:
                continue
            _, off, size = self.sections[name]
            for p in range(off, off + size, 24):
                r_offset, r_info, r_addend = struct.unpack_from("<QQq", data, p)
                if (r_info & 0xFFFFFFFF) == 1027:
                    self.relative[r_offset] = r_addend

    def v2f(self, vaddr):
        for p_vaddr, p_offset, p_filesz in self.segments:
            if p_vaddr <= vaddr < p_vaddr + p_filesz:
                return p_offset + (vaddr - p_vaddr)
        return None

    def ptr(self, vaddr):
        if vaddr in self.relative:
            return self.relative[vaddr]
        fo = self.v2f(vaddr)
        if fo is None or fo + 8 > len(self.data):
            return None
        return struct.unpack_from("<Q", self.data, fo)[0] or None

    def decode_at(self, vaddr, key):
        fo = self.v2f(vaddr)
        if fo is None:
            return None
        try:
            end = self.data.index(b"\0", fo)
        except ValueError:
            return None
        raw = self.data[fo:end]
        if not raw or len(raw) & 1:
            return None
        try:
            return decode(raw, key)
        except Exception:
            return None


# ------------------------------------------------------------------- the carve

HEX32 = re.compile(rb"^[0-9a-f]{32}$")
ANCHOR = b"x-api-key"
SLOT = 8


def log(m):
    print(f"[airreceiver-carve] {m}", file=sys.stderr)


def carve(data):
    elf = Elf(data)
    key = recover_key(data)
    if key is None:
        raise SystemExit("could not recover the string-obfuscation key")
    log(f"  string key recovered: {key.hex()}")

    # Every relocation slot that resolves, through one or two hops, to a string
    # that decodes. One hop is the table itself; two is the table-of-pointers the
    # 5.1.7 constructor uses.
    resolved = {}
    for slot in elf.relative:
        p1 = elf.ptr(slot)
        if p1 is None:
            continue
        for target in (p1, elf.ptr(p1)):
            if target is None:
                continue
            s = elf.decode_at(target, key)
            if s and s.isascii() and all(9 <= b < 127 for b in s) and len(s) > 1:
                resolved[slot] = s
                break

    log(f"  {len(resolved)} relocation slots resolve to decodable strings")

    anchors = [s for s, v in resolved.items() if v == ANCHOR]
    if not anchors:
        raise SystemExit(f"no table slot decodes to {ANCHOR!r}; the layout has changed")

    for anchor in anchors:
        api = resolved.get(anchor + SLOT)
        sig = resolved.get(anchor + 2 * SLOT)
        if api is None or sig is None:
            continue
        if not (HEX32.match(api) and HEX32.match(sig)):
            continue
        # Sanity: the same table must carry the endpoint this credential is for.
        urls = [v for v in resolved.values() if b"cast.remotetogo.com" in v]
        if not urls:
            raise SystemExit("found the credentials but no remotetogo URL; refusing")
        endpoint = next((u for u in urls if b"sig=" in u), urls[0])
        log(f"  anchor {ANCHOR.decode()} at slot 0x{anchor:x}; +8 and +16 are 32-hex")
        return {
            "api_key": api,
            "sig_secret": sig,
            "endpoint": endpoint,
            "anchor_slot": anchor,
            "string_key": key,
            "strings": len(resolved),
        }

    raise SystemExit(
        f"{len(anchors)} {ANCHOR!r} slot(s) found, but none had two 32-hex "
        "neighbours; the constructor layout has changed"
    )


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("lib", type=Path, help="libAirReceiver.so (arm64-v8a)")
    ap.add_argument("-o", "--out", type=Path, required=True)
    args = ap.parse_args()

    data = args.lib.read_bytes()
    log(f"{args.lib} ({len(data)} bytes, sha256 {hashlib.sha256(data).hexdigest()})")
    r = carve(data)
    log(f"  endpoint: {r['endpoint'].decode()}")
    log(f"  api key {len(r['api_key'])}B, sig secret {len(r['sig_secret'])}B")

    args.out.mkdir(parents=True, exist_ok=True)
    (args.out / "cks_api_key.txt").write_bytes(r["api_key"])
    (args.out / "cks_sig_secret.txt").write_bytes(r["sig_secret"])
    (args.out / "carve.json").write_text(
        json.dumps(
            {
                "source_sha256": hashlib.sha256(data).hexdigest(),
                "source_bytes": len(data),
                "anchor_slot": r["anchor_slot"],
                "string_key_sha256": hashlib.sha256(r["string_key"]).hexdigest(),
                "decodable_strings": r["strings"],
                "endpoint": r["endpoint"].decode(),
            },
            indent=2,
        )
        + "\n"
    )
    log(f"  wrote {args.out}")


if __name__ == "__main__":
    main()
