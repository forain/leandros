#!/usr/bin/env python3
"""Module build guards, header patching and alias generation (kmod ABI v1).

  kmodpost.py lint                       SLOTS registry lint (overlap, range, every module listed)
  kmodpost.py slot <arch> <name>         print "<slot_base> <slot_size>" (hex)
  kmodpost.py lib  <arch> <crate> <spec.json> <archive.a> <nm>
                                         guards (a) dependency allowlist, (b) kapi-only undefined
                                         symbols, (c) no defined symbol of a kernel crate
  kmodpost.py elf  <arch> <in.elf> <out.kmod> <buildid-hex-file> <objdump> [--alias FILE]
                                         guards (d) no SIMD/FP, (e) W^X + shape, (f) header patch,
                                         alias lines appended to FILE
Every guard failure is one line "kmodpost: <guard>: <reason>" on stderr and exit 1.
"""
import os, re, struct, subprocess, sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
SLOT = 0x40000
WINDOW = {"aarch64": 0xfffffe0000000000, "x86_64": 0xffffffff90000000}
WINDOW_SIZE = 0x10000000
NSLOTS = WINDOW_SIZE // SLOT
RESERVED_FROM = NSLOTS - 2  # 1022-1023: ktest/testblob
ALLOW_DEPS = {"netctl-abi", "kmod-abi", "kmod-rt", "kmod-kapi", "spin", "bitflags", "leandros-lib",
              "lock_api", "scopeguard"}
FORBIDDEN_CRATES = ["mm", "sched", "drivers", "usb", "kernel", "vfs", "boot"]
FLAG_SOFTFLOAT_CHECKED, FLAG_PERMANENT = 1, 2


def die(guard, msg):
    print(f"kmodpost: {guard}: {msg}", file=sys.stderr)
    sys.exit(1)


def read_slots():
    out = {}
    for ln in open(os.path.join(ROOT, "modules", "SLOTS")):
        ln = ln.split("#")[0].split()
        if not ln:
            continue
        if len(ln) != 3:
            die("slots", f"bad SLOTS line: {' '.join(ln)}")
        out[ln[0]] = (int(ln[1]), int(ln[2]))
    return out


def lint():
    slots = read_slots()
    spans = sorted((f, f + n, nm) for nm, (f, n) in slots.items())
    for i, (a, b, nm) in enumerate(spans):
        if b <= a:
            die("slots", f"{nm}: empty reservation")
        if b > RESERVED_FROM:
            die("slots", f"{nm}: slots {a}-{b-1} reach the reserved test slots (>= {RESERVED_FROM})")
        if i and spans[i - 1][1] > a:
            die("slots", f"{spans[i-1][2]} and {nm} overlap")
    mdir = os.path.join(ROOT, "modules")
    for d in sorted(os.listdir(mdir)):
        if os.path.isfile(os.path.join(mdir, d, "Cargo.toml")) and d.replace("-", "_") not in slots:
            die("slots", f"modules/{d} has no entry in modules/SLOTS")


def slot(arch, name):
    slots = read_slots()
    if name not in slots:
        die("slots", f"{name} not in modules/SLOTS")
    f, n = slots[name]
    return WINDOW[arch] + f * SLOT, n * SLOT


def run(cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, cwd=ROOT, **kw)


def check_lib(arch, crate, spec, archive, nm):
    env = dict(os.environ, RUSTFLAGS="")
    r = subprocess.run(["cargo", "tree", "-p", crate, "-e", "normal", "--prefix", "none", "--target", spec,
                        "-Zjson-target-spec"], capture_output=True, text=True, cwd=ROOT, env=env)
    if r.returncode:
        die("deps", f"cargo tree failed: {r.stderr.strip().splitlines()[-1] if r.stderr else r.returncode}")
    names = {ln.split()[0] for ln in r.stdout.splitlines() if ln.strip()}
    names.discard(crate)
    for n in sorted(names):
        if n not in ALLOW_DEPS and not re.search(r"[-_]hw$", n):
            die("deps", f"{crate} depends on `{n}`, not in the module dependency allowlist")
    # symbols (posix format: "name type value size"; archive members as "path[member]:" lines)
    r = run([nm, "-P", archive])
    if r.returncode:
        die("symbols", f"nm failed: {r.stderr.strip()[:200]}")
    defined, undef = set(), set()
    fb = "|".join(FORBIDDEN_CRATES)
    legacy = re.compile(r"^_ZN(" + "|".join(f"{len(c)}{c}" for c in FORBIDDEN_CRATES) + r")")
    v0 = re.compile(r"^_R.*?C(?:s[0-9a-zA-Z]*_)?(" + "|".join(f"{len(c)}{c}" for c in FORBIDDEN_CRATES) + r")")
    for ln in r.stdout.splitlines():
        p = ln.split()
        if len(p) < 2 or ln.endswith(":"):
            continue
        sym, ty = p[0], p[1]
        if ty in ("U", "w", "v"):
            undef.add(sym)
        elif ty not in ("a", "N", "n", "-"):
            defined.add(sym)
            if legacy.match(sym) or v0.match(sym):
                die("duplicate-global", f"defined symbol `{sym}` belongs to a kernel crate")
    # weak-undefined that is also defined elsewhere is fine; the difference is what must be kapi_
    for sym in sorted(undef - defined):
        if not sym.startswith("kapi_"):
            die("undefined", f"undefined symbol `{sym}` is not a kapi_ export")


