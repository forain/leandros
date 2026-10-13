#!/usr/bin/env python3
"""compare.py HOST_ELF GUEST_ELF — compare a host-built and a guest-built kernel.

Prints size, sha256, and the allocated sections side by side (address, size),
then the bytes that differ per section. Paths baked into panic locations
(the source checkout, CARGO_HOME, the sysroot) differ between the two builds
unless both used the same --remap-path-prefix, so .rodata is expected to differ
by roughly the length difference of those prefixes. Stdlib only.
"""
import hashlib
import struct
import sys


def sections(path):
    d = open(path, "rb").read()
    assert d[:4] == b"\x7fELF" and d[4] == 2, "64-bit ELF expected"
    shoff, = struct.unpack_from("<Q", d, 0x28)
    shentsize, shnum, shstrndx = struct.unpack_from("<HHH", d, 0x3A)
    hdrs = [struct.unpack_from("<IIQQQQIIQQ", d, shoff + i * shentsize) for i in range(shnum)]
    stro = hdrs[shstrndx][4]
    out = {}
    for h in hdrs:
        name_off, typ, flags, addr, off, size = h[:6]
        end = d.index(b"\0", stro + name_off)
        name = d[stro + name_off:end].decode()
        if name and flags & 2:  # SHF_ALLOC
            body = d[off:off + size] if typ != 8 else b""  # NOBITS has no bytes
            out[name] = (addr, size, body)
    return d, out


def main():
    h, hs = sections(sys.argv[1])
    g, gs = sections(sys.argv[2])
    print(f"host : {len(h):>9} bytes  sha256 {hashlib.sha256(h).hexdigest()}")
    print(f"guest: {len(g):>9} bytes  sha256 {hashlib.sha256(g).hexdigest()}")
    print(f"{'section':<22}{'host addr':>12}{'host size':>11}{'guest addr':>12}{'guest size':>11}  bytes differing")
    for n in sorted(set(hs) | set(gs), key=lambda n: (hs.get(n) or gs.get(n))[0]):
        a = hs.get(n, (0, 0, b""))
        b = gs.get(n, (0, 0, b""))
        diff = sum(x != y for x, y in zip(a[2], b[2])) + abs(len(a[2]) - len(b[2]))
        print(f"{n:<22}{a[0]:>#12x}{a[1]:>11}{b[0]:>#12x}{b[1]:>11}  {diff}")
    print("identical" if h == g else "files differ")


if __name__ == "__main__":
    main()
