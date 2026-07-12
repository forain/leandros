#!/usr/bin/env bash
# Partition, format, and populate a blank SD card for Raspberry Pi 5 boot.
#
# Unlike deploy-rpi5.sh (which only *updates* kernel.elf on an SD card that
# already has a FAT32 boot partition), this script prepares a card from
# scratch: it wipes the device, creates a single MBR/FAT32 partition (the
# layout the Pi 5's boot ROM expects), and populates it for one of two boot
# paths, selected with --boot-mode:
#
#   direct  - RPi firmware loads kernel.elf (the direct-boot ELF) straight,
#             no bootloader in between. Simplest, fewest moving parts.
#   limine  - RPi firmware loads the vendored RPI_EFI.fd as its "armstub",
#             which brings up UEFI; UEFI then boots Limine (BOOTAA64.EFI)
#             from the ESP, which reads limine.conf and loads kernel.elf +
#             the initrd. Needed for Limine-specific features.
#
# Usage:
#   sudo ./scripts/prepare-rpi5-sdcard.sh --boot-mode <limine|direct> <device>
#
# Examples:
#   sudo ./scripts/prepare-rpi5-sdcard.sh --boot-mode direct /dev/mmcblk0   # Linux
#   sudo ./scripts/prepare-rpi5-sdcard.sh --boot-mode limine /dev/disk4    # macOS
#
# Requires target/final-aarch64/{kernel,kernel-direct} and initrd-aarch64.cpio,
# i.e. a prior run of:
#   ./scripts/build-all.sh --arch aarch64 --rpi5
#
# Linux requirements: parted, mkfs.vfat (dosfstools), mount/umount, lsblk, curl
# macOS requirements: diskutil (built-in), curl
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

FW_CACHE_DIR="$REPO_ROOT/.rpi-firmware-cache"
LIMINE_CACHE_DIR="$REPO_ROOT/.limine-cache"
LIMINE_VERSION="11.4.1"     # keep in sync with build-all.sh; must stay >= 6 (CLAUDE.md)
FW_BASE_URL="https://raw.githubusercontent.com/raspberrypi/firmware/master/boot"

die() { echo "ERROR: $*" >&2; exit 1; }
info() { echo "[prepare-sdcard] $*"; }

# ── Argument parsing ─────────────────────────────────────────────────────────

BOOT_MODE=""
DEVICE=""
FIRMWARE_DIR=""
ASSUME_YES="false"
ORIG_ARGS=("$@")

show_usage() {
    sed -n '2,25p' "${BASH_SOURCE[0]}"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --boot-mode) BOOT_MODE="$2"; shift 2 ;;
        --firmware-dir) FIRMWARE_DIR="$2"; shift 2 ;;
        -y|--yes) ASSUME_YES="true"; shift ;;
        -h|--help) show_usage; exit 0 ;;
        -*) die "Unknown option: $1" ;;
        *) DEVICE="$1"; shift ;;
    esac
done

[[ "$BOOT_MODE" == "limine" || "$BOOT_MODE" == "direct" ]] || die "Must pass --boot-mode limine|direct"
[[ -n "$DEVICE" ]] || { show_usage; die "Missing <device> argument"; }
[[ "$(id -u)" == "0" ]] || die "Must run as root (try: sudo $0 ${ORIG_ARGS[*]})"

OS="$(uname -s)"
case "$OS" in
    Linux) IS_MACOS="false" ;;
    Darwin) IS_MACOS="true" ;;
    *) die "Unsupported host OS: $OS" ;;
esac

# ── Required build artifacts ──────────────────────────────────────────────────

KERNEL_STD="$REPO_ROOT/target/final-aarch64/kernel"
KERNEL_DIRECT="$REPO_ROOT/target/final-aarch64/kernel-direct"
INITRD="$REPO_ROOT/initrd-aarch64.cpio"
UEFI_DIR="$REPO_ROOT/target/rpi5-uefi"

for f in "$KERNEL_STD" "$KERNEL_DIRECT" "$INITRD"; do
    [[ -f "$f" ]] || die "Missing build artifact: $f
  Run: ./scripts/build-all.sh --arch aarch64 --rpi5"
done
if [[ "$BOOT_MODE" == "limine" ]]; then
    for f in "$UEFI_DIR/RPI_EFI.fd" "$UEFI_DIR/bcm2712-rpi-5-b.dtb" "$UEFI_DIR/config.txt"; do
        [[ -f "$f" ]] || die "Missing vendored RPi 5 UEFI firmware file: $f"
    done
fi

# ── Device validation & safety guard ─────────────────────────────────────────

