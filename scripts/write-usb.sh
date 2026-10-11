#!/usr/bin/env bash
# Write a bootable LeandrOS USB flash drive (or a raw image of one): Limine,
# the kernel + initrd, and the read-write f2fs root filesystem.
#
# Usage:
#   ./scripts/write-usb.sh <arch> <device> [--yes] [--gpt] [--no-cmdline] [--full]
#   ./scripts/write-usb.sh <arch> --image <out.img> [--gpt] [--no-cmdline] [--sparse]
#
#   <arch>    x86_64   Limine UEFI (BOOTX64.EFI) + legacy BIOS stage
#             aarch64  Limine UEFI (BOOTAA64.EFI); there is no BIOS on aarch64
#   <device>  macOS: /dev/diskN (see `diskutil list external`)
#             Linux: /dev/sdX   (see `lsblk -o NAME,SIZE,TRAN,RM,MODEL`)
#
# Options:
#   --yes     skip the interactive confirmation (device mode only)
#   --gpt     GPT instead of the default MBR (see below)
#   --full    device mode: dd the whole image, zeros included (the old
#             behaviour). The default is a sparse flash, see below.
#   --sparse  --image mode: if <out.img> already exists, keep it and write
#             onto it sparsely (what a device gets) instead of replacing it.
#   --no-cmdline  do not add `cmdline: root=PARTUUID=...` to the stick's
#             limine.conf; the kernel then picks the root by itself (the
#             unique removable f2fs volume, tie-broken by the boot disk)
#
# Environment:
#   LEANDROS_USB_EXTRA_CMDLINE  extra kernel arguments appended to the stick's
#             cmdline (e.g. `klog.testpanic` to test the panic log path)
#             Slow-stick emulation for QEMU (lane fsio/readfirst), all off by
#             default: usbms.delay_us=N adds N us per read command,
#             usbms.wdelay_us=N per write command; the allocation-unit merge
#             model usbms.au_kib=K usbms.au_open=N usbms.merge_us=M charges M us
#             for a write outside the N most recently written K KiB units and
#             usbms.au_hit_us (default 4000) inside them. The PC stick's
#             bimodal writes (nvrm-boot16) are close to
#             "usbms.delay_us=1500 usbms.au_kib=4096 usbms.au_open=2 usbms.merge_us=1000000"
#
# Inputs (all produced by ./scripts/build-all.sh — run it first):
#   target/final-<arch>/kernel, initrd-<arch>.cpio, f2fs-data0-<arch>.img,
#   .limine-cache/limine-<ver>-binary/, limine/limine.conf
#
# Layout (1 MiB aligned; MBR is the same scheme as prepare-rpi5-sdcard.sh, so
# the root partition is found the same way: the first type-0x83 entry):
#
#   MBR (default)                         GPT (--gpt)
#   LBA 0   MBR + Limine BIOS stage 1     LBA 0    protective MBR (+ BIOS stage 1)
#                                         LBA 1-33 primary header + entries
#   p1  1 MiB    512 MiB 0xEF, active     p1  1 MiB    512 MiB  EFI System
#                FAT32 ESP "LEANDROS"                           (C12A7328-...)
#   p2  513 MiB  rest, type 0x83 f2fs     p2  513 MiB  rest, Linux filesystem
#                                                      data (0FC63DAF-...), f2fs
#                                         backup entries + header at the very end
#
#   x86_64 --gpt adds p3, a ~1 MiB BIOS boot partition (21686148-...) in the gap
#   before p1: Limine's BIOS installer refuses GPT without one. It is listed last
#   in the table so the ESP and root stay partitions 1 and 2.
#
#   The ESP holds EFI/BOOT/BOOT{X64,AA64}.EFI, limine.conf, kernel.elf and
#   initrd.gz (uncompressed cpio, as build-all); x86_64 adds limine-bios.sys.
#
#   The stick's copy of limine.conf (not the repo file) gets
#   `cmdline: root=PARTUUID=<p2>` in the LeandrOS entry, in the form the
#   kernel's /dev/root resolution parses: MBR `<disk signature, 8 hex>-02`,
#   GPT the root partition's unique GUID. The disk signature / GUIDs are fresh
#   random values per stick, so two sticks never claim the same root.
#
# Log partition (last in the table, partition 3, or 4 on x86_64 --gpt): 64 MiB
# of raw space at the very end of the used area, no filesystem. MBR type 0xDA
# ("non-FS data"); GPT type GUID 042B7470-1302-4E61-9347-A0130B276754 (named
# "LEANDROS-LOG"). The kernel finds it by TYPE on the disk it booted from and
# keeps its console log there (4 slots, one per boot, see kernel/src/klog.rs);
# `scripts/read-usb-log.sh` reads it back on the host. Placement:
#   device: p2 is shrunk so that the log partition ends where the device ends
#           (before the GPT backup table); p2 never spans into it. On a stick
#           over 2 TiB the log sits right after the 2 TiB-capped p2 instead.
#   image:  the log partition follows the f2fs image, 1 MiB aligned.
# It is zeroed when written (a device gets 64 MiB of zeros there).
#
# Sparse flash (default for a device): cheap flash treats every block it was
# ever written as live, so dd'ing the zeros of a mostly-empty image (the 512 MiB
# ESP, the f2fs free tail, the log partition) leaves the controller no erased
# blocks and later random writes crawl. scripts/sparse-write.py writes only the
# non-zero 4 KiB blocks (runs, gaps under 1 MiB bridged) plus the ranges that
# must be written even when zero because they are read back:
#   - the first MiB (MBR/GPT; zeros erase a stale GPT) and the last 34 sectors
#   - the ESP's reserved area, both FATs and every allocated cluster
#   - f2fs everything below main_blkaddr (SB, checkpoints, SIT, NAT, SSA) and
#     each main block the image's SIT marks valid
#   - the 4 KiB header page of each log slot (zeroed; the rest of the log
#     partition keeps stale bytes, harmless: slots are found by magic + CRC)
# Skipped: the ESP free clusters, the f2fs free main area (the server zeroes
# each data block it allocates and writes node blocks whole), the rest of p2.
#
# The f2fs image is smaller than p2; the unused tail is ignored, exactly as on
# the Pi SD card. There is no second (/data) volume — init's fstab mount of
# /dev/vdc just warns.
#
# The whole layout is assembled into a sparse image file first (mtools + dd,
# no mounting, no sudo for that part), then copied to the device in one pass.
# Only the copy to a raw device needs root.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT_DIR"

