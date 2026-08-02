#!/usr/bin/env python3
"""Minimal AArch64 Unicorn harness for calling functions inside an Android .so.

Vendored from the RE tooling in `re-shell/artifacts/airreceiver-cast-signatures/`,
which is not under version control. Used by `airreceiver-carve.py` to recover the
`dbio` key-encryption key, which is computed at runtime by a digest whose body is a
bytecode interpreter — emulating it is cheaper and more durable than lifting the VM.

Maps the PT_LOADs at their link addresses, applies R_AARCH64_RELATIVE /
GLOB_DAT / JUMP_SLOT / ABS64, and stubs the libc imports so a single function
can be invoked directly.  Enough to run leaf routines in libAirReceiver.so;
not a general-purpose loader.

Ghidra loads these libraries at image base 0x100000, so
    ELF vaddr = Ghidra address - 0x100000
and this harness works in ELF vaddrs.
"""
import struct

from unicorn import UC_ARCH_ARM64, UC_HOOK_CODE, UC_MODE_LITTLE_ENDIAN, Uc
from unicorn.arm64_const import (UC_ARM64_REG_LR, UC_ARM64_REG_PC,
                                 UC_ARM64_REG_SP, UC_ARM64_REG_TPIDR_EL0,
                                 UC_ARM64_REG_X0, UC_ARM64_REG_X1,
                                 UC_ARM64_REG_X2, UC_ARM64_REG_X3,
                                 UC_ARM64_REG_X4, UC_ARM64_REG_X5)

PAGE = 0x1000
STACK, STACK_SZ = 0x7F0000000, 0x400000
HEAP, HEAP_SZ = 0x600000000, 0x1000000
STUBS = 0x500000000
RETADDR = 0x4FF000000
TLS = 0x400000000

_REGS = [UC_ARM64_REG_X0, UC_ARM64_REG_X1, UC_ARM64_REG_X2,
         UC_ARM64_REG_X3, UC_ARM64_REG_X4, UC_ARM64_REG_X5]

_STUBBED = ("malloc", "free", "calloc", "realloc", "memset", "memcpy",
            "memmove", "memcmp", "strlen", "__strlen_chk", "read",
            "__read_chk", "lseek64", "abort", "__stack_chk_fail",
            "__memcpy_chk", "__memset_chk")


def _align_dn(x):
    return x & ~(PAGE - 1)


def _align_up(x):
    return (x + PAGE - 1) & ~(PAGE - 1)


