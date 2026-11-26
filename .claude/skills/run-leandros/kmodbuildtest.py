#!/usr/bin/env python3
"""kmodbuildtest - every loadable module still builds, links against the kernel's export table and validates.

Host-only (no boot): runs scripts/kmod/kmod-build.sh with KMOD_EXTRA=all for the arch under test, staging into
target/kmod-check/<arch> so the images of the tree are untouched. The drivers are compiled twice, once into the
kernel and once into each module, and nothing else builds the module copy of a driver when the kernel is all-Y:
kapi drift (an irq/mm/sched/kdiag path the kmod-kapi shim lacks, a kapi_* symbol the kernel does not export) shows
up as a red module build here, with the failing module named.
Needs a built kernel (target/final-<arch>/{kernel,kernel-direct}); a regress tree has one from its own build.
aarch64 also builds sdhci for the Pi 4 board (KMOD_BOARD=raspi4b); the all-modules run uses rpi5.
Usage: kmodbuildtest.py [arch]   (default $LEANDROS_TEST_ARCH; REPO=<checkout> for another tree)
"""
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.environ.get("REPO", os.path.abspath(os.path.join(HERE, "../../..")))
arch = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("LEANDROS_TEST_ARCH", "x86_64")
if not os.path.exists(os.path.join(REPO, "target", "final-" + arch, "kernel")):
    print(f"FAIL kmodbuild_{arch}: no target/final-{arch}/kernel (build the kernel first)")
    sys.exit(1)

runs = [("all", {"KMOD_EXTRA": "all"})]
if arch == "aarch64":
    runs.append(("sdhci_raspi4b", {"KMOD_EXTRA": "sdhci", "KMOD_BOARD": "raspi4b"}))
bad = 0
for name, extra in runs:
    env = dict(os.environ, KMOD_STRICT="1", KMOD_OUT=os.path.join(REPO, "target", "kmod-check", arch), **extra)
    r = subprocess.run([os.path.join(REPO, "scripts/kmod/kmod-build.sh"), arch], cwd=REPO, env=env,
                       capture_output=True, text=True)
    if r.returncode:
        bad += 1
        print(f"FAIL kmodbuild_{arch}_{name}")
        print((r.stderr or "")[-3000:] + (r.stdout or "")[-500:])
    else:
        print(f"PASS kmodbuild_{arch}_{name}: " + r.stdout.strip().splitlines()[-1])
sys.exit(1 if bad else 0)