ARCH=""
LIMINE_CONF="limine/limine.conf"
LIMINE_VERSION="$(sed -n 's/^DEFAULT_LIMINE_VERSION="\(.*\)"/\1/p' scripts/build-all.sh)"
LIMINE_DIR=".limine-cache/limine-$LIMINE_VERSION-binary"

SECTOR=512
ESP_START=2048                       # LBA, 1 MiB
ESP_SECTORS=$((512 * 1024 * 2))      # 512 MiB
ROOT_START=$((ESP_START + ESP_SECTORS))
LOG_SECTORS=$((64 * 1024 * 2))       # 64 MiB raw log partition

die() { echo "ERROR: $*" >&2; exit 1; }

usage() {
    sed -n '2,/^# Inputs/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
    exit "${1:-1}"
}

# ── Arguments ─────────────────────────────────────────────────────────────────

IMAGE_OUT=""
DEVICE=""
ASSUME_YES=0
GPT=0
CMDLINE=1
SPARSE=""                            # "", 1 or 0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --image) IMAGE_OUT="${2:?--image needs a path}"; shift 2 ;;
        --yes)   ASSUME_YES=1; shift ;;
        --gpt)   GPT=1; shift ;;
        --no-cmdline) CMDLINE=0; shift ;;
        --sparse) SPARSE=1; shift ;;
        --full)   SPARSE=0; shift ;;
        -h|--help) usage 0 ;;
        -*) die "unknown option: $1" ;;
        x86_64|aarch64) [[ -z "$ARCH" ]] || die "more than one arch given"; ARCH="$1"; shift ;;
        *)  [[ -z "$DEVICE" ]] || die "more than one device given"; DEVICE="$1"; shift ;;
    esac
done
[[ -n "$ARCH" ]] || usage
[[ -n "$IMAGE_OUT" || -n "$DEVICE" ]] || usage
[[ -z "$IMAGE_OUT" || -z "$DEVICE" ]] || die "give either a device or --image, not both"
# Sparse is the default for a device; for --image only when asked and only
# onto a file that already exists (a fresh file is already a sparse file).
SPARSE_ONTO=""
if [[ -n "$DEVICE" ]]; then
    [[ "$SPARSE" == 0 ]] || SPARSE=1
