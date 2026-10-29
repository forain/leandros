#!/usr/bin/env python3
"""ksyms.py <kernel-elf> <out.ld>: export the kernel's kapi_* symbols to a linker script.

Writes PROVIDE(kapi_x = 0x...); lines to <out.ld> and the kernel build-id (hex) to
<out.ld>.buildid. Asserts every kapi_ name is unique and that their count equals the
.ksymtab length (__ksymtab_start/_end, 16-byte entries).
"""
import struct, sys

def main():
    elf, out = sys.argv[1], sys.argv[2]
    d = open(elf, "rb").read()
    assert d[:4] == b"\x7fELF" and d[4] == 2 and d[5] == 1, "not ELF64 LE"
    shoff, = struct.unpack_from("<Q", d, 0x28)
    shentsize, shnum = struct.unpack_from("<HH", d, 0x3A)
    phoff, = struct.unpack_from("<Q", d, 0x20)
    phentsize, phnum = struct.unpack_from("<HH", d, 0x36)
    secs = [struct.unpack_from("<IIQQQQIIQQ", d, shoff + i * shentsize) for i in range(shnum)]
    symtab = next(s for s in secs if s[1] == 2)
    strtab = secs[symtab[6]]
    syms = {}
    names = []
    for i in range(symtab[5] // 24):
        nm, info, other, shndx, val, size = struct.unpack_from("<IBBHQQ", d, symtab[4] + i * 24)
        if shndx == 0:
            continue
        o = strtab[4] + nm
        n = d[o:d.index(b"\0", o)].decode()
        if n.startswith("kapi_"):
            names.append(n)
        syms[n] = val
    assert len(names) == len(set(names)), "duplicate kapi_ symbol"
    nks = (syms["__ksymtab_end"] - syms["__ksymtab_start"]) // 16
    assert nks == len(names), "ksymtab has %d entries, %d kapi_ symbols" % (nks, len(names))
    # build-id note via the PT_LOAD that contains it
    bs = syms["__build_id_start"]
    off = None
    for i in range(phnum):
        t, fl, po, va, pa, fs, ms, al = struct.unpack_from("<IIQQQQQQ", d, phoff + i * phentsize)
        if t == 1 and va <= bs < va + fs:
            off = po + (bs - va)
    assert off is not None, "build-id note not in a PT_LOAD"
    namesz, descsz, ty = struct.unpack_from("<III", d, off)
    assert ty == 3 and descsz == 20, "bad build-id note"
    bid = d[off + 12 + ((namesz + 3) & ~3):][:descsz]
    with open(out, "w") as f:
        for n in sorted(names):
            f.write("PROVIDE(%s = 0x%x);\n" % (n, syms[n]))
    with open(out + ".buildid", "w") as f:
        f.write(bid.hex())
    print("ksyms: %d symbols, build-id %s" % (len(names), bid.hex()))

main()
