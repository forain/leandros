#!/bin/sh
# Rung 1: build kmscube (GLES2-over-GBM + legacy KMS) as a DYNAMIC musl ELF.
# Compile .c with the zig-cc musl wrapper; LINK with musl-dyn-link.sh (zig ld.lld
# -pie) so we get ET_DYN + PT_INTERP against our merged Mesa sysroot (NOT zig cc,
# which would emit ET_EXEC / static — see musl-dynamic NOTES landmine 1).
# Usage: build-kmscube.sh <x86_64|aarch64>
set -e
D=/Users/forain/.claude-forain/jobs/afde2e74/tmp/m3-gl-stack
arch="$1"; [ -n "$arch" ] || { echo "usage: build-kmscube.sh <arch>"; exit 2; }
S=$D/sysroot-$arch
CC=$D/toolchain/$arch-linux-musl-cc
K=$D/src/kmscube
B=$D/build/kmscube-$arch
mkdir -p "$B" "$D/out"

# non-gst source set (matches meson `sources`), + cube-shadertoy (GLES3 present)
SRCS="common.c cube-smooth.c cube-gears.c cube-tex.c drm-atomic.c drm-common.c \
drm-legacy.c drm-offscreen.c esTransform.c frame-512x512-NV12.c frame-512x512-RGBA.c \
kmscube.c perfcntrs.c cube-shadertoy.c"

CFLAGS="-O2 -fPIC -fno-sanitize=all -std=gnu99 -DHAVE_GLES3 -Wno-unused-parameter"
# Mesa/EGL/GLES/gbm/drm headers from the merged sysroot (explicit -I, zig supplies libc headers)
INCS="-I$S/usr/include -I$S/usr/include/libdrm"

objs=""
for s in $SRCS; do
  o="$B/${s%.c}.o"
  "$CC" $CFLAGS $INCS -c "$K/$s" -o "$o"
  objs="$objs $o"
done

# link: dynamic PIE exe; Mesa stack + libm as direct NEEDED
LIBS="-L$S/usr/lib -lEGL -lGLESv2 -lgbm -ldrm -lm"
sh "$D/toolchain/musl-dyn-link.sh" "$arch" exe "$S" "$D/out/kmscube-$arch" $objs -- $LIBS
echo "BUILT $D/out/kmscube-$arch"
