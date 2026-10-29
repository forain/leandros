#!/usr/bin/env python3
"""bidcheck.py: a shipped kernel must find its own lib/modules/<build-id>/ in the
initrd it ships with (and in the staged f2fs root).

  bidcheck.py build <arch>                  build-all gate: every final-<arch>/kernel* variant vs
                                            initrd-<arch>.cpio and target/kmod/<arch>/rootfs.
                                            Only when a module is set (kmod-list/KMOD_EXTRA/staged dirs).
  bidcheck.py pair <kernel> <initrd.cpio>   deploy gate: exit 1 when the initrd carries lib/modules
                                            but not <kernel>'s build-id.
"""
import os, struct, sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))


def build_id(path):
    """GNU build-id (hex) of an ELF64 file via its SHT_NOTE sections / PT_NOTE segments, or None."""
    with open(path, "rb") as f:
        d = f.read()
    if d[:4] != b"\x7fELF" or d[4] != 2:
        return None
    regions = []
    phoff, = struct.unpack_from("<Q", d, 0x20)
    phentsize, phnum = struct.unpack_from("<HH", d, 0x36)
    for i in range(phnum):
        t, fl, po, va, pa, fs, ms, al = struct.unpack_from("<IIQQQQQQ", d, phoff + i * phentsize)
        if t == 4:
            regions.append((po, po + fs))
    shoff, = struct.unpack_from("<Q", d, 0x28)
    shentsize, shnum = struct.unpack_from("<HH", d, 0x3A)
    for i in range(shnum if shoff else 0):
        _, ty, _, _, so, ssz = struct.unpack_from("<IIQQQQ", d, shoff + i * shentsize)
        if ty == 7:
            regions.append((so, so + ssz))
    for o, end in regions:
        while o + 12 <= end:
            nsz, dsz, ty = struct.unpack_from("<III", d, o)
            name = d[o + 12:o + 12 + nsz].rstrip(b"\0")
            doff = o + 12 + ((nsz + 3) & ~3)
            if ty == 3 and name == b"GNU":
                return d[doff:doff + dsz].hex()
            o = doff + ((dsz + 3) & ~3)
    return None


def cpio_names(path):
    """Member names of a newc cpio archive."""
    with open(path, "rb") as f:
        d = f.read()
    names, o = [], 0
    while o + 110 <= len(d) and d[o:o + 6] == b"070701":
        fsz = int(d[o + 54:o + 62], 16)
        nsz = int(d[o + 94:o + 102], 16)
        name = d[o + 110:o + 110 + nsz - 1].decode(errors="replace")
        if name == "TRAILER!!!":
            break
        names.append(name.lstrip("./"))
        o = (o + 110 + nsz + 3) & ~3
        o = (o + fsz + 3) & ~3
    return names


def initrd_ids(path):
    ids = set()
    for n in cpio_names(path):
        p = n.split("/")
        if len(p) >= 3 and p[0] == "lib" and p[1] == "modules" and p[2]:
            ids.add(p[2])
    return ids


def pair(kernel, initrd):
    bid = build_id(kernel)
    if not bid:
        return None  # no note: nothing to match
    ids = initrd_ids(initrd)
    if ids and bid not in ids:
        return (f"{kernel} has build-id {bid} but {initrd} only carries lib/modules/"
                f"{{{', '.join(sorted(ids))}}}: the kernel would not find its modules "
                f"(rebuild with scripts/build-all.sh)")
    return None


def build(arch):
    kdir = os.path.join(ROOT, "target", "kmod", arch)
    have_mods = False
    kl = os.path.join(ROOT, "target", f"kconfig-{arch}", "kmod-list")
    if os.path.isfile(kl) and os.path.getsize(kl) > 0:
        have_mods = True
    if os.environ.get("KMOD_EXTRA", "").strip() or os.environ.get("KMOD_TEST") == "1":
        have_mods = True
    if not have_mods:
        return 0
    initrd = os.path.join(ROOT, f"initrd-{arch}.cpio")
    ids = initrd_ids(initrd) if os.path.isfile(initrd) else set()
    bad = 0
    for v in ("kernel", "kernel-direct"):
        k = os.path.join(ROOT, "target", f"final-{arch}", v)
        if not os.path.isfile(k):
            continue
        bid = build_id(k)
        if not bid:
            print(f"bidcheck: FAIL {k} has no GNU build-id", file=sys.stderr)
            bad += 1
            continue
        if bid not in ids:
            print(f"bidcheck: FAIL {v} build-id {bid} has no lib/modules dir in {initrd}", file=sys.stderr)
            bad += 1
        if not os.path.isdir(os.path.join(kdir, "rootfs", "lib", "modules", bid)):
            print(f"bidcheck: FAIL {v} build-id {bid} has no lib/modules dir in the root staging", file=sys.stderr)
            bad += 1
        if not bad:
            print(f"bidcheck: {arch} {v} {bid[:12]} ok (initrd + root)")
    return 1 if bad else 0


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "build":
        sys.exit(build(sys.argv[2]))
    if len(sys.argv) == 4 and sys.argv[1] == "pair":
        e = pair(sys.argv[2], sys.argv[3])
        if e:
            print("bidcheck: " + e, file=sys.stderr)
        sys.exit(1 if e else 0)
    sys.exit(__doc__)
