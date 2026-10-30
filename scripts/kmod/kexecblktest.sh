#!/usr/bin/env bash
# kexecblktest.sh [arch]: M6 gate (kmod W2-3). Builds CONFIG_MODULES=y CONFIG_VIRTIO_BLK=m (root is on
# virtio-blk, modules/BOOT lists virtio_blk), boots, kexecs into the same kernel + initrd
# (.claude/skills/run-leandros/kexecblktest.py) and requires the second kernel to load the module and
# mount root. Restores .config afterwards (the next build-all rebuilds the =y kernel).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
arch="${1:-aarch64}"
cd "$ROOT"
saved=""
if [[ -f .config ]]; then saved="$(mktemp)"; cp .config "$saved"; fi
restore() { if [[ -n "$saved" ]]; then cp "$saved" .config; rm -f "$saved"; else rm -f .config; fi; }
trap restore EXIT
printf 'CONFIG_MODULES=y\nCONFIG_VIRTIO_BLK=m\n' > .config
./scripts/build-all.sh --arch "$arch" > "${TMPDIR:-/tmp}/kexecblktest-build.log" 2>&1 || { tail -n 20 "${TMPDIR:-/tmp}/kexecblktest-build.log"; exit 1; }
export LEANDROS_RUN_ID="kexecblk-$arch"
python3 .claude/skills/run-leandros/runtests.py "$arch" kexecblk > "${TMPDIR:-/tmp}/kexecblktest-run.log" 2>&1 || true
L="/tmp/ffsession/tests-kexecblk"
if grep -aq "RESULT: PASS" "$L"/*.log "${TMPDIR:-/tmp}/kexecblktest-run.log" 2>/dev/null; then echo "kexecblktest $arch: PASS"; else
    tail -n 15 "${TMPDIR:-/tmp}/kexecblktest-run.log"; echo "kexecblktest $arch: FAIL"; exit 1; fi
