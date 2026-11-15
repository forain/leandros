#!/usr/bin/env bash
# Build the AWS EC2 boot disk image of LeandrOS: a raw GPT UEFI disk (Limine
# revision >= 6 from build-all.sh's pinned release, BOOTX64.EFI / BOOTAA64.EFI
# on the ESP), the f2fs root and a small /home, with a serial-console kernel
# command line. It is write-usb.sh's image mode with EC2 sizing and cmdline.
#
#   ./scripts/ec2-image.sh <x86_64|aarch64> [--out F] [--authorized-keys F | --no-keys]
#
# Run ./scripts/build-all.sh --arch <arch> first. Output: build/ec2-<arch>.img
# (sparse). Upload it with scripts/ec2-ami.sh; see docs/hardware/aws-ec2.md.
#
# Environment:
#   LEANDROS_EC2_CMDLINE   kernel arguments after root=PARTUUID=... (default:
#                          "nvme.rw=1 console=ttyS0 ec2=1" on x86_64,
#                          "nvme.rw=1 ec2=1" on aarch64: the console there
#                          comes from the firmware/SPCR). nvme.rw=1 makes the
#                          NVMe EBS root writable (read-only by default).
#   LEANDROS_EC2_ROOT_GIB  root partition reservation (default: the f2fs image
#                          size rounded up, plus 1 GiB)
#   LEANDROS_EC2_HOME_GIB  /home partition size (default 1)
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/.."

die() { echo "ec2-image: $*" >&2; exit 1; }
ARCH="${1:-}"; [[ $# -gt 0 ]] && shift
case "$ARCH" in x86_64|aarch64) ;; *) die "usage: $0 <x86_64|aarch64> [--out F] [--authorized-keys F | --no-keys]" ;; esac
OUT="build/ec2-$ARCH.img"
KEYARGS=(--no-keys)
while [[ $# -gt 0 ]]; do
    case "$1" in
        --out) OUT="${2:?--out needs a path}"; shift 2 ;;
        --authorized-keys) KEYARGS=(--authorized-keys "${2:?--authorized-keys needs a file}"); shift 2 ;;
        --no-keys) KEYARGS=(--no-keys); shift ;;
        *) die "unknown option: $1" ;;
    esac
done

F2FS="f2fs-data0-$ARCH.img"
[[ -f "$F2FS" ]] || die "missing $F2FS - run ./scripts/build-all.sh --arch $ARCH first"
sz=$(stat -c %s "$F2FS" 2>/dev/null || stat -f %z "$F2FS")
ROOT_GIB="${LEANDROS_EC2_ROOT_GIB:-$(( (sz + (1 << 30) - 1) / (1 << 30) + 1 ))}"
HOME_GIB="${LEANDROS_EC2_HOME_GIB:-1}"
if [[ "$ARCH" == x86_64 ]]; then DEF_CL="nvme.rw=1 console=ttyS0 ec2=1"; else DEF_CL="nvme.rw=1 ec2=1"; fi
CL="${LEANDROS_EC2_CMDLINE-$DEF_CL}"

mkdir -p "$(dirname "$OUT")"
rm -f "$OUT"
# EC2 hosts have no Intel iGPU: the generic (non-Intel) root image is right.
LEANDROS_USB_ALLOW_GENERIC_GPU=1 \
LEANDROS_USB_ROOT_GIB="$ROOT_GIB" LEANDROS_USB_HOME_GIB="$HOME_GIB" \
LEANDROS_USB_EXTRA_CMDLINE="$CL" \
    "$SCRIPT_DIR/write-usb.sh" "$ARCH" --image "$OUT" --gpt "${KEYARGS[@]}"
echo "ec2-image: $OUT ($(( $(stat -c %s "$OUT" 2>/dev/null || stat -f %z "$OUT") >> 20 )) MiB apparent, cmdline root=PARTUUID=... $CL)"
