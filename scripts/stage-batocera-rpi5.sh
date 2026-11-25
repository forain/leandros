#!/usr/bin/env bash
# Stage LeandrOS tryboot files on a Batocera Pi 5 boot partition, DRY-RUN by
# default. Only two things are ever created in <boot-dir>: the folder leandros/
# (kernel.img initrd.cpio cmdline.txt) and the file tryboot.txt. Every other
# file in <boot-dir> (Batocera's config.txt, kernel, dtbs ...) is checksummed
# before and after and must stay byte-identical. Paths in tryboot.txt are
# explicit (kernel=leandros/kernel.img, initramfs leandros/initrd.cpio,
# cmdline=leandros/cmdline.txt); os_prefix is not used. No device_tree= line:
# the firmware picks Batocera's own bcm2712-rpi-5-b.dtb / bcm2712d0-rpi-5-b.dtb
# from the boot root by board stepping; bcm2712-rpi-5-b.dtb must exist there
# (its checksum is recorded and re-verified). Refuses if autoboot.txt,
# tryboot.txt or leandros/ already exist.
#
# Rollback: --rollback removes exactly leandros/ and tryboot.txt.
# Usage: stage-batocera-rpi5.sh --boot-dir DIR --partuuid P [--kernel F] [--initrd F] [--apply]
#        stage-batocera-rpi5.sh --boot-dir DIR --rollback
#   <boot-dir> is a directory (a mounted boot partition or a copy of it).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$HERE")"
TPL="$ROOT/configs/rpi5-batocera"
BOOT="" PU="" APPLY=0 ROLLBACK=0
KERNEL="$ROOT/target/final-aarch64/kernel-direct.bin"
INITRD="$ROOT/initrd-aarch64.cpio"
die() { echo "stage-batocera-rpi5: $*" >&2; exit 1; }
while (($#)); do case "$1" in
    --boot-dir) BOOT="$2"; shift 2 ;;
    --partuuid) PU="$2"; shift 2 ;;
    --kernel) KERNEL="$2"; shift 2 ;;
    --initrd) INITRD="$2"; shift 2 ;;
    --apply) APPLY=1; shift ;;
    --rollback) ROLLBACK=1; shift ;;
    *) die "unknown argument $1" ;;
esac; done
[[ -d "$BOOT" ]] || die "--boot-dir must be a directory"
snap() { (cd "$BOOT" && find . -type f ! -path './leandros/*' ! -name tryboot.txt -print0 | sort -z | xargs -0 -r sha256sum); }
if ((ROLLBACK)); then
    before="$(snap)"
    rm -rf "$BOOT/leandros"; rm -f "$BOOT/tryboot.txt"; sync
    [[ "$before" == "$(snap)" ]] || die "rollback changed other files"
    echo "rolled back: removed leandros/ and tryboot.txt; other files unchanged"; exit 0
fi
[[ "$PU" =~ ^[0-9A-Fa-f-]{8,36}$ ]] || die "--partuuid looks wrong: '$PU'"
[[ -f "$KERNEL" && -f "$INITRD" ]] || die "kernel/initrd not found ($KERNEL, $INITRD)"
[[ -f "$BOOT/bcm2712-rpi-5-b.dtb" ]] || die "$BOOT/bcm2712-rpi-5-b.dtb missing (not a Pi 5 boot partition?)"
[[ ! -e "$BOOT/autoboot.txt" ]] || die "autoboot.txt present in $BOOT: A/B layout is not supported"
[[ ! -e "$BOOT/tryboot.txt" ]] || die "tryboot.txt already exists in $BOOT; refusing to replace it"
[[ ! -e "$BOOT/leandros" ]] || die "$BOOT/leandros already exists"
before="$(snap)"
cmdline="$(sed "s/@PARTUUID@/$PU/" "$TPL/leandros-cmdline.txt.in")"
grep -q '@' <<<"$cmdline" && die "unfilled placeholder in cmdline"
for k in tryboot_a_b autoboot; do
    grep -q "$k" "$TPL/tryboot.txt.in" "$TPL/leandros-cmdline.txt.in" && die "template mentions $k" || true
done
d0="absent"; [[ -f "$BOOT/bcm2712d0-rpi-5-b.dtb" ]] && d0="present"
echo "boot dir: $BOOT ($(wc -l <<<"$before") existing files checksummed; D0 dtb $d0)"
echo "would add: leandros/{kernel.img,initrd.cpio,cmdline.txt} and tryboot.txt"
echo "cmdline:   $cmdline"
if ((! APPLY)); then echo "dry run: nothing written (use --apply)"; exit 0; fi
mkdir "$BOOT/leandros"
cp "$KERNEL" "$BOOT/leandros/kernel.img"
cp "$INITRD" "$BOOT/leandros/initrd.cpio"
printf '%s\n' "$cmdline" > "$BOOT/leandros/cmdline.txt"
cp "$TPL/tryboot.txt.in" "$BOOT/tryboot.txt"
sync
cmp -s "$KERNEL" "$BOOT/leandros/kernel.img" && cmp -s "$INITRD" "$BOOT/leandros/initrd.cpio" || die "copy verify failed"
[[ "$before" == "$(snap)" ]] || die "EXISTING BOOT FILES CHANGED (checksum mismatch)"
echo "applied; existing boot files (incl. dtbs) verified unchanged"