def rd(b, o, f):
    return struct.unpack_from(f, b, o)[0]


def parse_elf(b):
    if b[:4] != b"\x7fELF" or b[4] != 2 or b[5] != 1:
        die("shape", "not ELF64 little-endian")
    e = {"type": rd(b, 16, "<H"), "machine": rd(b, 18, "<H"), "entry": rd(b, 24, "<Q")}
    phoff, shoff = rd(b, 32, "<Q"), rd(b, 40, "<Q")
    phentsize, phnum, shentsize, shnum, shstrndx = struct.unpack_from("<HHHHH", b, 54)
    e["ph"] = []
    for i in range(phnum):
        o = phoff + i * phentsize
        t, fl, off, va, pa, fs, ms, al = struct.unpack_from("<IIQQQQQQ", b, o)
        e["ph"].append(dict(type=t, flags=fl, off=off, vaddr=va, filesz=fs, memsz=ms, align=al))
    secs = []
    for i in range(shnum):
        o = shoff + i * shentsize
        nm, ty, fl, addr, off, sz = struct.unpack_from("<IIQQQQ", b, o)
        secs.append(dict(nameoff=nm, type=ty, flags=fl, addr=addr, off=off, size=sz))
    if secs:
        so = secs[shstrndx]["off"]
        for s in secs:
            end = b.index(b"\0", so + s["nameoff"])
            s["name"] = b[so + s["nameoff"]:end].decode()
    e["secs"] = {s["name"]: s for s in secs if "name" in s}
    return e


SIMD_A64 = re.compile(r"(?<![\w.])[vqdsbh]\d{1,2}(?![\w])")


def scan_simd(arch, objdump, path):
    r = run([objdump, "-d", "--no-show-raw-insn", "-j", ".text", path])
    if r.returncode:
        die("simd", f"objdump failed: {r.stderr.strip()[:200]}")
    for ln in r.stdout.splitlines():
        m = re.match(r"\s*([0-9a-f]+):\s*(.*)$", ln)
        if not m:
            continue
        rest = m.group(2)
        if "\t" in rest and re.fullmatch(r"([0-9a-f]{2} ?)+\s*", rest.split("\t")[0]):
            rest = rest.split("\t", 1)[1]  # drop the raw instruction bytes
        ins = re.sub(r"<[^>]*>", "", rest.split("//")[0].split(";")[0]).replace("\t", " ").strip()
        if not ins or ins.startswith("."):
            continue  # data words (.word) are not instructions
        mn, _, ops = ins.partition(" ")
        mn = mn.lower()
        if arch == "aarch64":
            if mn.startswith("f") or re.fullmatch(r"(ld|st)[1-4]r?", mn):
                die("simd", f"aarch64 FP/SIMD instruction at {m.group(1)}: {ins}")
            if mn in ("msr", "mrs") and re.search(r"fpcr|fpsr", ops.lower()):
                die("simd", f"aarch64 FP control register access at {m.group(1)}: {ins}")
            if SIMD_A64.search(ops):
                die("simd", f"aarch64 SIMD/FP register operand at {m.group(1)}: {ins}")
        else:
            if re.search(r"%[xyz]mm\d|%mm\d", ins):
                die("simd", f"x86_64 SSE/MMX instruction at {m.group(1)}: {ins}")


def check_shape(arch, b, e):
    PT_LOAD, PT_DYNAMIC, PT_INTERP, PT_TLS, PT_GNU_STACK = 1, 2, 3, 7, 0x6474e551
    if e["type"] != 2:
        die("shape", "not ET_EXEC")
    loads = [p for p in e["ph"] if p["type"] == PT_LOAD]
    for p in e["ph"]:
        if p["type"] in (PT_TLS, PT_DYNAMIC, PT_INTERP):
            die("shape", f"forbidden program header type {p['type']} (TLS/dynamic/interp)")
        if p["type"] == PT_GNU_STACK and p["flags"] & 1:
            die("shape", "executable PT_GNU_STACK")
    # R, RX and RW in that order; the RW segment is absent for a module with no writable data.
    if [p["flags"] for p in loads] not in ([4, 5, 6], [4, 5]):
        die("wx", f"expected PT_LOAD flags R,RX[,RW], got {[p['flags'] for p in loads]}")
    for p in loads:
        if p["vaddr"] % 4096 or p["off"] % 4096:
            die("shape", "PT_LOAD not page aligned")
    for n in (".got", ".got.plt", ".plt", ".init_array", ".fini_array", ".tdata", ".tbss", ".ctors"):
        s = e["secs"].get(n)
        if s and s["size"]:
            die("shape", f"section {n} is not empty ({s['size']} bytes)")