if [[ "$IS_MACOS" == "true" ]]; then
    diskutil info "$DEVICE" >/dev/null 2>&1 || die "Not a recognizable disk: $DEVICE"
    IS_WHOLE="$(diskutil info "$DEVICE" | awk -F': +' '/^ *Whole:/ {print $2}')"
    [[ "$IS_WHOLE" == "Yes" ]] || die "$DEVICE is a partition/slice, not a whole disk. Pass e.g. /dev/disk4, not /dev/disk4s1."

    ROOT_DISK_ID="$(diskutil info / | awk -F': +' '/Part of Whole:/ {print $2}')"
    TARGET_DISK_ID="$(basename "$DEVICE")"
    [[ "$TARGET_DISK_ID" != "$ROOT_DISK_ID" ]] || die "Refusing to touch $DEVICE: it backs the running system's / volume."
else
    [[ -b "$DEVICE" ]] || die "Not a block device: $DEVICE"
    DEV_TYPE="$(lsblk -no TYPE "$DEVICE" 2>/dev/null | head -1)"
    [[ "$DEV_TYPE" == "disk" ]] || die "$DEVICE is not a whole disk (type=$DEV_TYPE). Pass the base device, e.g. /dev/mmcblk0, not a partition."

    ROOT_SRC="$(findmnt -no SOURCE / 2>/dev/null || true)"
    ROOT_DISK=""
    [[ -n "$ROOT_SRC" ]] && ROOT_DISK="/dev/$(lsblk -no pkname "$ROOT_SRC" 2>/dev/null || true)"
    [[ -z "$ROOT_DISK" || "$ROOT_DISK" != "$DEVICE" ]] || die "Refusing to touch $DEVICE: it backs the running system's / filesystem."
fi

# ── Confirm with the user (this is destructive) ──────────────────────────────

echo "About to WIPE and repartition: $DEVICE"
echo "Boot mode: $BOOT_MODE"
if [[ "$IS_MACOS" == "true" ]]; then
    diskutil list "$DEVICE"
else
    lsblk "$DEVICE"
fi

if [[ "$ASSUME_YES" != "true" ]]; then
    read -r -p "Type the device path ($DEVICE) again to confirm: " confirm
    [[ "$confirm" == "$DEVICE" ]] || die "Confirmation did not match. Aborted."
fi

# ── Partition + format ────────────────────────────────────────────────────────
# Two MBR partitions (MBR, not GPT, matches what Raspberry Pi OS / rpi-imager
# ships and is the most broadly-tested layout for the Pi boot ROM's
# first-stage FAT scan):
#   1. FAT32 LBA (type 0x0c), 512 MiB, boot partition the firmware scans.
#   2. Type 0x83 ("Linux" — F2FS has no dedicated MBR type byte, matching how
#      a real Linux system would label it), rest of the disk, holds F2FS
#      root. drivers/src/sdhci.rs's find_f2fs_partition() parses the MBR at
#      boot to locate this rather than assuming a fixed offset.

BOOT_PART=""
ROOT_PART=""
MOUNT_DIR=""

# Patch partition 2's MBR type byte to 0x83. Needed on macOS specifically:
# diskutil has no "Linux"/F2FS format name, so partition 2 is created as a
# second FAT32 (type 0x0c, indistinguishable from partition 1) and then
# immediately overwritten with real F2FS content below — this makes that
# distinguishable to the kernel's MBR scan. Byte offset 446 is the MBR
# partition table; each entry is 16 bytes with the type byte at +4, so
# partition 2 (0-indexed entry 1) is at 446 + 16 + 4 = 466.
mark_root_partition_linux() {
    printf '\x83' | dd of="$DEVICE" bs=1 seek=466 count=1 conv=notrunc status=none
}