elif [[ "$SPARSE" == 1 && -e "$IMAGE_OUT" ]]; then
    SPARSE_ONTO="$IMAGE_OUT"
fi

KERNEL="target/final-$ARCH/kernel"
INITRD="initrd-$ARCH.cpio"
F2FS_IMG="f2fs-data0-$ARCH.img"
if [[ "$ARCH" == x86_64 ]]; then EFI_NAME=BOOTX64.EFI; else EFI_NAME=BOOTAA64.EFI; fi

# ── Inputs and tools ──────────────────────────────────────────────────────────

[[ -n "$LIMINE_VERSION" ]] || die "could not read DEFAULT_LIMINE_VERSION from scripts/build-all.sh"
for f in "$KERNEL" "$INITRD" "$F2FS_IMG" "$LIMINE_CONF" \
         "$LIMINE_DIR/$EFI_NAME"; do
    [[ -f "$f" ]] || die "missing $f — run ./scripts/build-all.sh first"
done

# Refuse stale pieces: the root image must not predate the kernel it boots
# with, or the stick ships a userland built against an older ABI.
if [[ "$F2FS_IMG" -ot "$KERNEL" ]]; then
    die "$F2FS_IMG is older than $KERNEL — rerun ./scripts/build-all.sh"
fi

if [[ "$ARCH" == x86_64 ]]; then
    [[ -f "$LIMINE_DIR/limine-bios.sys" ]] || die "missing $LIMINE_DIR/limine-bios.sys — run ./scripts/build-all.sh first"
fi

for t in mkfs.fat mmd mcopy python3 dd; do
    command -v "$t" >/dev/null || die "'$t' not found (macOS: brew install dosfstools mtools; Linux: apt install dosfstools mtools)"
done

# The binary release ships limine.c, not a host binary. build-all.sh calls
# "$LIMINE_DIR/limine" with `|| true`, so on a host without one its images
# silently lack the BIOS stage — here BIOS boot is a goal, so build it.
LIMINE_TOOL="$LIMINE_DIR/limine"
if [[ "$ARCH" == x86_64 && ! -x "$LIMINE_TOOL" ]]; then
    echo "🔧 Building the limine host tool..."
    cc -O2 -std=c99 -o "$LIMINE_TOOL" "$LIMINE_DIR/limine.c" \
        || die "could not build $LIMINE_TOOL"
fi

# ── Device checks (before any work, so a typo fails fast) ─────────────────────

OS="$(uname -s)"
DEV_BYTES=0
RAW_DEVICE=""
if [[ -n "$DEVICE" ]]; then
    [[ -b "$DEVICE" ]] || die "not a block device: $DEVICE"
    case "$OS" in
        Darwin)
            [[ "$DEVICE" =~ ^/dev/disk[0-9]+$ ]] || die "use the whole disk (/dev/diskN), not a slice"
            info="$(diskutil info "$DEVICE")" || die "diskutil cannot see $DEVICE"
            grep -Eq 'Device Location: +External|Removable Media: +(Removable|Yes)' <<<"$info" \
                || die "$DEVICE is not external/removable — refusing"
            grep -Eq 'Internal: +Yes' <<<"$info" && die "$DEVICE is an internal disk — refusing"
            DEV_BYTES="$(diskutil info -plist "$DEVICE" | plutil -extract TotalSize raw -)"
            MODEL="$(sed -n 's/^ *Device \/ Media Name: *//p' <<<"$info")"
            RAW_DEVICE="${DEVICE/\/dev\/disk//dev/rdisk}"
            ;;
        Linux)
            [[ "$(lsblk -dno TYPE "$DEVICE")" == disk ]] || die "use the whole disk (/dev/sdX), not a partition"
            tran="$(lsblk -dno TRAN "$DEVICE" | tr -d ' ')"
            rm_flag="$(lsblk -dno RM "$DEVICE" | tr -d ' ')"
            [[ "$tran" == usb || "$rm_flag" == 1 ]] || die "$DEVICE is not USB/removable (TRAN=$tran RM=$rm_flag) — refusing"
            lsblk -no MOUNTPOINT "$DEVICE" | grep -qx '/' \
                && die "$DEVICE holds the running system — refusing"
            DEV_BYTES="$(lsblk -bdno SIZE "$DEVICE")"
            MODEL="$(lsblk -dno MODEL "$DEVICE")"
            RAW_DEVICE="$DEVICE"
            ;;
        *) die "unsupported host OS: $OS" ;;
    esac