def fnv1a32(data):
    h = 0x811c9dc5
    for c in data:
        h = ((h ^ c) * 0x01000193) & 0xffffffff
    return h


def alias_lines(b, e, name, entry_size=16):
    """modules.alias lines for the .kmod.ids rows (pci, pci class, usb, of, sdio)."""
    s = e["secs"].get(".kmod.ids")
    out = []
    if not s:
        return out
    ofs = e["secs"].get(".kmod.ofstr")
    for i in range(s["size"] // entry_size):
        bus, a, bb, c, cls, mask = struct.unpack_from("<HHHHII", b, s["off"] + i * entry_size)
        pid = "*" if bb == 0xffff else f"{bb:04x}"
        vid = "*" if a == 0xffff else f"{a:04x}"
        if bus == 1:
            if mask:
                out.append(f"pci {vid} {pid} {cls & mask:06x} {mask:06x} {name}")
            else:
                out.append(f"pci {vid} {pid} {name}")
        elif bus == 2:
            f = lambda sh: "*" if not (mask >> sh) & 0xff else f"{(cls >> sh) & 0xff:02x}"
            out.append(f"usb {vid} {pid} {f(16)} {f(8)} {f(0)} {name}")
        elif bus == 3:
            if not ofs:
                die("alias", "OF row without .kmod.ofstr")
            off = a | (bb << 16)
            if off + c >= ofs["size"] + 0 or b[ofs["off"] + off + c] != 0 or c == 0:
                die("alias", f"OF row {i}: bad offset/length ({off},{c})")
            raw = bytes(b[ofs["off"] + off:ofs["off"] + off + c])
            if b"\0" in raw or fnv1a32(raw) != cls:
                die("alias", f"OF row {i}: hash/len does not match the string")
            out.append(f"of {raw.decode()} {name}")
        elif bus == 5:
            out.append(f"sdio {a:04x} {pid} {name}")
        else:
            die("alias", f"row {i}: unknown bus {bus}")
    return out


def post_elf(arch, inp, outp, bidfile, objdump, alias):
    b = bytearray(open(inp, "rb").read())
    e = parse_elf(bytes(b))
    want = {"aarch64": 183, "x86_64": 62}[arch]
    if e["machine"] != want:
        die("shape", f"wrong e_machine {e['machine']}")
    check_shape(arch, bytes(b), e)
    scan_simd(arch, objdump, inp)
    info = e["secs"].get(".kmod.info")
    if not info or info["size"] < 128:
        die("header", "missing .kmod.info")
    first = [p for p in e["ph"] if p["type"] == 1][0]
    base, size = first["vaddr"], None
    bid = bytes.fromhex(open(bidfile).read().strip())
    if len(bid) != 20:
        die("header", "build id is not 20 bytes")
    o = info["off"]
    name = bytes(b[o + 96:o + 128]).split(b"\0")[0].decode()
    sb, sz = slot(arch, name)
    if base != sb:
        die("header", f"linked at {base:#x}, SLOTS says {sb:#x}")
    flags = rd(b, o + 20, "<I") | FLAG_PERMANENT | (FLAG_SOFTFLOAT_CHECKED if arch == "aarch64" else 0)
    struct.pack_into("<I", b, o + 20, flags)
    b[o + 24:o + 44] = bid
    struct.pack_into("<I", b, o + 44, 20)
    struct.pack_into("<QQQ", b, o + 48, sb, sz, e["entry"])
    ids = e["secs"].get(".kmod.ids")
    if ids and ids["size"]:
        struct.pack_into("<QII", b, o + 80, ids["addr"], ids["size"] // 16, 16)
    open(outp, "wb").write(b)
    if alias:
        with open(alias, "a") as f:
            for ln in alias_lines(bytes(b), e, name):
                f.write(ln + "\n")


def main():
    a = sys.argv[1:]
    if not a:
        print(__doc__)
        sys.exit(2)
    if a[0] == "lint":
        lint()
    elif a[0] == "slot":
        base, size = slot(a[1], a[2])
        print(f"{base:#x} {size:#x}")
    elif a[0] == "lib":
        check_lib(*a[1:6])
    elif a[0] == "elf":
        alias = a[a.index("--alias") + 1] if "--alias" in a else None
        post_elf(a[1], a[2], a[3], a[4], a[5], alias)
    else:
        print(__doc__)
        sys.exit(2)


if __name__ == "__main__":
    main()