partition_and_format() {
    if [[ "$IS_MACOS" == "true" ]]; then
        diskutil unmountDisk "$DEVICE" >/dev/null 2>&1 || true
        info "Partitioning (MBR: 512MiB FAT32 boot + F2FS root)..."
        diskutil partitionDisk "$DEVICE" 2 MBR "MS-DOS FAT32" RPI5BOOT 512M "MS-DOS FAT32" RPI5ROOT R >/dev/null
        BOOT_PART="${DEVICE}s1"
        ROOT_PART="${DEVICE}s2"

        diskutil unmountDisk "$DEVICE" >/dev/null 2>&1 || true
        mark_root_partition_linux

        diskutil mount "$BOOT_PART" >/dev/null
        MOUNT_DIR="$(diskutil info "$BOOT_PART" | awk -F': +' '/Mount Point:/ {print $2}')"
        [[ -n "$MOUNT_DIR" ]] || die "Could not determine mount point for $BOOT_PART"
    else
        command -v parted >/dev/null || die "parted not found (try: apt install parted)"
        command -v mkfs.vfat >/dev/null || die "mkfs.vfat not found (try: apt install dosfstools)"

        # Unmount any currently-mounted partitions on this device first.
        for p in "${DEVICE}"*; do
            [[ "$p" == "$DEVICE" ]] && continue
            mountpoint -q "$p" 2>/dev/null && umount "$p" 2>/dev/null || true
        done
        wipefs -a "$DEVICE" >/dev/null 2>&1 || true

        info "Partitioning (MBR: 512MiB FAT32 boot + F2FS root)..."
        parted --script "$DEVICE" mklabel msdos \
            mkpart primary fat32 1MiB 513MiB set 1 boot on \
            mkpart primary 513MiB 100% set 2 type 0x83
        partprobe "$DEVICE" 2>/dev/null || true
        sleep 1

        if [[ "$DEVICE" =~ mmcblk[0-9]+$ ]]; then
            BOOT_PART="${DEVICE}p1"
            ROOT_PART="${DEVICE}p2"
        else
            BOOT_PART="${DEVICE}1"
            ROOT_PART="${DEVICE}2"
        fi
        [[ -b "$BOOT_PART" && -b "$ROOT_PART" ]] || die "Partitions did not appear: $BOOT_PART / $ROOT_PART"

        info "Formatting $BOOT_PART as FAT32..."
        mkfs.vfat -F 32 -n RPI5BOOT "$BOOT_PART" >/dev/null

        MOUNT_DIR="$(mktemp -d)"
        mount "$BOOT_PART" "$MOUNT_DIR"
    fi
}

unmount_boot_partition() {
    [[ -z "$MOUNT_DIR" ]] && return 0
    sync
    if [[ "$IS_MACOS" == "true" ]]; then
        diskutil eject "$DEVICE" >/dev/null 2>&1 || true
    else
        umount "$MOUNT_DIR" 2>/dev/null || true
        rmdir "$MOUNT_DIR" 2>/dev/null || true
    fi
}
trap unmount_boot_partition EXIT

partition_and_format
info "Mounted $BOOT_PART at $MOUNT_DIR"

# ── Populate the F2FS root partition ─────────────────────────────────────────
# Reuses the same "populated" F2FS image builder scripts/build-all.sh uses
# for the QEMU test disks (userland binaries baked in, f2fstest included).
# The image is auto-sized to its packed content (well under the partition,
# which is "rest of disk" — tens of GB on a real card); the unused tail of
# the partition is simply never addressed by the F2FS filesystem within it.
populate_f2fs_root() {
    info "Building populated F2FS root image (reuses userland build output)..."
    local tmp_img
    tmp_img="$(mktemp -t rpi5-f2fs-root)"
    ( cd "$REPO_ROOT" && python3 scripts/mkfs-f2fs-populated.py "$tmp_img" aarch64 ) \
        || die "mkfs-f2fs-populated.py failed. Run ./scripts/build-all.sh --arch aarch64 --rpi5 first."

    info "Writing F2FS root to $ROOT_PART ($(du -h "$tmp_img" | cut -f1))..."
    if [[ "$IS_MACOS" == "true" ]]; then
        diskutil unmount "$ROOT_PART" >/dev/null 2>&1 || true
        local raw_root="/dev/r$(basename "$ROOT_PART")"
        dd if="$tmp_img" of="$raw_root" bs=4m status=progress
    else
        umount "$ROOT_PART" 2>/dev/null || true
        dd if="$tmp_img" of="$ROOT_PART" bs=4M status=progress conv=fsync
    fi
    rm -f "$tmp_img"
}

populate_f2fs_root

# ── Fetch the RPi 5 GPU firmware blobs (start4.elf, fixup4.dat) ─────────────
# These are Broadcom binary blobs redistributed by the Raspberry Pi Foundation;
# they are not vendored in this repo (see README's "Deploying to Raspberry Pi
# 5" section). Cache them locally so repeat runs don't re-download.

fetch_gpu_firmware() {
    local src_dir=""
    if [[ -n "$FIRMWARE_DIR" ]]; then
        src_dir="$FIRMWARE_DIR"
        [[ -f "$src_dir/start4.elf" && -f "$src_dir/fixup4.dat" ]] || \
            die "--firmware-dir $FIRMWARE_DIR is missing start4.elf and/or fixup4.dat"
    else
        mkdir -p "$FW_CACHE_DIR"
        if [[ ! -f "$FW_CACHE_DIR/start4.elf" || ! -f "$FW_CACHE_DIR/fixup4.dat" ]]; then
            info "Downloading RPi 5 GPU firmware (start4.elf, fixup4.dat)..."
            curl -fsSL -o "$FW_CACHE_DIR/start4.elf" "$FW_BASE_URL/start4.elf" || \
                die "Failed to download start4.elf from $FW_BASE_URL. Use --firmware-dir to supply it manually."
            curl -fsSL -o "$FW_CACHE_DIR/fixup4.dat" "$FW_BASE_URL/fixup4.dat" || \
                die "Failed to download fixup4.dat from $FW_BASE_URL. Use --firmware-dir to supply it manually."
        fi
        src_dir="$FW_CACHE_DIR"
    fi
    cp "$src_dir/start4.elf" "$MOUNT_DIR/start4.elf"
    cp "$src_dir/fixup4.dat" "$MOUNT_DIR/fixup4.dat"
}

