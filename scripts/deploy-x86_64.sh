#!/usr/bin/env bash
# Deploy the Leandros kernel to a USB drive for bare-metal x86-64 boot.
#
# Usage:
#   ./deploy-x86_64.sh <kernel.elf> <device>
#
# Example:
#   ./deploy-x86_64.sh target/x86_64-unknown-none/release/leandros /dev/sdb
#
# This script reuses the same FAT32 image built by run-x86_64.sh and writes
# it to the target USB device with dd.  The device is overwritten completely —
# all existing data will be lost.
#
# UEFI firmware will find EFI/BOOT/BOOTX64.EFI on the FAT32 partition and
# chain-load Limine → kernel.elf without any additional setup.
#
# Requirements (same as run-x86_64.sh):
#   sudo apt install dosfstools mtools

set -euo pipefail

KERNEL="${1:?Usage: deploy-x86_64.sh <kernel.elf> <device>}"
DEVICE="${2:?Usage: deploy-x86_64.sh <kernel.elf> <device>}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── Safety checks ─────────────────────────────────────────────────────────────

die() { echo "ERROR: $*" >&2; exit 1; }

[[ -f "$KERNEL" ]] || die "Kernel file not found: $KERNEL"
[[ -b "$DEVICE" ]] || die "Not a block device: $DEVICE"

# Refuse to write to a mounted device.
if grep -qs "^${DEVICE}" /proc/mounts; then
    die "$DEVICE is mounted. Unmount all its partitions first."
fi

# Warn and require confirmation.
echo "WARNING: This will overwrite ALL data on $DEVICE."
echo "Kernel: $KERNEL"
read -r -p "Type 'yes' to continue: " confirm
[[ "$confirm" == "yes" ]] || { echo "Aborted."; exit 0; }

# ── Build the FAT32 image ─────────────────────────────────────────────────────
#
# Delegate image creation to run-x86_64.sh up to the QEMU launch step.
# We call the image-building portion by re-sourcing its variables.

LIMINE_VERSION="11.3.1"
LIMINE_EFI_URL="https://raw.githubusercontent.com/limine-bootloader/limine/v${LIMINE_VERSION}-binary/BOOTX64.EFI"
TARGET_DIR="$REPO_ROOT/target"
LIMINE_CACHE="$TARGET_DIR/limine/$LIMINE_VERSION"
LIMINE_EFI="$LIMINE_CACHE/BOOTX64.EFI"
DISK="$TARGET_DIR/x86_64-disk.img"
DISK_SIZE_MB=64

require_cmd() {
    command -v "$1" &>/dev/null || die "'$1' not found — install: $2"
}
require_cmd mkfs.fat "sudo apt install dosfstools"
require_cmd mmd      "sudo apt install mtools"
require_cmd mcopy    "sudo apt install mtools"

# Fetch Limine if not cached.
if [[ ! -f "$LIMINE_EFI" ]]; then
    echo "[limine] Downloading Limine $LIMINE_VERSION BOOTX64.EFI..."
    mkdir -p "$LIMINE_CACHE"
    if command -v curl &>/dev/null; then
        curl -sSL --fail "$LIMINE_EFI_URL" -o "$LIMINE_EFI"
    elif command -v wget &>/dev/null; then
        wget -qO "$LIMINE_EFI" "$LIMINE_EFI_URL"
    else
        die "Neither curl nor wget found."
    fi
    [[ -f "$LIMINE_EFI" ]] || die "Failed to download BOOTX64.EFI"
fi

# Write limine.cfg.
LIMINE_CFG="$(mktemp)"
trap 'rm -f "$LIMINE_CFG"' EXIT

cat > "$LIMINE_CFG" <<'EOF'
timeout: 0

/Leandros
    protocol: limine
    path: boot():/kernel.elf
    kaslr: no
EOF

# Build FAT32 image.
echo "[disk] Building $DISK_SIZE_MB MiB FAT32 disk image..."
dd if=/dev/zero of="$DISK" bs=1M count="$DISK_SIZE_MB" status=none
mkfs.fat -F 32 -n LEANDROS "$DISK" >/dev/null

mmd   -i "$DISK" ::/EFI
mmd   -i "$DISK" ::/EFI/BOOT
mcopy -oi "$DISK" "$LIMINE_EFI"    ::/EFI/BOOT/BOOTX64.EFI
mcopy -oi "$DISK" "$LIMINE_CFG"    ::/limine.cfg
mcopy -oi "$DISK" "$KERNEL"        ::/kernel.elf

# ── Write image to USB device ─────────────────────────────────────────────────

echo "[deploy] Writing $DISK → $DEVICE ..."
dd if="$DISK" of="$DEVICE" bs=4M conv=fsync status=progress
sync

echo "[deploy] Done. Insert $DEVICE into the target machine and boot."
