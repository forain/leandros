#!/usr/bin/env python3
"""Fail the build when a kernel function's stack frame outgrows the kernel stack.

Every task in this kernel runs on a fixed 128 KiB kernel stack allocated from the
buddy allocator (`sched::KERNEL_STACK_ORDER`), with no guard page: the frames
below it are ordinary allocatable memory, so a frame larger than the stack does
not fault, it silently overwrites whatever the allocator handed out next. That
is not a hypothetical. `f2fs::mount` once assembled a 71,872 B `MountState` as a
stack temporary, which LLVM turned into a 155,880 B frame -- 2.4x the whole
stack -- and the ~90 KB that landed underneath took out the page tables on one
host and something survivable on another, which is why it read for a while as a
host-dependent boot failure rather than as an overflow (TODO.md item 15).

Nothing about that was visible in a passing build, so this check makes it
visible. It reads the `.stack_sizes` section that `-Zemit-stack-sizes` puts in
the linked ELF -- exact per-function frame sizes straight from the backend, no
disassembly heuristics and no external tools -- and exits non-zero if any
function exceeds the budget.

Usage:
    check-stack-frames.py <kernel-elf> [--budget BYTES] [--top N] [--list]
"""

import sys
import struct

# A frame this large is a bug even though it still fits: it leaves too little
# for everything below it on the call chain. 48 KiB is 37% of the 128 KiB
# stack, and ~14 KiB above the largest frame the tree legitimately has.
DEFAULT_BUDGET = 48 * 1024


def _u(data, off, size, little):
    return int.from_bytes(data[off:off + size], "little" if little else "big")


def read_sections(blob):
    """Minimal ELF64 section-table walk. Returns (sections, little_endian)."""
    if blob[:4] != b"\x7fELF":
        raise SystemExit("not an ELF file")
    if blob[4] != 2:
        raise SystemExit("only ELF64 is supported")
    little = blob[5] == 1
    e = "<" if little else ">"
    e_shoff, = struct.unpack_from(e + "Q", blob, 0x28)
    e_shentsize, e_shnum, e_shstrndx = struct.unpack_from(e + "HHH", blob, 0x3A)

    raw = []
    for i in range(e_shnum):
        off = e_shoff + i * e_shentsize
        name, sh_type, _flags, addr, offset, size, link, _info, _align, entsize = \
            struct.unpack_from(e + "IIQQQQIIQQ", blob, off)
        raw.append(dict(name_off=name, type=sh_type, addr=addr, offset=offset,
                        size=size, link=link, entsize=entsize))

    strtab = raw[e_shstrndx]
    names = blob[strtab["offset"]:strtab["offset"] + strtab["size"]]

    def name_at(o):
        end = names.index(b"\x00", o)
        return names[o:end].decode("utf-8", "replace")

    for s in raw:
        s["name"] = name_at(s["name_off"])
    return raw, little


def read_symbols(blob, sections, little):
    """Map function address -> symbol name, from .symtab."""
    e = "<" if little else ">"
    symtab = next((s for s in sections if s["name"] == ".symtab"), None)
    if symtab is None:
        return {}
    strtab = sections[symtab["link"]]
    strs = blob[strtab["offset"]:strtab["offset"] + strtab["size"]]
    out = {}
    count = symtab["size"] // symtab["entsize"]
    for i in range(count):
        off = symtab["offset"] + i * symtab["entsize"]
        st_name, st_info, _other, _shndx, st_value, _size = \
            struct.unpack_from(e + "IBBHQQ", blob, off)
        if st_info & 0xF != 2:  # STT_FUNC
            continue
        end = strs.index(b"\x00", st_name)
        out.setdefault(st_value, strs[st_name:end].decode("utf-8", "replace"))
    return out


def uleb128(data, off):
    val = 0
    shift = 0
    while True:
        b = data[off]
        off += 1
        val |= (b & 0x7F) << shift
        if not b & 0x80:
            return val, off
        shift += 7


def read_stack_sizes(blob, sections, little):
    """Parse `.stack_sizes`: an 8-byte address then a ULEB128 frame size, repeated."""
    sec = next((s for s in sections if s["name"] == ".stack_sizes"), None)
    if sec is None:
        return None
    data = blob[sec["offset"]:sec["offset"] + sec["size"]]
    out = []
    off = 0
    while off + 8 <= len(data):
        addr = _u(data, off, 8, little)
        off += 8
        size, off = uleb128(data, off)
        out.append((addr, size))
    return out


def demangle(name):
    """Enough of Rust's v0 mangling to read the report: drop the hash suffix."""
    if name.startswith("_RNv") or name.startswith("_R"):
        return name
    if name.startswith("_ZN") and name.endswith("E"):
        body, i, parts = name[3:-1], 0, []
        while i < len(body) and body[i].isdigit():
            j = i
            while body[j].isdigit():
                j += 1
            n = int(body[i:j])
            parts.append(body[j:j + n])
            i = j + n
        if parts and parts[-1].startswith("17h"):
            parts.pop()
        return "::".join(parts) or name
    return name


def main(argv):
    if len(argv) < 2:
        raise SystemExit(__doc__)
    path = argv[1]
    budget = DEFAULT_BUDGET
    top = 10
    list_all = "--list" in argv
    if "--budget" in argv:
        budget = int(argv[argv.index("--budget") + 1])
    if "--top" in argv:
        top = int(argv[argv.index("--top") + 1])

    with open(path, "rb") as fh:
        blob = fh.read()

    sections, little = read_sections(blob)
    frames = read_stack_sizes(blob, sections, little)
    if frames is None:
        print("check-stack-frames: no .stack_sizes section in %s -- build the "
              "kernel with -Zemit-stack-sizes in RUSTFLAGS, or this check is "
              "silently doing nothing." % path, file=sys.stderr)
        return 2

    syms = read_symbols(blob, sections, little)
    named = sorted(((size, syms.get(addr, "<0x%x>" % addr)) for addr, size in frames),
                   reverse=True)

    if list_all:
        for size, name in named:
            print("%9d  %s" % (size, demangle(name)))
        return 0

    over = [(s, n) for s, n in named if s > budget]

    print("check-stack-frames: %d functions, largest %d B, budget %d B"
          % (len(named), named[0][0] if named else 0, budget))
    for size, name in named[:top]:
        flag = "  <-- OVER" if size > budget else ""
        print("    %9d B  %s%s" % (size, demangle(name), flag))

    if over:
        print("\ncheck-stack-frames: FAIL -- %d function(s) over the %d B budget."
              % (len(over), budget))
        print("The kernel stack is %d KiB with no guard page, so a frame that "
              "outgrows it corrupts\nunrelated memory instead of faulting. Build "
              "the value in place (Box::new_uninit) rather\nthan letting rustc "
              "materialise it as a stack temporary -- see TODO.md item 15."
              % (128,))
        return 1

    print("check-stack-frames: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