fetch_gpu_firmware

# ── Populate for the selected boot mode ──────────────────────────────────────

if [[ "$BOOT_MODE" == "direct" ]]; then
    info "Copying direct-boot kernel + initrd..."
    cp "$UEFI_DIR/bcm2712-rpi-5-b.dtb" "$MOUNT_DIR/bcm2712-rpi-5-b.dtb"
    cp "$KERNEL_DIRECT" "$MOUNT_DIR/kernel.elf"
    cp "$INITRD" "$MOUNT_DIR/initrd.cpio"

    cat > "$MOUNT_DIR/config.txt" <<'EOF'
arm_64bit=1
kernel=kernel.elf
# 0x48000000 matches the fixed address the direct-boot kernel scans for the
# CPIO magic (see run-qemu.sh's -device loader,addr=0x48000000 for the QEMU
# equivalent of this same convention).
initramfs initrd.cpio 0x48000000
enable_uart=1
# Real hardware has no VirtIO GPU (that's QEMU-only) and no in-kernel
# Broadcom mailbox driver yet, so the kernel's only path to HDMI output is
# a framebuffer the *firmware* pre-allocates and publishes as a
# `framebuffer` DTB node (simple-framebuffer binding: reg/width/height/
# stride) — boot/src/device_tree.rs already parses that node, and
# kernel/src/main.rs already wires a non-zero framebuffer_base straight
# into the raw-pixel boot console (drivers/src/framebuffer.rs's fb_putc),
# independent of the VirtIO-GPU/DRM/KMS pipeline. These three directives
# are what make the firmware actually allocate and publish it.
framebuffer_width=1024
framebuffer_height=768
framebuffer_depth=32
hdmi_force_hotplug=1
EOF

else
    info "Copying UEFI firmware + Limine + kernel + initrd..."
    cp "$UEFI_DIR/RPI_EFI.fd" "$MOUNT_DIR/RPI_EFI.fd"
    cp "$UEFI_DIR/bcm2712-rpi-5-b.dtb" "$MOUNT_DIR/bcm2712-rpi-5-b.dtb"
    cp "$UEFI_DIR/config.txt" "$MOUNT_DIR/config.txt"

    # Reuse build-all.sh's Limine download/cache convention.
    LIMINE_DIR="$LIMINE_CACHE_DIR/limine-$LIMINE_VERSION-binary"
    if [[ ! -d "$LIMINE_DIR" ]]; then
        info "Downloading Limine $LIMINE_VERSION..."
        mkdir -p "$LIMINE_CACHE_DIR"
        MAJOR_VERSION="$(echo "$LIMINE_VERSION" | cut -d'.' -f1)"
        (
            cd "$LIMINE_CACHE_DIR"
            curl -fsSL -o "limine-$LIMINE_VERSION-binary.tar.gz" \
                "https://github.com/limine-bootloader/limine/archive/refs/heads/v${MAJOR_VERSION}.x-binary.tar.gz"
            tar -xzf "limine-$LIMINE_VERSION-binary.tar.gz"
            mv "Limine-${MAJOR_VERSION}.x-binary" "limine-$LIMINE_VERSION-binary"
            rm "limine-$LIMINE_VERSION-binary.tar.gz"
        )
    fi

    mkdir -p "$MOUNT_DIR/EFI/BOOT" "$MOUNT_DIR/boot/limine"
    cp "$LIMINE_DIR/BOOTAA64.EFI" "$MOUNT_DIR/EFI/BOOT/BOOTAA64.EFI"
    cp "$LIMINE_DIR/limine-bios.sys" "$MOUNT_DIR/boot/limine/limine-bios.sys"
    cp "$KERNEL_STD" "$MOUNT_DIR/kernel.elf"
    cp "$INITRD" "$MOUNT_DIR/initrd.gz"     # uncompressed; our loader doesn't handle .gz (see build-all.sh)
    cp "$REPO_ROOT/limine/limine.conf" "$MOUNT_DIR/limine.conf"
fi

info "Done. Contents of $BOOT_PART:"
find "$MOUNT_DIR" -maxdepth 2 -type f | sed "s|$MOUNT_DIR|  |"

echo
echo "[prepare-sdcard] Unmounting and ejecting..."
unmount_boot_partition
trap - EXIT
MOUNT_DIR=""

echo "[prepare-sdcard] Safe to remove. Insert into the Raspberry Pi 5 and power on ($BOOT_MODE boot)."
