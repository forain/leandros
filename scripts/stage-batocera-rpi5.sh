#!/usr/bin/env bash
# Stage LeandrOS tryboot files on a Batocera Pi 5 boot partition, DRY-RUN by
# default. Batocera's own boot files must stay byte-identical: the script
# records sha256 of every pre-existing file under <boot-dir> before and after
# and fails if any changed or disappeared. It only ever ADDS four files:
#   leandros-kernel.img  leandros-initrd.cpio  leandros-cmdline.txt  tryboot.txt
# and refuses if tryboot.txt already exists, or autoboot.txt is present
# (tryboot_a_b/autoboot.txt are never used or created).
#
# Usage: stage-batocera-rpi5.sh --boot-dir DIR --partuuid P [--kernel F] [--initrd F] [--apply]
#   <boot-dir> is a directory (a mounted boot partition or a copy of it).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname "$HERE")"
TPL="$ROOT/configs/rpi5-batocera"
BOOT="" PU="" APPLY=0
KERNEL="$ROOT/target/final-aarch64/kernel-direct.bin"
INITRD="$ROOT/initrd-aarch64.cpio"
die() { echo "stage-batocera-rpi5: $*" >&2; exit 1; }
while (($#)); do case "$1" in
    --boot-dir) BOOT="$2"; shift 2 ;;
    --partuuid) PU="$2"; shift 2 ;;
    --kernel) KERNEL="$2"; shift 2 ;;
    --initrd) INITRD="$2"; shift 2 ;;
    --apply) APPLY=1; shift ;;
    *) die "unknown argument $1" ;;
esac; done
[[ -d "$BOOT" ]] || die "--boot-dir must be a directory"
[[ "$PU" =~ ^[0-9A-Fa-f-]{8,36}$ ]] || die "--partuuid looks wrong: '$PU'"
[[ -f "$KERNEL" && -f "$INITRD" ]] || die "kernel/initrd not found ($KERNEL, $INITRD)"
[[ ! -e "$BOOT/autoboot.txt" ]] || die "autoboot.txt present in $BOOT: tryboot_a_b layout is not supported"
[[ ! -e "$BOOT/tryboot.txt" ]] || die "tryboot.txt already exists in $BOOT; refusing to replace it"
for n in leandros-kernel.img leandros-initrd.cpio leandros-cmdline.txt; do
    [[ ! -e "$BOOT/$n" ]] || die "$BOOT/$n already exists"
done
snap() { (cd "$BOOT" && find . -type f ! -name 'leandros-*' ! -name tryboot.txt -print0 | sort -z | xargs -0 -r sha256sum); }
before="$(snap)"
cmdline="$(sed "s/@PARTUUID@/$PU/" "$TPL/leandros-cmdline.txt.in")"
grep -q '@' <<<"$cmdline" && die "unfilled placeholder in cmdline"
for k in tryboot_a_b autoboot; do
    grep -q "$k" "$TPL/tryboot.txt.in" "$TPL/leandros-cmdline.txt.in" && die "template mentions $k" || true
done
echo "boot dir: $BOOT ($(wc -l <<<"$before") existing files checksummed)"
echo "would add: leandros-kernel.img leandros-initrd.cpio leandros-cmdline.txt tryboot.txt"
echo "cmdline:   $cmdline"
if ((! APPLY)); then echo "dry run: nothing written (use --apply)"; exit 0; fi
cp "$KERNEL" "$BOOT/leandros-kernel.img"
cp "$INITRD" "$BOOT/leandros-initrd.cpio"
printf '%s\n' "$cmdline" > "$BOOT/leandros-cmdline.txt"
cp "$TPL/tryboot.txt.in" "$BOOT/tryboot.txt"
sync
after="$(snap)"
[[ "$before" == "$after" ]] || die "EXISTING BOOT FILES CHANGED (checksum mismatch)"
echo "applied; existing boot files verified unchanged"