class Emu:
    def __init__(self, path):
        self.data = D = open(path, "rb").read()
        self._parse(D)

        self.uc = uc = Uc(UC_ARCH_ARM64, UC_MODE_LITTLE_ENDIAN)
        lo = min(_align_dn(v) for _, v, _, _ in self.segs)
        hi = max(_align_up(v + m) for _, v, _, m in self.segs)
        uc.mem_map(lo, hi - lo)
        for off, va, fsz, _ in self.segs:
            uc.mem_write(va, D[off:off + fsz])
        for base, size in ((STACK, STACK_SZ), (HEAP, HEAP_SZ), (STUBS, PAGE),
                           (RETADDR, PAGE), (TLS, PAGE)):
            uc.mem_map(base, size)
        uc.reg_write(UC_ARM64_REG_TPIDR_EL0, TLS)
        uc.mem_write(TLS + 0x28, struct.pack("<Q", 0xDEADBEEFCAFE0000))

        self.brk = HEAP + 0x1000
        self._relocate()
        self._stubs()
        uc.hook_add(UC_HOOK_CODE, self._dispatch)

    # ---- ELF -------------------------------------------------------------
    def _parse(self, D):
        e_phoff = struct.unpack_from("<Q", D, 0x20)[0]
        e_phentsize, e_phnum = struct.unpack_from("<HH", D, 0x36)
        self.segs = []
        for i in range(e_phnum):
            o = e_phoff + i * e_phentsize
            if struct.unpack_from("<I", D, o)[0] != 1:      # PT_LOAD
                continue
            p_off, p_va, _, p_fsz, p_msz, _ = struct.unpack_from("<QQQQQQ", D, o + 8)
            self.segs.append((p_off, p_va, p_fsz, p_msz))

        e_shoff = struct.unpack_from("<Q", D, 0x28)[0]
        e_shentsize, e_shnum, e_shstrndx = struct.unpack_from("<HHH", D, 0x3A)
        shstr = struct.unpack_from("<Q", D, e_shoff + e_shstrndx * e_shentsize + 0x18)[0]
        self.sec = {}
        for i in range(e_shnum):
            o = e_shoff + i * e_shentsize
            sh_name = struct.unpack_from("<I", D, o)[0]
            _, sh_off, sh_size = struct.unpack_from("<QQQ", D, o + 0x10)
            end = D.index(b"\0", shstr + sh_name)
            self.sec[D[shstr + sh_name:end].decode()] = (sh_off, sh_size)

        ds_off, ds_size = self.sec[".dynsym"]
        dstr_off, _ = self.sec[".dynstr"]
        self.dynsym = []
        for i in range(ds_size // 24):
            o = ds_off + i * 24
            st_name = struct.unpack_from("<I", D, o)[0]
            st_shndx = struct.unpack_from("<H", D, o + 6)[0]
            st_value = struct.unpack_from("<Q", D, o + 8)[0]
            end = D.index(b"\0", dstr_off + st_name)
            self.dynsym.append((D[dstr_off + st_name:end].decode(), st_value,
                                st_shndx != 0))

    def _relocate(self):
        self.imports = {}
        for name in (".rela.dyn", ".rela.plt"):
            if name not in self.sec:
                continue
            off, size = self.sec[name]
            for p in range(off, off + size, 24):
                r_offset, r_info, r_addend = struct.unpack_from("<QQq", self.data, p)
                rtype, rsym = r_info & 0xFFFFFFFF, r_info >> 32
                if rtype == 1027:                            # RELATIVE
                    self.uc.mem_write(r_offset, struct.pack("<Q", r_addend))
                elif rtype in (1025, 1026, 257):             # GLOB_DAT/JUMP_SLOT/ABS64
                    sym, value, defined = self.dynsym[rsym]
                    if defined:
                        self.uc.mem_write(r_offset, struct.pack("<Q", value + r_addend))
                    else:
                        self.imports.setdefault(sym, []).append(r_offset)

    def _stubs(self):
        self.by_addr = {}
        for i, name in enumerate(_STUBBED):
            addr = STUBS + i * 8
            self.by_addr[addr] = name
            self.uc.mem_write(addr, b"\xc0\x03\x5f\xd6")            # ret
            for slot in self.imports.get(name, []):
                self.uc.mem_write(slot, struct.pack("<Q", addr))
        # Anything else returns 0.  Without this, an unrelocated PLT GOT slot
        # sends the CPU to pc=0 the moment an unexpected import is touched.
        generic = STUBS + len(_STUBBED) * 8
        self.uc.mem_write(generic, b"\x00\x00\x80\xd2\xc0\x03\x5f\xd6")
        self.generic_stub = generic
        for name, slots in self.imports.items():
            if name in _STUBBED:
                continue
            for slot in slots:
                self.uc.mem_write(slot, struct.pack("<Q", generic))

    def _dispatch(self, uc, address, size, user):
        name = self.by_addr.get(address)
        if name is None:
            return
        getattr(self, "h_" + name.lstrip("_"))()
        uc.reg_write(UC_ARM64_REG_PC, uc.reg_read(UC_ARM64_REG_LR))

    # ---- libc ------------------------------------------------------------
    def _a(self, i):
        return self.uc.reg_read(_REGS[i])

    def _ret(self, v):
        self.uc.reg_write(UC_ARM64_REG_X0, v & 0xFFFFFFFFFFFFFFFF)

    def _alloc(self, n):
        p = self.brk
        self.brk = (self.brk + n + 0xF) & ~0xF
        return p

    def h_malloc(self):
        self._ret(self._alloc(self._a(0)))

    def h_calloc(self):
        n = self._a(0) * self._a(1)
        p = self._alloc(n)
        self.uc.mem_write(p, b"\0" * n)
        self._ret(p)

    def h_realloc(self):
        self._ret(self._alloc(self._a(1)))

    def h_free(self):
        self._ret(0)

    def h_abort(self):
        raise RuntimeError("abort()")

    def h_stack_chk_fail(self):
        raise RuntimeError("stack protector tripped")

    def h_memset(self):
        d, c, n = self._a(0), self._a(1), self._a(2)
        self.uc.mem_write(d, bytes([c & 0xFF]) * n)
        self._ret(d)

    h_memset_chk = h_memset

    def h_memcpy(self):
        d, s, n = self._a(0), self._a(1), self._a(2)
        self.uc.mem_write(d, bytes(self.uc.mem_read(s, n)))
        self._ret(d)

    h_memmove = h_memcpy
    h_memcpy_chk = h_memcpy

    def h_memcmp(self):
        a, b, n = self._a(0), self._a(1), self._a(2)
        x, y = bytes(self.uc.mem_read(a, n)), bytes(self.uc.mem_read(b, n))
        self._ret(0 if x == y else (1 if x > y else -1))

    def h_strlen(self):
        p, n = self._a(0), 0
        while self.uc.mem_read(p + n, 1)[0]:
            n += 1
        self._ret(n)

    h_strlen_chk = h_strlen

    def h_read(self):
        self._ret(0)

    h_read_chk = h_read

    def h_lseek64(self):
        self._ret(0)

    # ---- calling ---------------------------------------------------------
    def call(self, addr, *args):
        uc = self.uc
        uc.reg_write(UC_ARM64_REG_SP, STACK + STACK_SZ - 0x1000)
        uc.reg_write(UC_ARM64_REG_LR, RETADDR)
        for i, v in enumerate(args):
            uc.reg_write(_REGS[i], v)
        uc.emu_start(addr, RETADDR)
        return uc.reg_read(UC_ARM64_REG_X0)

    def write(self, addr, b):
        self.uc.mem_write(addr, bytes(b))

    def read(self, addr, n):
        return bytes(self.uc.mem_read(addr, n))

    def cstr(self, addr):
        n = 0
        while self.read(addr + n, 1)[0]:
            n += 1
        return self.read(addr, n)

    def scratch(self, n=0x1000):
        return self._alloc(n)
