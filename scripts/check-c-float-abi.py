#!/usr/bin/env python3
"""Fail the build when an x86_64 C program's libm entry points use the wrong
float ABI.

userland/libc is built for Rust's soft-float x86_64-unknown-none target, so a
plain `extern "C" fn sqrt(f64) -> f64` reads its argument from rdi, while
clang-built C callers pass it in xmm0. That mismatch made DOOM's SoundFont
synth compute garbage gains and clip (lane kvmaudio, 2026-10-24). The C entry
points must be trampolines that read xmm0 first.

Usage: check-c-float-abi.py <x86_64 ELF or archive>
"""
import os
import re
import shutil
import subprocess
import sys

SYMS = ["pow", "exp", "log", "log10", "tan", "sqrt"]


def objdump():
    for cand in ("/opt/homebrew/opt/llvm/bin/llvm-objdump", shutil.which("llvm-objdump"),
                 shutil.which("objdump")):
        if cand and os.path.exists(cand):
            return cand
    sys.exit("check-c-float-abi: no objdump found")


def main():
    path = sys.argv[1]
    out = subprocess.run([objdump(), "-d", "--no-show-raw-insn",
                          "--disassemble-symbols=" + ",".join(SYMS), path],
                         capture_output=True, text=True).stdout
    bodies, cur = {}, None
    for line in out.splitlines():
        m = re.match(r"^[0-9a-f]+ <(\w+)>:", line)
        if m:
            cur = m.group(1) if m.group(1) in SYMS else None
            if cur:
                bodies.setdefault(cur, [])
        elif cur and re.match(r"^\s+[0-9a-f]+:", line):
            bodies[cur].append(line)
    bad = []
    for sym, insns in bodies.items():
        head = " ".join(insns[:3])
        if "%xmm0" not in head and "xmm0" not in head:
            bad.append(f"{sym}: {insns[0].strip() if insns else '<empty>'}")
    if not bodies:
        print(f"check-c-float-abi: no libm symbols in {path} (nothing to check)")
        return 0
    if bad:
        print("check-c-float-abi: FAIL, soft-float body exported to C callers:")
        for b in bad:
            print("  " + b)
        return 1
    print(f"check-c-float-abi: OK ({len(bodies)} symbols read xmm0) in {os.path.basename(path)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
