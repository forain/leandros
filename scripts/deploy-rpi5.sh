#!/usr/bin/env bash
# Deploy the Leandros kernel to a Raspberry Pi 5 SD card.
#
# Usage:
#   ./deploy-rpi5.sh <kernel.elf> <device>
#
# Example:
#   ./deploy-rpi5.sh target/aarch64-unknown-none/release/leandros /dev/mmcblk0
#
# The SD card must already have a FAT32 boot partition (partition 1).
# This script mounts partition 1, copies kernel.elf and config.txt, then unmounts.
# All existing RPi firmware files (start4.elf, fixup4.dat, etc.) are preserved.
#
# RPi 5 boot sequence:
#   SoC ROM → firmware (start4.elf) → config.txt → kernel.elf (bare ELF/bin)
#
# The kernel is loaded at the address specified by kernel_address in config.txt.
# Leandros entry point must be at that address (0x80000 by default for AArch64).
#
# Requirements:
#   sudo apt install mount util-linux   (usually pre-installed)
#
# First-time SD card setup (run once, not part of this script):
#   1. Flash Raspberry Pi OS Lite to the SD card.
#   2. OR: create a FAT32 partition 1 and copy the RPi 5 firmware files:
#        start4.elf, fixup4.dat, bcm2712-rpi-5-b.dtb
#      These can be obtained from: https://github.com/raspberrypi/firmware/tree/master/boot

set -euo pipefail

KERNEL="${1:?Usage: deploy-rpi5.sh <kernel.elf> <device>}"
DEVICE="${2:?Usage: deploy-rpi5.sh <kernel.elf> <device>}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── Helpers ───────────────────────────────────────────────────────────────────

die() { echo "ERROR: $*" >&2; exit 1; }

# ── Safety checks ─────────────────────────────────────────────────────────────

[[ -f "$KERNEL" ]] || die "Kernel file not found: $KERNEL"
[[ -b "$DEVICE" ]] || die "Not a block device: $DEVICE"

# Determine the boot partition device node.
# For /dev/sdX  → /dev/sdX1
# For /dev/mmcblkN → /dev/mmcblkNp1
if [[ "$DEVICE" =~ mmcblk[0-9]+$ ]]; then
    BOOT_PART="${DEVICE}p1"
else
    BOOT_PART="${DEVICE}1"
fi

[[ -b "$BOOT_PART" ]] || die "Boot partition not found: $BOOT_PART
  Make sure the SD card has a FAT32 partition 1 with RPi 5 firmware files."

# ── Mount and deploy ──────────────────────────────────────────────────────────

MOUNT_DIR="$(mktemp -d)"
trap 'umount "$MOUNT_DIR" 2>/dev/null; rmdir "$MOUNT_DIR"' EXIT

echo "[deploy] Mounting $BOOT_PART → $MOUNT_DIR"
mount "$BOOT_PART" "$MOUNT_DIR" || die "Failed to mount $BOOT_PART (try: sudo $0 $*)"

# Sanity check: make sure this looks like an RPi boot partition.
if [[ ! -f "$MOUNT_DIR/start4.elf" && ! -f "$MOUNT_DIR/start.elf" ]]; then
    echo "WARNING: RPi firmware (start4.elf / start.elf) not found on $BOOT_PART."
    echo "  This may not be an RPi boot partition."
    read -r -p "Continue anyway? [y/N] " confirm
    [[ "$confirm" =~ ^[Yy]$ ]] || { echo "Aborted."; exit 0; }
fi

# Copy the kernel ELF.
echo "[deploy] Copying kernel.elf..."
cp "$KERNEL" "$MOUNT_DIR/kernel.elf"

# Write config.txt (preserves any existing content, appends our settings
# only if they are not already present).
CONFIG="$MOUNT_DIR/config.txt"

# Ensure RPi 5 firmware will load our ELF and boot at EL2 → EL1.
# kernel=    : filename on the FAT32 partition to load as the kernel
# arm_64bit= : 1 = load in 64-bit (AArch64) mode
# kernel_address=: physical load address; 0x80000 is the standard AArch64 default

write_config_key() {
    local key="$1" value="$2"
    if grep -qs "^${key}=" "$CONFIG" 2>/dev/null; then
        # Key already exists — update it in-place.
        sed -i "s|^${key}=.*|${key}=${value}|" "$CONFIG"
    else
        echo "${key}=${value}" >> "$CONFIG"
    fi
}

echo "[deploy] Writing config.txt settings..."
touch "$CONFIG"
write_config_key "kernel"    "kernel.elf"
write_config_key "arm_64bit" "1"
# Note: kernel_address is NOT set here.  When kernel= points to an ELF file,
# the RPi firmware reads the load address from the PT_LOAD program headers.
# kernel_address only applies to flat binary (.img) images.

echo "[deploy] config.txt:"
cat "$CONFIG"

# Flush and unmount.
sync
umount "$MOUNT_DIR"
trap - EXIT
rmdir "$MOUNT_DIR"

echo "[deploy] Done. Insert the SD card into the Raspberry Pi 5 and power on."
