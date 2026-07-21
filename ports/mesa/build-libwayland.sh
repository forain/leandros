#!/bin/sh
# Cross-build libwayland (client/server/cursor/egl + wayland-egl-backend.pc) for
# <arch>-linux-musl into sysroot-<arch>. Uses the native scanner from host/ via
# --native-file (NOT env PKG_CONFIG_PATH, which meson ignores for native deps).
# Prereqs: build-host-scanner.sh done; libffi already in sysroot-<arch>.
# Usage: build-libwayland.sh <arch>   (requires src/wayland checkout >=1.23)
set -e
ARCH="$1"; [ -n "$ARCH" ] || { echo "usage: $0 <x86_64|aarch64>"; exit 2; }
ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT/src/wayland"
B="$ROOT/build/wayland-$ARCH"; rm -rf "$B"
meson setup "$B" \
  --cross-file "$ROOT/cross-musl-$ARCH.ini" \
  --native-file "$ROOT/native-host.ini" \
  --prefix=/usr --buildtype=release \
  -Dlibraries=true -Dscanner=false -Dtests=false \
  -Ddocumentation=false -Ddtd_validation=false
ninja -C "$B"
DESTDIR="$ROOT/sysroot-$ARCH" ninja -C "$B" install