fi

F2FS_BYTES="$(wc -c <"$F2FS_IMG" | tr -d ' ')"
F2FS_SECTORS=$(( (F2FS_BYTES + SECTOR - 1) / SECTOR ))
ALIGN=2048                           # sectors, 1 MiB
F2FS_ALIGNED=$(( (F2FS_SECTORS + ALIGN - 1) / ALIGN * ALIGN ))
NEEDED_BYTES=$(( (ROOT_START + F2FS_SECTORS) * SECTOR ))   # what the image file holds
BIOS_BOOT=0                          # GPT + x86_64: a BIOS boot partition is needed
if (( GPT )) && [[ "$ARCH" == x86_64 ]]; then BIOS_BOOT=1; fi
GPT_TAIL=33                          # sectors: backup entries (32) + header (1)

if [[ -n "$DEVICE" ]]; then
    (( DEV_BYTES >= NEEDED_BYTES + LOG_SECTORS * SECTOR )) \
        || die "$DEVICE is $((DEV_BYTES >> 20)) MiB; need at least $(( (NEEDED_BYTES >> 20) + LOG_SECTORS * SECTOR / 1048576 )) MiB (incl. the log partition)"
    # p2 spans to the start of the log partition, which ends at the end of the
    # stick (MBR caps a partition at 2 TiB; GPT leaves room for the backup
    # table at the end).
    TOTAL_SECTORS=$(( DEV_BYTES / SECTOR ))
    if (( GPT )); then
        END=$(( TOTAL_SECTORS - GPT_TAIL ))
    else
        END=$TOTAL_SECTORS
    fi
    LOG_START=$(( (END - LOG_SECTORS) / ALIGN * ALIGN ))
    # MBR entries are 32-bit: on a stick over 2 TiB the log sits below that
    # limit (p2 ends there), not at the very end.
    if (( ! GPT && LOG_START + LOG_SECTORS > 0xFFFFFFFF )); then
        LOG_START=$(( (0xFFFFFFFF - LOG_SECTORS) / ALIGN * ALIGN ))
    fi
    ROOT_SECTORS=$(( LOG_START - ROOT_START ))
else
    ROOT_SECTORS=$F2FS_ALIGNED
    LOG_START=$(( ROOT_START + ROOT_SECTORS ))
    # A GPT image carries its backup table inside the file: pad one MiB.
    TOTAL_SECTORS=$(( LOG_START + LOG_SECTORS ))
    if (( GPT )); then TOTAL_SECTORS=$(( TOTAL_SECTORS + 2048 )); fi
fi

# ── Assemble the image ────────────────────────────────────────────────────────

WORK="$(mktemp -d "${TMPDIR:-/tmp}/leandros-usb.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
IMG="${IMAGE_OUT:-$WORK/usb.img}"
[[ -z "$SPARSE_ONTO" ]] || IMG="$WORK/usb.img"
GPT_BACKUP="$WORK/gpt-backup.bin"
ESP_IMG="$WORK/esp.img"
PARTUUID_FILE="$WORK/partuuid"

echo "💽 Assembling image ($((NEEDED_BYTES >> 20)) MiB, root partition $((ROOT_SECTORS * SECTOR >> 20)) MiB, log partition $((LOG_SECTORS * SECTOR >> 20)) MiB at LBA $LOG_START)..."
rm -f "$IMG"
# Sparse: only the ESP and f2fs ranges get real blocks.
IMG_BYTES="$NEEDED_BYTES"
# An image (not a device) must contain the whole disk: the log partition and,
# on GPT, the backup table.
if [[ -n "$IMAGE_OUT" ]]; then IMG_BYTES=$(( TOTAL_SECTORS * SECTOR )); fi
dd if=/dev/zero of="$IMG" bs=1 count=0 seek="$IMG_BYTES" 2>/dev/null

# Partition table. The 446-byte boot-code area is left zeroed for
# `limine bios-install`, which preserves the table and the disk signature.
python3 - "$IMG" "$GPT" "$GPT_BACKUP" "$PARTUUID_FILE" "$TOTAL_SECTORS" "$ESP_START" "$ESP_SECTORS" "$ROOT_START" "$ROOT_SECTORS" "$BIOS_BOOT" "$LOG_START" "$LOG_SECTORS" <<'PY'
import os, struct, sys, uuid, zlib
img, gpt, backup, partuuid_file = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
total, esp_start, esp_len, root_start, root_len, bios_boot, log_start, log_len = map(int, sys.argv[5:])

