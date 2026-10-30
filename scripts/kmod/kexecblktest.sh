#!/usr/bin/env bash
# kexecblktest.sh [arch]: M6 gate SCAFFOLD (real in W2-3). Build with CONFIG_MODULES=y CONFIG_VIRTIO_BLK=m
# (root on virtio-blk, modules/BOOT lists virtio_blk), boot, run `kexec` into the same kernel via
# .claude/skills/run-leandros/kexecfdttest.py, and require the second kernel to mount root.
# Until W2-3 makes VIRTIO_BLK buildable as a module this only prints the plan.
set -euo pipefail
echo "SKIP kexecblktest: scaffold only (needs W2-3 CONFIG_VIRTIO_BLK=m); see artifacts/notes/lane-kmodbus-2026-10-29.md"
