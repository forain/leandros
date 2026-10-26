#!/usr/bin/env bash
# Guard for the aarch64 direct-boot early page tables (entry_aarch64.s): the
# GiB holding the kernel image must be mapped with 2 MiB blocks, so a DTB
# `no-map` range in that GiB (on a Pi 5: RAM GiB 0, firmware carve-outs) can
# be taken out of the HHDM. With a 1 GiB block the split is refused and the
# boot line reads `left 0x...3FD000 -> STILL MAPPED`.
#
# QEMU loads a bare ELF without its generated DTB (1 MiB, does not fit below
# the image at 0x40080000), so this dumps the virt DTB, packs it and passes it
# with -dtb plus the `mm.nomaptest=` knob on -append.
#
# Usage: scripts/test-direct-nomap.sh [tag]   (needs a built kernel-direct)
set -euo pipefail
cd "$(dirname "$0")/.."
tag="${1:-direct-nomap}"
mem="${LEANDROS_QEMU_MEM:-4G}"
range="0x50003000-0x50400000"   # GiB 1, beside the image (0x40080000..)
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
qemu-system-aarch64 -machine "virt,gic-version=3,dumpdtb=$tmp/raw.dtb" \
    -smp 4 -cpu max -m "$mem" -accel tcg -display none >/dev/null 2>&1
dtc -q -I dtb -O dtb -o "$tmp/virt.dtb" "$tmp/raw.dtb"
export LEANDROS_QEMU_MEM="$mem"
export LEANDROS_QEMU_EXTRA="-dtb $tmp/virt.dtb -append mm.nomaptest=$range"
export LEANDROS_RUN_ID="${LEANDROS_RUN_ID:-$tag-aarch64}"
python3 .claude/skills/run-leandros/runtests.py aarch64 "$tag" --mode direct /bin/true | tail -n 1
line="$(grep -a -m1 '\[early\] nomap:' "/tmp/ffsession/tests-$tag/serial-full.log" || true)"
echo "$line"
case "$line" in
    *" left 0x0000000000000000 -> unmapped"*) echo "direct-nomap: PASS" ;;
    *) echo "direct-nomap: FAIL"; exit 1 ;;
esac