def entry(active, ptype, start, length):
    # CHS fields set to the "use LBA" sentinel; every firmware since the
    # 1990s reads the LBA fields.
    chs = b"\xfe\xff\xff"
    return struct.pack("<B3sB3sII", 0x80 if active else 0, chs, ptype, chs, start, length)

mbr = bytearray(512)
sig = 0
while sig == 0:                                      # 0 means "no signature"
    sig = struct.unpack("<I", os.urandom(4))[0]
mbr[440:444] = struct.pack("<I", sig)                # disk signature
# PARTUUID of the root partition (p2), Linux's text form (lower-case hex).
root_partuuid = "%08x-02" % sig
if not gpt:
    mbr[446:462] = entry(True,  0xEF, esp_start,  esp_len)
    mbr[462:478] = entry(False, 0x83, root_start, root_len)
    mbr[478:494] = entry(False, 0xDA, log_start,  log_len)    # raw kernel log
else:
    # Protective MBR: one 0xEE entry spanning the disk (capped at 32 bits).
    mbr[446:462] = entry(False, 0xEE, 1, min(total - 1, 0xFFFFFFFF))
mbr[510:512] = b"\x55\xaa"

with open(img, "r+b") as f:
    f.write(mbr)
    if gpt:
        ESP_TYPE  = uuid.UUID("C12A7328-F81F-11D2-BA4B-00A0C93EC93B").bytes_le
        DATA_TYPE = uuid.UUID("0FC63DAF-8483-4772-8E79-3D69D8477DE4").bytes_le
        def part(ptype, first, length, name, guid=None):
            guid = guid or uuid.uuid4()
            return struct.pack("<16s16sQQQ72s", ptype, guid.bytes_le,
                               first, first + length - 1, 0, name.encode("utf-16-le"))
        root_guid = uuid.uuid4()
        root_partuuid = str(root_guid)               # mixed-endian text, lower case
        entries = bytearray(128 * 128)               # 128 entries of 128 bytes
        entries[0:128]   = part(ESP_TYPE,  esp_start,  esp_len,  "LEANDROS ESP")
        entries[128:256] = part(DATA_TYPE, root_start, root_len, "LEANDROS root", root_guid)
        if bios_boot:
            # Limine's BIOS stage 2 lives in a BIOS boot partition on GPT; the
            # 1 MiB gap before the ESP (LBA 64..2047, ~992 KiB) is free.
            BIOS_TYPE = uuid.UUID("21686148-6449-6E6F-744E-656564454649").bytes_le
            entries[256:384] = part(BIOS_TYPE, 64, esp_start - 64, "BIOS boot")
        LOG_TYPE = uuid.UUID("042B7470-1302-4E61-9347-A0130B276754").bytes_le
        lo = 384 if bios_boot else 256               # after the BIOS boot entry
        entries[lo:lo+128] = part(LOG_TYPE, log_start, log_len, "LEANDROS-LOG")
        entries_crc = zlib.crc32(entries)
        disk_guid = uuid.uuid4().bytes_le
        first_usable, last_usable = 34, total - 34

        def header(my_lba, other_lba, entries_lba):
            h = bytearray(92)
            h[0:8] = b"EFI PART"
            struct.pack_into("<IIIIQQQQ", h, 8, 0x00010000, 92, 0, 0,
                             my_lba, other_lba, first_usable, last_usable)
            h[56:72] = disk_guid
            struct.pack_into("<QIII", h, 72, entries_lba, 128, 128, entries_crc)
            struct.pack_into("<I", h, 16, zlib.crc32(h))     # CRC computed with its field zeroed
            return bytes(h).ljust(512, b"\0")

        f.seek(512);  f.write(header(1, total - 1, 2))
        f.seek(1024); f.write(entries)
        tail = bytes(entries) + header(total - 1, 1, total - 33)   # backup entries, then header
        with open(backup, "wb") as b:
            b.write(tail)
with open(partuuid_file, "w") as pf:
    pf.write(root_partuuid)
PY
ROOT_PARTUUID="$(cat "$PARTUUID_FILE")"

