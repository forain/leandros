#!/usr/bin/env python3
"""Compare the exported dynsym (name/type/size/bind/vis) of Alpine's musl
loader against the golden list scripts/musl/alpine-parity.<arch>.golden.

  alpine-parity.py <arch> [--update] [--against <libc.so>]

--update rewrites the golden list from the pinned Alpine libc (or from the
--against file); --against compares an arbitrary libc.so instead (used to prove
the golden list equals a known-good libc). Host-only, stdlib only. Exits 1 on
any drift.
"""
import os
import struct
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, ".."))
import apkdb  # noqa: E402


def dynsym(data):
    shoff, = struct.unpack_from("<Q", data, 40)
    shentsize, shnum = struct.unpack_from("<HH", data, 58)
    secs = [struct.unpack_from("<IIQQQQIIQQ", data, shoff + i * shentsize) for i in range(shnum)]
    out = set()
    for (_n, typ, _f, _a, off, size, link, _i, _al, ent) in secs:
        if typ != 11:          # SHT_DYNSYM
            continue
        stroff = secs[link][4]
        for o in range(off + ent, off + size, ent):
            name, info, other, shndx, _val, sz = struct.unpack_from("<IBBHQQ", data, o)
            if shndx == 0 or name == 0 or info & 15 in (3, 4):   # undefined / unnamed / section / file
                continue
            e = data.index(b"\0", stroff + name)
            t = info & 15      # FUNC/IFUNC sizes differ (Alpine builds -Os): not compared
            out.add("%s %d %d %d %d" % (data[stroff + name:e].decode(), t,
                                        0 if t in (2, 10) else sz, info >> 4, other & 3))
    return sorted(out)


def main():
    arch = sys.argv[1]
    golden = os.path.join(HERE, "alpine-parity.%s.golden" % arch)
    if "--against" in sys.argv:
        cur = dynsym(open(sys.argv[sys.argv.index("--against") + 1], "rb").read())
    else:
        sys.path.insert(0, os.path.join(HERE, ".."))
        import alpinepkgs
        apk = alpinepkgs.musl_apk(arch)
        cur = dynsym(apkdb.apk_file(apk, "lib/ld-musl-%s.so.1" % arch))
    if "--update" in sys.argv:
        open(golden, "w").write("\n".join(cur) + "\n")
        print("alpine-parity: wrote %d symbols to %s" % (len(cur), golden))
        return 0
    want = open(golden).read().split("\n")[:-1]
    miss, extra = sorted(set(want) - set(cur)), sorted(set(cur) - set(want))
    for l in miss:
        print("MISSING", l)
    for l in extra:
        print("EXTRA  ", l)
    if miss or extra:
        print("alpine-parity: %s DRIFT (%d missing, %d extra)" % (arch, len(miss), len(extra)))
        return 1
    print("alpine-parity: %s ok (%d symbols)" % (arch, len(cur)))
    return 0


sys.exit(main())
