#!/bin/sh
# Cross-build libdrm 2.4.x (core only, all vendor drivers disabled) for
# <arch>-linux-musl into sysroot-<arch>. Usage: build-libdrm.sh <arch>
set -e
ARCH="$1"; [ -n "$ARCH" ] || { echo "usage: $0 <x86_64|aarch64>"; exit 2; }
ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT/src/libdrm"
meson setup "$ROOT/build/libdrm-$ARCH" --cross-file "$ROOT/cross-musl-$ARCH.ini" \
  --prefix=/usr --buildtype=release \
  -Dintel=disabled -Dradeon=disabled -Damdgpu=disabled -Dnouveau=disabled \
  -Dvmwgfx=disabled -Domap=disabled -Dexynos=disabled -Dfreedreno=disabled \
  -Dtegra=disabled -Dvc4=disabled -Detnaviv=disabled \
  -Dcairo-tests=disabled -Dvalgrind=disabled -Dman-pages=disabled -Dtests=false
ninja -C "$ROOT/build/libdrm-$ARCH"
DESTDIR="$ROOT/sysroot-$ARCH" ninja -C "$ROOT/build/libdrm-$ARCH" install