# p1: FAT32 ESP, laid out like build-all.sh's create_disk_image so Limine sees
# the same boot() tree on the stick as it does in QEMU.
mkfs.fat -C "$ESP_IMG" $((ESP_SECTORS / 2)) -F 32 -n LEANDROS >/dev/null
mmd   -i "$ESP_IMG" ::/EFI ::/EFI/BOOT ::/boot ::/boot/limine
mcopy -oi "$ESP_IMG" "$LIMINE_DIR/$EFI_NAME" "::/EFI/BOOT/$EFI_NAME"
if [[ "$ARCH" == x86_64 ]]; then
    mcopy -oi "$ESP_IMG" "$LIMINE_DIR/limine-bios.sys" ::/boot/limine/limine-bios.sys
    mcopy -oi "$ESP_IMG" "$LIMINE_DIR/limine-bios.sys" ::/limine-bios.sys
fi
mcopy -oi "$ESP_IMG" "$KERNEL"      ::/kernel.elf
mcopy -oi "$ESP_IMG" "$INITRD"      ::/initrd.gz
# The stick's own limine.conf: the repo file plus the root= cmdline.
STICK_CONF="$WORK/limine.conf"
if (( CMDLINE )); then
    python3 - "$LIMINE_CONF" "$STICK_CONF" "root=PARTUUID=$ROOT_PARTUUID${LEANDROS_USB_EXTRA_CMDLINE:+ $LEANDROS_USB_EXTRA_CMDLINE}" <<'PY'
import re, sys
src, dst, arg = sys.argv[1:]
out, done = [], False
for line in open(src).read().splitlines():
    if re.match(r"\s*(kernel_)?cmdline:", line):
        continue                                     # replaced below
    out.append(line)
    m = re.match(r"(\s*)path:", line)
    if m and not done:
        out.append(f"{m.group(1)}cmdline: {arg}")
        done = True
if not done:
    sys.exit("limine.conf has no `path:` line to attach the cmdline to")
open(dst, "w").write("\n".join(out) + "\n")
PY
    echo "   root=PARTUUID=$ROOT_PARTUUID (in the stick's limine.conf)"
else
    cp "$LIMINE_CONF" "$STICK_CONF"
fi
mcopy -oi "$ESP_IMG" "$STICK_CONF" ::/limine.conf
dd if="$ESP_IMG" of="$IMG" bs=1048576 seek=$((ESP_START * SECTOR >> 20)) conv=notrunc 2>/dev/null
rm -f "$ESP_IMG"

# p2: the populated, read-write f2fs root, verbatim.
dd if="$F2FS_IMG" of="$IMG" bs=1048576 seek=$((ROOT_START * SECTOR >> 20)) conv=notrunc,sparse 2>/dev/null \
    || dd if="$F2FS_IMG" of="$IMG" bs=1048576 seek=$((ROOT_START * SECTOR >> 20)) conv=notrunc 2>/dev/null

# BSD dd's `sparse` truncates the file at the last block it wrote; restore the
# full length (the GPT backup table sits in the tail of an image).
dd if=/dev/zero of="$IMG" bs=1 count=0 seek="$IMG_BYTES" 2>/dev/null
if (( GPT )) && [[ -n "$IMAGE_OUT" ]]; then
    dd if="$GPT_BACKUP" of="$IMG" bs=512 seek=$((TOTAL_SECTORS - GPT_TAIL)) conv=notrunc 2>/dev/null
fi

# Limine BIOS stage 1 into the MBR boot code (+ stage 2 into the gap before p1).
# x86_64 only; Limine supports BIOS boot from GPT as well. aarch64 has no BIOS.
if [[ "$ARCH" == x86_64 ]]; then
    out="$("$LIMINE_TOOL" bios-install "$IMG" 2>&1)" || { echo "$out" >&2; die "limine bios-install failed"; }
fi

# The MBR PARTUUID is built from the disk signature: make sure the BIOS stage
# install left it alone, or the stick's root= would name a disk that is not there.
if (( ! GPT )); then
    sig="$(dd if="$IMG" bs=1 skip=440 count=4 2>/dev/null | od -An -tx4 | tr -d ' \n')"
    [[ "$sig-02" == "$ROOT_PARTUUID" ]] || die "disk signature changed to $sig (expected ${ROOT_PARTUUID%-02})"
fi

