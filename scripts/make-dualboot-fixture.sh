#!/bin/sh
# make-dualboot-fixture.sh <x86_64|aarch64> <out.img>
#
# Builds the dual-boot test disk (48 GiB sparse GPT mirroring the laptop):
#   p1 FAT32 ESP 1 GiB with real systemd-boot, loader.conf and a BLS entry
#   p2 ext4 "endeavouros" (laptop feature bits) holding an Alpine root plus
#      checksummed test data, forced past the 9 GiB shrink point
#   p3 swap at the end (odd start, like the laptop)
# Everything runs in Alpine/Debian containers (docker or podman); the host has
# no e2fsprogs. Sidecars <out.img>.{meta,manifest,esp.sha256} are written next
# to the image for check-dualboot-fixture.sh. Clone per run with `cp -c`.
#
# Env: DUALBOOT_ARTIFACTS (cache dir, default ~/code/leandros-artifacts/dualboot),
#      FX_FILLER_GIB (10), FX_DATA_MIB (2048), LEANDROS_CONTAINER,
#      FX_GAP_GIB (0): leave this many GiB unallocated between p2 and the swap
#      (p2 shrinks to match), for the free-space installer.
set -eu
ARCH=${1:?usage: $0 <x86_64|aarch64> <out.img>}
OUT=${2:?usage: $0 <x86_64|aarch64> <out.img>}
HERE=$(cd "$(dirname "$0")" && pwd)
. "$HERE/container-lib.sh"
leandros_pick_container || { echo "no container runtime: $CT_WHY" >&2; exit 1; }
case "$ARCH" in
  x86_64) EFI=x64 DEBARCH=amd64 ;;
  aarch64) EFI=aa64 DEBARCH=arm64 ;;
  *) echo "arch must be x86_64 or aarch64" >&2; exit 2 ;;
esac
ART=${DUALBOOT_ARTIFACTS:-$HOME/code/leandros-artifacts/dualboot}
CACHE=$ART/cache/$ARCH; BUILD=$ART/build/$ARCH
mkdir -p "$CACHE" "$BUILD"

if [ ! -s "$CACHE/systemd-boot$EFI.efi" ]; then
  echo "== fetching systemd-boot ($DEBARCH) from Debian (Alpine does not package it)"
  $CT run --rm -v "$CACHE:/o" debian:stable-slim sh -c "
    set -e; dpkg --add-architecture $DEBARCH; apt-get -qq update >/dev/null
    cd /tmp && apt-get -qq download systemd-boot-efi:$DEBARCH
    dpkg-deb -x systemd-boot-efi_*.deb x
    cp x/usr/lib/systemd/boot/efi/systemd-boot$EFI.efi /o/"
fi

rm -f "$BUILD"/fixture.*
$CT run --rm -e ARCH="$ARCH" -e CACHE=/cache -e OUTDIR=/out \
  -e FX_FILLER_GIB="${FX_FILLER_GIB:-10}" -e FX_DATA_MIB="${FX_DATA_MIB:-2048}" -e FX_NOPLACE="${FX_NOPLACE:-}" -e FX_GAP_GIB="${FX_GAP_GIB:-0}" \
  -v "$HERE/dualboot-fixture:/scripts:ro" -v "$CACHE:/cache" -v "$BUILD:/out" \
  alpine:3.21 sh /scripts/inner.sh

mkdir -p "$(dirname "$OUT")"
rm -f "$OUT"
cp -c "$BUILD/fixture.img" "$OUT" 2>/dev/null || cp "$BUILD/fixture.img" "$OUT"
for s in meta manifest esp.sha256; do cp "$BUILD/fixture.$s" "$OUT.$s"; done
echo "fixture: $OUT (sidecars $OUT.{meta,manifest,esp.sha256})"
