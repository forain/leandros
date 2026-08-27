#!/bin/sh
# Cross-build Mesa (EGL+GLESv2+GBM, softpipe/kms_swrast, no LLVM, no glvnd) with
# the WAYLAND platform for <arch>-linux-musl; installs the ship-set to stage-<arch>.
# -Dlegacy-wayland=bind-wayland-display compiles the EGL_WL_bind_wayland_display
# path (cosmic-panel bind_wl_display); NOTE: swrast still will not ADVERTISE that
# extension at runtime (it needs dma-buf import/export). Drop the option for a
# pure EGL_EXT_platform_wayland build. Usage: build-mesa-wayland.sh <arch>
# Prereqs: libdrm + libffi + libwayland in sysroot-<arch>; host scanner built;
#   src/mesa (25.3.x) checkout; .venv with mako/packaging/pyyaml; brew bison.
set -e
ARCH="$1"; [ -n "$ARCH" ] || { echo "usage: $0 <x86_64|aarch64>"; exit 2; }
ROOT="$(cd "$(dirname "$0")" && pwd)"
# Broadcom V3D (Raspberry Pi 5 / BCM2712, V3D 7.1) is aarch64-only for us, and
# needs NO LLVM (NIR + Broadcom's own QPU backend), so it costs only compile
# time. softpipe stays in the list as the QEMU/software fallback: the megadriver
# holds both and the pipe loader picks by DRM driver name at runtime.
case "$ARCH" in
  aarch64) GALLIUM_DRIVERS=softpipe,v3d ;;
  *)       GALLIUM_DRIVERS=softpipe ;;
esac
export PATH="/opt/homebrew/opt/bison/bin:$PATH"                     # modern bison ahead of Apple 2.3
export PYTHONPATH="$(echo "$ROOT"/.venv/lib/python3.*/site-packages)"  # mako/packaging for meson AND ninja
cd "$ROOT/src/mesa"
B="$ROOT/build/mesa-wayland-$ARCH"; rm -rf "$B"
meson setup "$B" \
  --cross-file "$ROOT/cross-musl-$ARCH.ini" \
  --native-file "$ROOT/native-host.ini" \
  --prefix=/usr --buildtype=release \
  --wrap-mode=default --force-fallback-for=zlib,expat \
  -Dplatforms=wayland \
  -Dlegacy-wayland=bind-wayland-display \
  -Degl=enabled -Dgles2=enabled -Dgbm=enabled -Dopengl=true \
  -Dglx=disabled -Dgallium-drivers="$GALLIUM_DRIVERS" -Dvulkan-drivers=[] \
  -Dllvm=disabled -Dshared-glapi=enabled -Dglvnd=disabled \
  -Dtools=[] -Dvalgrind=disabled
ninja -C "$B"
DESTDIR="$ROOT/stage-$ARCH" ninja -C "$B" install
echo "MESA-WAYLAND-$ARCH ship-set -> $ROOT/stage-$ARCH/usr/lib"
