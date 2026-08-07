#!/bin/sh
# Runs INSIDE Alpine. Compiles + runs smoke.c against the PATCHED ship set
# (libc.so NEEDED) + staged deps, forcing GALLIUM_DRIVER=llvmpipe. Provides a
# libc.so -> Alpine vanilla-musl symlink so the target-patched libs resolve.
ARCH="$1"
S=/out/stage-$ARCH/usr
D=/out/deps-$ARCH
(
  apk add --no-cache gcc musl-dev libdrm expat \
      wayland-libs-client wayland-libs-server zlib libffi >/dev/null 2>&1 &&
  ln -sf /lib/libc.musl-$ARCH.so.1 /usr/lib/libc.so &&  # single-instance musl (NOTES: two-libc trap)
  gcc /out/smoke.c -o /tmp/smoke -I"$S/include" -L"$S/lib" -lEGL -lGLESv2 \
      -Wl,-rpath-link,"$S/lib:$D" &&
  echo "== compiled; running llvmpipe smoke ==" &&
  LD_LIBRARY_PATH="$S/lib:$D" GALLIUM_DRIVER=llvmpipe LIBGL_ALWAYS_SOFTWARE=true \
      EGL_PLATFORM=surfaceless /tmp/smoke
)
RC=$?
echo "=== rc=$RC arch=$ARCH smoke ==="
