#!/usr/bin/env bash
# M19 falsification — put LIVE_BUCKETS back to 64 on aarch64 and see the symptoms
# return, with the KERNEL as the only delta.
#
# Everything else is held fixed by construction: the same staged busd binary,
# the same f2fs image regenerated from the same inputs before each run, the same
# harness, the same phase timings. scripts/m7z2-kernel-only.sh rebuilds the
# standard kernel and re-embeds it in leandros-limine-aarch64.img and touches
# neither userland, nor the initrd, nor the data image.
#
# The restore must be BYTE-IDENTICAL to the control, not merely "512 again" —
# a restore that only looks right leaves the tree in a state nobody measured.
set -euo pipefail
cd "$(dirname "$0")/.."
OUT="${1:-/tmp/m19-mutate}"
mkdir -p "$OUT"
PORT=ipc/src/port.rs

freshimg() {
  python3 scripts/mkfs-f2fs-populated.py f2fs-data0-aarch64.img aarch64 >/dev/null
  cp f2fs-data0-aarch64.img f2fs-data1-aarch64.img
  md5 -q f2fs-data0-aarch64.img
}

echo "=== control kernel (LIVE_BUCKETS as committed) ==="
grep -n 'const LIVE_BUCKETS' $PORT
./scripts/m7z2-kernel-only.sh aarch64 >"$OUT/build-control.log" 2>&1
CONTROL_MD5=$(md5 -q target/final-aarch64/kernel)
echo "CONTROL kernel md5 = $CONTROL_MD5"

echo "=== mutate to 64 ==="
sed -i '' 's/^const LIVE_BUCKETS: usize = 512;/const LIVE_BUCKETS: usize = 64;/' $PORT
grep -n 'const LIVE_BUCKETS' $PORT
./scripts/m7z2-kernel-only.sh aarch64 >"$OUT/build-mutant.log" 2>&1
MUTANT_MD5=$(md5 -q target/final-aarch64/kernel)
echo "MUTANT  kernel md5 = $MUTANT_MD5"
[ "$MUTANT_MD5" != "$CONTROL_MD5" ] || { echo "FATAL: mutation did not change the kernel"; exit 1; }

echo "=== fresh image, then the mutant session ==="
pkill -9 -f 'qemu-syste[m]' 2>/dev/null || true
sleep 2
IMG_MD5=$(freshimg)
echo "image md5 = $IMG_MD5"
# The mutant is EXPECTED to wedge, so the marker wait comes down from the
# default: the guest's own waits before the first marker total 240 s, and
# sitting out a 20-minute timeout twice proves nothing that 7 minutes does not.
M19_MARK_TIMEOUT=420 python3 -u artifacts/m19_a64.py "$OUT/session" aarch64 mutant \
  >"$OUT/mutant-harness.txt" 2>&1 || true
pkill -9 -f 'qemu-syste[m]' 2>/dev/null || true
sleep 2

echo "=== restore ==="
sed -i '' 's/^const LIVE_BUCKETS: usize = 64;/const LIVE_BUCKETS: usize = 512;/' $PORT
grep -n 'const LIVE_BUCKETS' $PORT
./scripts/m7z2-kernel-only.sh aarch64 >"$OUT/build-restore.log" 2>&1
RESTORE_MD5=$(md5 -q target/final-aarch64/kernel)
echo "RESTORE kernel md5 = $RESTORE_MD5"

{
  echo "control  $CONTROL_MD5"
  echo "mutant   $MUTANT_MD5"
  echo "restore  $RESTORE_MD5"
  echo "image    $IMG_MD5"
} > "$OUT/md5s.txt"
cat "$OUT/md5s.txt"
if [ "$RESTORE_MD5" = "$CONTROL_MD5" ]; then
  echo ">>> RESTORE IS BYTE-IDENTICAL TO THE CONTROL"
else
  echo ">>> RESTORE DIFFERS FROM THE CONTROL — the tree is not where it started"
  exit 1
fi
git diff --stat $PORT
