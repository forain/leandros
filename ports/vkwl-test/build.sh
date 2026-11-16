#!/bin/sh
# Build vkwl (Vulkan swapchain on a Wayland surface) as a dynamic musl ELF.
#
# Deliberately NOT the Alpine-container idiom the rest of venus-lane uses:
# vkwl links libwayland-client, and the one toolchain already proven to produce
# a libwayland-client-linked binary that runs on LeandrOS is the m3-gl-stack
# musl toolchain that built wlclient (build-wlclient.sh). Reusing it means the
# DT_NEEDED closure is by construction the same closure already staged in the
# guest image. The Vulkan headers come from $MESA_SRC/include (default the llvmpipe-lane checkout),
# exposed through a private include dir holding ONLY vulkan/ so it cannot
# shadow the sysroot's own headers.
#
# Source lives in ports/vkwl-test/vkwl.c (this directory); the staged output
# goes to the venus-lane stage dir that scripts/mkfs-f2fs-populated.py packs.
# Usage: build-vkwl.sh <x86_64|aarch64>
set -e
D="$HOME/code/leandros-artifacts/m3-gl-stack"
C="$HOME/code/leandros-artifacts/m4-client"
V="$HOME/code/leandros-artifacts/venus-lane"
SRC="$(cd "$(dirname "$0")" && pwd)"
arch="$1"; [ -n "$arch" ] || { echo "usage: build-vkwl.sh <arch>"; exit 2; }
S="$D/sysroot-$arch"
CC="$D/toolchain/$arch-linux-musl-cc"
B="$V/build-vkwl-$arch"
mkdir -p "$B/vkinc"
MESA_INC="${MESA_SRC:-$HOME/code/leandros-artifacts/llvmpipe-lane/src/mesa}/include"
ln -sfn "$MESA_INC/vulkan" "$B/vkinc/vulkan"; [ -d "$MESA_INC/vk_video" ] && ln -sfn "$MESA_INC/vk_video" "$B/vkinc/vk_video"

CFLAGS="-O2 -fPIC -std=gnu11 -Wall -Wextra -Wno-unused-parameter -Wno-unused-function -fno-stack-protector -U_FORTIFY_SOURCE -D_FORTIFY_SOURCE=0"
INCS="-I$S/usr/include -I$C -I$B/vkinc"

objs=""
for s in "$C/xdg-shell-protocol.c" "$SRC/vkwl.c"; do
  o="$B/$(basename "${s%.c}").o"
  "$CC" $CFLAGS $INCS -c "$s" -o "$o"
  objs="$objs $o"
done

mkdir -p "$V/stage-$arch/usr/bin"
LIBS="-L$S/usr/lib -lwayland-client -lm"
sh "$D/toolchain/musl-dyn-link.sh" "$arch" exe "$S" "$V/stage-$arch/usr/bin/vkwl" $objs -- $LIBS
echo "== NEEDED =="
patchelf --print-needed "$V/stage-$arch/usr/bin/vkwl" 2>/dev/null || true
file "$V/stage-$arch/usr/bin/vkwl" || true
echo "BUILT $V/stage-$arch/usr/bin/vkwl"
