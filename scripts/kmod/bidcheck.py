#!/usr/bin/env python3
"""bidcheck.py: a shipped kernel must find its own lib/modules/<build-id>/ in the
initrd it ships with (and in the staged f2fs root).

  bidcheck.py build <arch>                  build-all gate: every final-<arch>/kernel* variant vs
                                            initrd-<arch>.cpio and target/kmod/<arch>/rootfs.
                                            Only when a module is set (kmod-list/KMOD_EXTRA/staged dirs).
  bidcheck.py pair <kernel> <initrd.cpio>   deploy gate: exit 1 when the initrd carries lib/modules
                                            but not <kernel>'s build-id, or whose modules.load names a missing .kmod.
  bidcheck.py bootcheck <arch>              every =m name in modules/BOOT is in the initrd, in modules.load
                                            (BOOT order) and has an alias row; unknown names fail.
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
    return _sym_build_id(d, phoff, phentsize, phnum, shoff, shentsize, shnum)


def _sym_build_id(d, phoff, phentsize, phnum, shoff, shentsize, shnum):
    """Folded layout (aarch64: note lives inside .rodata): read the bytes at __build_id_start,
    the same place the kernel reads its id at runtime."""
    if not shoff:
        return None
    secs = [struct.unpack_from("<IIQQQQIIQQ", d, shoff + i * shentsize) for i in range(shnum)]
    st = next((s for s in secs if s[1] == 2), None)
    if st is None:
        return None
    strtab = secs[st[6]]
    bs = None
    for i in range(st[5] // 24):
        nm, info, other, shndx, val, size = struct.unpack_from("<IBBHQQ", d, st[4] + i * 24)
        o = strtab[4] + nm
        if shndx and d[o:o + 17] == b"__build_id_start\0":
            bs = val
            break
    if bs is None:
        return None
    for i in range(phnum):
        t, fl, po, va, pa, fs, ms, al = struct.unpack_from("<IIQQQQQQ", d, phoff + i * phentsize)
        if t == 1 and va <= bs < va + fs:
            o = po + (bs - va)
            nsz, dsz, ty = struct.unpack_from("<III", d, o)
            if ty == 3 and dsz == 20:
                return d[o + 12 + ((nsz + 3) & ~3):][:dsz].hex()
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


def cpio_files(path, prefix="lib/modules/"):
    """name -> bytes for the newc members under `prefix`."""
    with open(path, "rb") as f:
        d = f.read()
    out, o = {}, 0
    while o + 110 <= len(d) and d[o:o + 6] == b"070701":
        fsz = int(d[o + 54:o + 62], 16)
        nsz = int(d[o + 94:o + 102], 16)
        name = d[o + 110:o + 110 + nsz - 1].decode(errors="replace")
        if name == "TRAILER!!!":
            break
        o = (o + 110 + nsz + 3) & ~3
        if name.lstrip("./").startswith(prefix):
            out[name.lstrip("./")] = d[o:o + fsz]
        o = (o + fsz + 3) & ~3
    return out


def boot_names():
    """Names in modules/BOOT, in order."""
    p = os.environ.get("LEANDROS_BOOT_FILE") or os.path.join(ROOT, "modules", "BOOT")
    if not os.path.isfile(p):
        return []
    return [w for ln in open(p) for w in ln.split("#")[0].split()]


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


def _modules_load_problems(files):
    """modules.load entries without a .kmod next to them, per build-id directory."""
    bad = []
    for n, data in files.items():
        if n.endswith("/modules.load"):
            d = n[:-len("modules.load")]
            for m in data.decode(errors="replace").replace(",", " ").split():
                if d + m + ".kmod" not in files:
                    bad.append(f"{n} names {m} but {d}{m}.kmod is not in the initrd")
    return bad


def bootcheck(arch, boot=None, initrd=None):
    """Every =m module named in modules/BOOT must be in the initrd, listed in modules.load
    (BOOT order) and have an alias row. Names that are =y (a crate exists, not in kmod-list)
    are skipped; names with no crate at all fail. Returns the problem list."""
    boot = boot_names() if boot is None else boot
    if not boot:
        return []
    kl = os.path.join(ROOT, "target", f"kconfig-{arch}", "kmod-list")
    built = set()
    if os.path.isfile(kl):
        built = {w.replace("-", "_") for w in open(kl).read().split()}
    built |= {w.replace("-", "_") for w in os.environ.get("KMOD_EXTRA", "").split()}
    initrd = initrd or os.path.join(ROOT, f"initrd-{arch}.cpio")
    files = cpio_files(initrd) if os.path.isfile(initrd) else {}
    bad = []
    for m in boot:
        if m.replace("_", "-") and not os.path.isfile(os.path.join(ROOT, "modules", m.replace("_", "-"), "Cargo.toml")) \
                and m not in built:
            bad.append(f"modules/BOOT names {m}: no module crate modules/{m.replace('_', '-')}")
    want = [m for m in boot if m in built]
    if not want:
        return bad
    dirs = sorted({n.rsplit("/", 1)[0] for n in files if n.endswith(".kmod")})
    if not dirs:
        return bad + [f"BOOT modules {want} are =m but {initrd} has no lib/modules/*/*.kmod"]
    for d in dirs:
        load = files.get(d + "/modules.load", b"").decode(errors="replace").replace(",", " ").split()
        alias = files.get(d + "/modules.alias", b"").decode(errors="replace").splitlines()
        for m in want:
            if d + "/" + m + ".kmod" not in files:
                bad.append(f"{d}: BOOT module {m}.kmod missing from the initrd")
            if m not in load:
                bad.append(f"{d}: BOOT module {m} not listed in modules.load")
            if not any(ln.split()[-1:] == [m] for ln in alias):
                bad.append(f"{d}: BOOT module {m} has no modules.alias row")
        order = [m for m in load if m in want]
        if order != [m for m in want if m in load]:
            bad.append(f"{d}: modules.load order {order} differs from BOOT order {want}")
    return bad


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
    for p in bootcheck(arch):
        print(f"bidcheck: FAIL bootcheck: {p}", file=sys.stderr)
        bad += 1
    return 1 if bad else 0


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "build":
        sys.exit(build(sys.argv[2]))
    if len(sys.argv) == 3 and sys.argv[1] == "bootcheck":
        pr = bootcheck(sys.argv[2])
        for p in pr:
            print(f"bidcheck: FAIL bootcheck: {p}", file=sys.stderr)
        sys.exit(1 if pr else 0)
    if len(sys.argv) == 4 and sys.argv[1] == "pair":
        e = pair(sys.argv[2], sys.argv[3])
        if not e and os.path.isfile(sys.argv[3]):
            pr = _modules_load_problems(cpio_files(sys.argv[3]))
            e = "; ".join(pr) if pr else None
        if e:
            print("bidcheck: " + e, file=sys.stderr)
        sys.exit(1 if e else 0)
    sys.exit(__doc__)
