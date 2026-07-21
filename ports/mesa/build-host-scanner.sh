#!/bin/sh
# Build the NATIVE (macOS) wayland-scanner + wayland-scanner.pc into host/.
# Needs brew expat on the default pkg-config path. Unpatched on macOS.
# Requires src/wayland (>=1.23) checkout.
set -e
ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT/src/wayland"
meson setup "$ROOT/build/host-scanner" \
  -Dlibraries=false -Dscanner=true -Dtests=false \
  -Ddocumentation=false -Ddtd_validation=false \
  --prefix="$ROOT/host" --buildtype=release
ninja -C "$ROOT/build/host-scanner"
ninja -C "$ROOT/build/host-scanner" install
"$ROOT/host/bin/wayland-scanner" --version
