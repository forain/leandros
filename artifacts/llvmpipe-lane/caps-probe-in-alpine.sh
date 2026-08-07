#!/bin/sh
# Runs INSIDE Alpine. Compiles caps_probe.c and runs it against a chosen lib set,
# surfaceless. WHICH: "softpipe" (m3-gl-stack) or "llvmpipe" (lane stage+deps).
# ARCH: aarch64|x86_64.  Emits an explicit rc trailer.
ARCH="$1"; WHICH="$2"
(
  apk add --no-cache gcc musl-dev libdrm expat zlib libffi \
      wayland-libs-client wayland-libs-server >/dev/null 2>&1 &&
  ln -sf /lib/libc.musl-$ARCH.so.1 /usr/lib/libc.so &&   # single-instance musl
  if [ "$WHICH" = softpipe ]; then
    # zig-built softpipe ship set (current on-target): m3-gl-stack sysroot (mounted at /m3)
    S=/m3/sysroot-$ARCH/usr
    INC=/out/stage-$ARCH/usr/include   # EGL/GLES headers (same Mesa version)
    LIBS="$S/lib"
    DEPS=""
  else
    # Alpine-built llvmpipe ship set: lane stage + deps
    S=/out/stage-$ARCH/usr
    INC=$S/include
    LIBS="$S/lib"
    DEPS="/out/deps-$ARCH"
  fi
  echo "== WHICH=$WHICH ARCH=$ARCH LIBS=$LIBS ==" &&
  ls -la "$LIBS"/libgallium-*.so "$LIBS"/gbm/dri_gbm.so 2>&1 | head &&
  gcc -fno-stack-protector /out/caps_probe.c -o /tmp/probe -I"$INC" -L"$LIBS" -lEGL -lGLESv2 \
      -Wl,-rpath-link,"$LIBS:$DEPS" &&
  echo "== compiled; running surfaceless caps probe ==" &&
  LD_LIBRARY_PATH="$LIBS:$DEPS" EGL_PLATFORM=surfaceless /tmp/probe
)
RC=$?
echo "=== rc=$RC arch=$ARCH which=$WHICH capsprobe ==="
