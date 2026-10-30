#!/bin/sh
# check-dualboot-fixture.sh <img> [--installed] [--boot <x86_64|aarch64>]
#
# Mode 1 (default, "pristine"): GPT, e2fsck -fn, ext4 feature bits, sha256 of
# the test data, swap hash, ESP fsck + listing, original ESP file hashes.
# --installed: additionally verifies the LeandrOS side (p4-p6 in the gap with
# no-auto attrs, p2 shrunk but intact, /EFI/leandros, limine.conf paths and
# root=PARTUUID, /loader/entries/leandros.conf).
# --boot <arch>: boots a cp -c clone in QEMU through systemd-boot into the
# Linux kernel and asserts LINUX-DUALBOOT-OK (serial only), no screenshots.
set -eu
IMG=${1:?usage: $0 <img> [--installed] [--boot <arch>]}; shift
HERE=$(cd "$(dirname "$0")" && pwd)
MODE=pristine; BOOT=""; FORCE_KERNEL=""
while [ $# -gt 0 ]; do case "$1" in
  --installed) MODE=installed ;;
  --boot) BOOT=$2; shift ;;
  *) echo "unknown arg $1" >&2; exit 2 ;; esac; shift; done
IMG=$(cd "$(dirname "$IMG")" && pwd)/$(basename "$IMG")
RC=0
if [ -z "$BOOT" ] || [ "$MODE" = installed ]; then
  . "$HERE/container-lib.sh"; leandros_pick_container || { echo "no container: $CT_WHY" >&2; exit 1; }
  $CT run --rm -e MODE=$MODE -v "$HERE/dualboot-fixture:/scripts:ro" \
    -v "$IMG:/img:ro" $(for s in meta manifest esp.sha256; do [ -f "$IMG.$s" ] && printf -- '-v %s:/img.%s:ro ' "$IMG.$s" "$s"; done) \
    alpine:3.21 sh /scripts/check-inner.sh || RC=$?
fi
if [ -n "$BOOT" ]; then
  case "$BOOT" in
    x86_64)  FW=/opt/homebrew/share/qemu/edk2-x86_64-code.fd
             MACH="-machine q35 -cpu max -accel tcg" Q=qemu-system-x86_64 ;;
    aarch64) FW=/opt/homebrew/share/qemu/edk2-aarch64-code.fd
             MACH="-machine virt -cpu max -accel hvf" Q=qemu-system-aarch64 ;;
    *) echo "bad arch" >&2; exit 2 ;;
  esac
  [ "$BOOT" = aarch64 ] && [ "$(uname -m)" != arm64 ] && MACH="-machine virt -cpu max -accel tcg"
  T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
  cp -c "$IMG" "$T/disk.img"
  VARS=$T/vars.fd
  if [ "$BOOT" = x86_64 ]; then cp /opt/homebrew/share/qemu/edk2-i386-vars.fd "$VARS"; else dd if=/dev/zero of="$VARS" bs=1m count=64 2>/dev/null; fi
  DISKDEV="${FX_DISKDEV:-virtio-blk-pci}"
  echo "== QEMU boot ($BOOT, $DISKDEV, timeout ${FX_BOOT_TIMEOUT:-240}s)"
  $Q $MACH -m 2048 -nographic -serial stdio -monitor none -no-reboot \
    -drive if=pflash,format=raw,readonly=on,file=$FW \
    -drive if=pflash,format=raw,file=$VARS \
    -drive file="$T/disk.img",format=raw,if=none,id=d0 -device $DISKDEV,drive=d0,serial=fx0 \
    > "$T/serial.log" 2>&1 &
  QP=$!; i=0
  while kill -0 $QP 2>/dev/null && [ $i -lt ${FX_BOOT_TIMEOUT:-240} ]; do
    grep -q 'LINUX-DUALBOOT-\(OK\|FAIL\)' "$T/serial.log" && break; sleep 1; i=$((i+1)); done
  kill $QP 2>/dev/null || true; wait $QP 2>/dev/null || true
  tr -d '\r' < "$T/serial.log" | grep -E 'fixture-init|LINUX-DUALBOOT|Linux version|systemd-boot' | head -8 | cut -c1-160
  if tr -d '\r' < "$T/serial.log" | grep -q 'LINUX-DUALBOOT-OK.* bad=0'; then echo "PASS boot $BOOT"
  else echo "FAIL boot $BOOT (serial tail follows)"; tail -15 "$T/serial.log" | cut -c1-160; RC=1; fi
fi
exit $RC
