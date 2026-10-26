#!/usr/bin/env python3
"""rpi5checktest - the kernel's --features rpi5 (Raspberry Pi 5) variant still compiles.

Host-only: runs scripts/check-rpi5.sh (cargo check, no boot). Nothing else
builds the cfg-gated rpi5 code, so it drifts silently (blkdev_sdhci, 2026-10).
Usage: rpi5checktest.py [arch]   (arch ignored; REPO=<checkout> for another tree)
"""
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.environ.get("REPO", os.path.abspath(os.path.join(HERE, "../../..")))
r = subprocess.run([os.path.join(REPO, "scripts/check-rpi5.sh")], cwd=REPO,
                   capture_output=True, text=True)
if r.returncode:
    print("FAIL rpi5_kernel_compiles")
    print(r.stdout[-3000:] + r.stderr[-1000:])
    sys.exit(1)
print("PASS rpi5_kernel_compiles")
