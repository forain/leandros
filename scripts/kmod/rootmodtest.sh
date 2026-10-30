#!/usr/bin/env bash
# rootmodtest.sh [arch] [nvme|usb_storage|sdhci]: root-on-module gate. Builds CONFIG_MODULES=y with the driver
# as =m, attaches the ROOT FILESYSTEM disk (LABEL=leandros-root, data0) to QEMU on that bus
# (LEANDROS_ROOTFS_BUS=nvme|usb), so the kernel can only mount root after the module (modules/BOOT) was
# loaded from the initrd. Requires the module load line and "/ on sdX/nvme0n1" in /proc/mounts.
# sdhci: QEMU -M raspi4b (aarch64 only; build-all --raspi4b, CONFIG_BOARD_RASPI4B=y): root on the SD card through the module.
# (The --rpi5 CONFIG_SDHCI=m build is compile-only: no QEMU model of the Pi 5 controller.)
# Restores .config afterwards (the next build-all rebuilds the =y kernel).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
arch="${1:-aarch64}"
mod="${2:-usb_storage}"
cd "$ROOT"
saved=""
if [[ -f .config ]]; then saved="$(mktemp)"; cp .config "$saved"; fi
restore() { if [[ -n "$saved" ]]; then cp "$saved" .config; rm -f "$saved"; else rm -f .config; fi; }
trap restore EXIT
case "$mod" in
    nvme) bus=nvme; dev='nvme0n1'; extra='nvme.rw=1'; board=''; modebits='' ;;   # NVMe is read-only by default; an f2fs root needs rw
    usb_storage) bus=usb; dev='sd[a-z]'; extra=''; board=''; modebits='' ;;
    sdhci) bus=virtio; dev='vd[a-z]'; extra=''; board='--raspi4b'; modebits='--mode raspi4b'; [[ "$arch" == aarch64 ]] || { echo 'sdhci: aarch64 only'; exit 2; } ;;
    *) echo "unknown module $mod"; exit 2 ;;
esac
sym=$(echo "$mod" | tr a-z A-Z)
printf 'CONFIG_MODULES=y\nCONFIG_%s=m\n' "$sym" > .config
[[ "$mod" == sdhci ]] && printf 'CONFIG_BOARD_RASPI4B=y\n' >> .config
./scripts/build-all.sh --arch "$arch" $board > "${TMPDIR:-/tmp}/rootmodtest-build.log" 2>&1 || { tail -n 20 "${TMPDIR:-/tmp}/rootmodtest-build.log"; exit 1; }
export LEANDROS_RUN_ID="rootmod-$arch-$mod" LEANDROS_ROOTFS_BUS="$bus"
tag="rootmod-$mod"
python3 .claude/skills/run-leandros/runtests.py "$arch" "$tag" $modebits ${extra:+--cmdline "$extra"} "cat /proc/modules" "cat /proc/mounts" > /dev/null 2>&1 || true
L="/tmp/ffsession/tests-$tag/serial-full.log"
fail=0
check() { if grep -aq -- "$2" "$L"; then echo "ok   $3"; else echo "FAIL $3"; fail=1; fi; }
check "x" "^\\[kmod\\] .*$mod loaded" "$mod loaded from the initrd"
check "x" "^\\[BLK\\] /dev/root -> $dev" "root resolved to $dev"
check "x" "pivot_root successful" "root pivoted"
check "x" "^$mod [0-9]* [0-9]* - Live" "listed in /proc/modules"
[[ $fail -eq 0 ]] && echo "rootmodtest $arch $mod: PASS" || { echo "rootmodtest $arch $mod: FAIL"; exit 1; }