# Sanity: the f2fs superblock magic must sit at p2 + 1024.
magic="$(dd if="$IMG" bs=1 skip=$((ROOT_START * SECTOR + 1024)) count=4 2>/dev/null | od -An -tx4 | tr -d ' ')"
[[ "$magic" == f2f52010 ]] || die "no f2fs superblock at the root partition (got '$magic')"

sparse_flash() {   # $1 = target (device or file)
    $SUDO python3 "$SCRIPT_DIR/sparse-write.py" "$IMG" "$1" \
        --esp $((ESP_START * SECTOR)) $((ESP_SECTORS * SECTOR)) \
        --root $((ROOT_START * SECTOR)) \
        --log $((LOG_START * SECTOR)) $((LOG_SECTORS * SECTOR))
}

if [[ -n "$SPARSE_ONTO" ]]; then
    SUDO=""
    echo "📝 Sparse write onto existing $SPARSE_ONTO..."
    sparse_flash "$SPARSE_ONTO"
    echo "✅ Image written to $SPARSE_ONTO"
    exit 0
fi

if [[ -n "$IMAGE_OUT" ]]; then
    echo "✅ Image written to $IMAGE_OUT"
    echo "   Flash it with: sudo dd if=$IMAGE_OUT of=<device> bs=4194304"
    exit 0
fi

# ── Write the device ──────────────────────────────────────────────────────────

echo
echo "⚠️  This will ERASE everything on $DEVICE"
echo "    ${MODEL:-unknown model}, $((DEV_BYTES / 1000000000)) GB"
if (( ! ASSUME_YES )); then
    read -r -p "Type the device path ($DEVICE) to continue: " confirm
    [[ "$confirm" == "$DEVICE" ]] || { echo "Aborted."; exit 1; }
fi

SUDO=""
(( EUID == 0 )) || SUDO="sudo"

case "$OS" in
    Darwin) diskutil unmountDisk force "$DEVICE" >/dev/null ;;
    Linux)
        for p in $(lsblk -lnpo NAME "$DEVICE" | tail -n +2); do
            $SUDO umount "$p" 2>/dev/null || true
        done
        ;;
esac

# Clear any old GPT backup header at the end of the stick: firmware that sees
# a valid backup GPT can prefer it over our MBR (a --gpt stick gets its own
# backup table written there after the image, below).
$SUDO dd if=/dev/zero of="$RAW_DEVICE" bs=512 count=34 \
    seek=$((DEV_BYTES / 512 - 34)) conv=notrunc 2>/dev/null || true

if (( SPARSE )); then
    echo "📝 Sparse write of the $((NEEDED_BYTES >> 20)) MiB image to $DEVICE (--full for a plain dd)..."
    sparse_flash "$RAW_DEVICE"
else
echo "📝 Writing $((NEEDED_BYTES >> 20)) MiB to $DEVICE..."
if [[ "$OS" == Darwin ]]; then
    $SUDO dd if="$IMG" of="$RAW_DEVICE" bs=4m
else
    $SUDO dd if="$IMG" of="$RAW_DEVICE" bs=4M conv=fsync status=progress
fi
echo "📝 Zeroing the $((LOG_SECTORS * SECTOR >> 20)) MiB log partition at LBA $LOG_START..."
if [[ "$OS" == Darwin ]]; then
    $SUDO dd if=/dev/zero of="$RAW_DEVICE" bs=1m count=$((LOG_SECTORS * SECTOR >> 20)) \
        seek=$((LOG_START * SECTOR >> 20)) 2>/dev/null
else
    $SUDO dd if=/dev/zero of="$RAW_DEVICE" bs=1M count=$((LOG_SECTORS * SECTOR >> 20)) \
        seek=$((LOG_START * SECTOR >> 20)) conv=fsync 2>/dev/null
fi
fi
if (( GPT )); then
    echo "📝 Writing the backup GPT at the end of $DEVICE..."
    $SUDO dd if="$GPT_BACKUP" of="$RAW_DEVICE" bs=512 \
        seek=$((TOTAL_SECTORS - GPT_TAIL)) conv=notrunc 2>/dev/null
fi
sync

case "$OS" in
    Darwin) diskutil eject "$DEVICE" >/dev/null || true ;;
    Linux)  $SUDO eject "$DEVICE" 2>/dev/null || true ;;
esac

echo "✅ $DEVICE is ready. Boot it in UEFI or legacy/CSM mode."
