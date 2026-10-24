#!/usr/bin/env python3
"""Fail the build when an x86_64 C program (or the libc archive it links) has
a float-ABI entry point built soft-float.

x86_64-unknown-none is Rust's soft-float target: doubles travel in rdi/rsi/rax
and the compiler never emits an SSE instruction. clang-built C callers pass
and receive doubles in xmm0-7. DOOM's SoundFont synth clipped because libc's
pow/exp/sqrt read integer registers (lane kvmaudio); the class fix builds the
C-facing libleandros_libc.a hard-float (lane floatabi).

Rule: every defined function that takes or returns a float/double under the C
ABI (libm names, strtod/atof, compiler-rt float builtins) must touch an xmm
register somewhere in its body. A soft-float body cannot (no SSE codegen).

Usage: check-c-float-abi.py <linked x86_64 ELF>...  (calls must be resolved)
"""
import os
import re
import shutil
import subprocess
import sys

_MATH = """acos asin atan atan2 cos sin tan cosh sinh tanh acosh asinh atanh exp exp2
expm1 log log2 log10 log1p pow sqrt cbrt hypot fabs floor ceil round trunc rint
nearbyint lrint llrint lround llround fmod remainder remquo copysign fmin fmax
fdim fma ldexp frexp modf scalbn scalbln ilogb logb nextafter erf erfc tgamma
lgamma nan""".split()
_BUILTINS = """powidf2 powisf2 floattidf floattisf floatuntidf floatuntisf fixdfti
fixsfti fixunsdfti fixunssfti floatdidf floatdisf floatundidf floatundisf
floatsidf floatsisf floatunsidf floatunsisf fixdfdi fixsfdi fixunsdfdi
fixunssfdi fixdfsi fixsfsi fixunsdfsi fixunssfsi adddf3 subdf3 muldf3 divdf3
addsf3 subsf3 mulsf3 divsf3 negdf2 negsf2 extendsfdf2 truncdfsf2 truncdfhf2
truncsfhf2 extendhfsf2 muldc3 mulsc3 divdc3 divsc3 eqdf2 nedf2 ltdf2 ledf2
gtdf2 gedf2 unorddf2 eqsf2 nesf2 ltsf2 lesf2 gtsf2 gesf2 unordsf2""".split()
SYMS = set(_MATH) | {m + "f" for m in _MATH} | {"__" + b for b in _BUILTINS} | {
    "strtod", "strtof", "atof"}


def tool(name):
    for cand in (f"/opt/homebrew/opt/llvm/bin/llvm-{name}", shutil.which(f"llvm-{name}"),
                 shutil.which(name)):
        if cand and os.path.exists(cand):
            return cand
    sys.exit(f"check-c-float-abi: no {name} found")


def defined_float_syms(path):
    """{name: address} of defined float-ABI functions (aliases included)."""
    out = subprocess.run([tool("nm"), "--defined-only", path],
                         capture_output=True, text=True).stdout
    syms = {}
    for line in out.splitlines():
        f = line.split()
        if len(f) >= 3 and f[1] in "TtWw" and f[-1] in SYMS:
            syms[f[-1]] = int(f[0], 16)
    return syms


def parse_functions(path):
    """{start address: instruction lines} for every disassembled function.
    Keyed by address: objdump prints only one name for aliased symbols."""
    out = subprocess.run([tool("objdump"), "-d", "--no-show-raw-insn", path],
                         capture_output=True, text=True).stdout
    funcs, cur = {}, None
    for line in out.splitlines():
        m = re.match(r"^([0-9a-f]+) <[^>]+>:", line)
        if m:
            cur = int(m.group(1), 16)
            funcs.setdefault(cur, [])
        elif cur is not None and re.match(r"^\s+[0-9a-f]+:", line):
            funcs[cur].append(line)
    return funcs


def uses_sse(funcs, addr, depth=3):
    """True if the function at addr, or one it calls/jumps to (depth-limited),
    touches an xmm register, or tail-jumps indirectly (CPU dispatch, e.g.
    libm's fma; not judgeable). Hard-float compiler_builtins exports are often
    thin frame + tail-call wrappers around a mangled body."""
    seen, todo = set(), [(addr, 0)]
    while todo:
        a, d = todo.pop()
        if a in seen or a not in funcs:
            continue
        seen.add(a)
        for insn in funcs[a]:
            if "xmm" in insn or re.search(r"\sjmp\w*\s+\*", insn):
                return True
            m = re.search(r"\s(?:call|jmp)\w*\s+0x([0-9a-f]+) <([^>+]+)>", insn)
            if m and d < depth:
                todo.append((int(m.group(1), 16), d + 1))
    return False


def check(path):
    syms = defined_float_syms(path)
    if not syms:
        print(f"check-c-float-abi: no float-ABI symbols in {path} (nothing to check)")
        return 0
    funcs = parse_functions(path)
    bad = sorted(s for s, a in syms.items() if not uses_sse(funcs, a))
    if bad:
        print(f"check-c-float-abi: FAIL in {path}, {len(bad)}/{len(syms)} float-ABI "
              f"symbols are soft-float (never touch xmm): {' '.join(bad)}")
        return 1
    print(f"check-c-float-abi: OK ({len(syms)} float-ABI symbols use xmm) in "
          f"{os.path.basename(path)}")
    return 0


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    return max(check(p) for p in sys.argv[1:])


if __name__ == "__main__":
    sys.exit(main())
