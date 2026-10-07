#!/usr/bin/env python3
"""License audit of every C source and header that went into libnvrm.a.

Reads the compiler dependency files (*.d) that NVIDIA's Makefile leaves in
$OPENRM_CACHE/build-x86_64 after ports/openrm/build.sh, so the file list is
exactly what was compiled, not a guess. Prints a count per (kind, license).
"""
import collections
import glob
import os
import re
import subprocess

HERE = os.path.dirname(os.path.abspath(__file__))
cache, src = subprocess.check_output(
    ["bash", "-c", f'source "{HERE}/version.sh"; echo "$OPENRM_CACHE"; echo "$OPENRM_SRC"'],
    text=True).split("\n")[:2]
nv = os.path.join(src, "src", "nvidia")

files = set()
for d in glob.glob(os.path.join(cache, "build-x86_64", "*.d")):
    for tok in open(d).read().replace("\\\n", " ").split():
        if tok.endswith((".c", ".h")) and "lib/clang" not in tok and not tok.startswith("/opt"):
            files.add(tok if tok.startswith("/") else os.path.normpath(os.path.join(nv, tok)))
# The Makefile strips each .c from its own .d, so take sources from the
# Makefile's lists (srcs.mk + libspdm's openspdm.mk) instead.
mkvars = {"LIBSPDM_SOURCE_DIR": "src/libraries/libspdm", "LIBSPDM_VERSION": "3.1.1"}
for mk in ("srcs.mk", "src/libraries/libspdm/nvidia/openspdm.mk"):
    for line in open(os.path.join(nv, mk)):
        m = re.match(r"\s*(SRCS|SRCS_CXX|LIBSPDM_SOURCES)\s*\+=\s*(\S+)", line)
        if m:
            path = re.sub(r"\$\((\w+)\)", lambda v: mkvars.get(v.group(1), ""), m.group(2))
            files.add(os.path.normpath(os.path.join(nv, path)))
files = {f for f in files if "/ports/openrm" not in f and os.path.exists(f)}


def lic(p):
    h = open(p, errors="replace").read(6000)
    m = re.search(r"SPDX-License-Identifier:\s*([^\n*]+)", h)
    if m:
        return m.group(1).strip()
    if "Permission is hereby granted, free of charge" in h:
        return "MIT (full text, no SPDX tag)"
    if "Permission to use, copy, modify, distribute, and sell" in h:
        return "SciTech x86emu (permissive, MIT-style)"
    m = re.search(r".*(Copyright|copyright|License).*", h)
    if m:
        return "OTHER: " + m.group(0).strip()[:90]
    return "no notice (covered by repo COPYING)"


count = collections.Counter()
example = {}
for p in sorted(files):
    key = ("source" if p.endswith(".c") else "header", lic(p))
    count[key] += 1
    example.setdefault(key, p.replace(src + "/", ""))
print(f"{len(files)} files compiled into nv-kernel.o")
for k, v in sorted(count.items()):
    print(f"{v:5d}  {k[0]:6s}  {k[1]}   e.g. {example[k]}")
