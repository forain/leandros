#!/usr/bin/env python3
"""Extract `i915.kmsdump` frames from a LeandrOS log into PPM files.

usage: kmsdump-extract.py LOG [OUTDIR]   -> OUTDIR/kmsdump-<flip>.ppm
"""
import base64, os, re, sys

log, out = sys.argv[1], (sys.argv[2] if len(sys.argv) > 2 else ".")
os.makedirs(out, exist_ok=True)
cur, buf = None, []
for raw in open(log, "rb"):
    line = raw.decode("latin-1").rstrip("\r\n")
    m = re.search(r"kmsdump begin flip (\d+)", line)
    if m:
        cur, buf = int(m.group(1)), []
        continue
    if cur is not None and re.search(r"kmsdump end flip", line):
        data = base64.b64decode("".join(buf))
        path = os.path.join(out, f"kmsdump-{cur}.ppm")
        open(path, "wb").write(data)
        print(path, len(data))
        cur = None
        continue
    if cur is not None and re.fullmatch(r"[A-Za-z0-9+/=]+", line):
        buf.append(line)
