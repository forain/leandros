#!/bin/sh
# Runs INSIDE an Alpine 3.21 container (native toolchain, NO zig).
# Builds Mesa 25.3.6 with llvmpipe+softpipe (LLVM 19 shared) matching the
# existing LeandrOS ship-set config (wayland platform, non-glvnd, EGL/GLES2/GBM).
# Mounts:  /work/mesa (source, ro-ish)   /out (artifact lane, rw)
# Emits an explicit '=== rc=N ...' trailer as the LAST line — trust that, not log content.
ARCH="$1"
export PATH="/usr/lib/llvm19/bin:$PATH"
(
  apk add --no-cache build-base clang19 llvm19-dev llvm19-static \
    meson samurai bison flex python3 py3-mako py3-packaging py3-yaml \
    libdrm-dev wayland-dev wayland-protocols expat-dev zlib-dev zstd-dev \
    linux-headers pkgconf patchelf file &&
  echo "== llvm-config: $(command -v llvm-config) $(llvm-config --version) ==" &&
  B=/tmp/build-$ARCH &&
  rm -rf "$B" &&
  meson setup "$B" /work/mesa --prefix=/usr --buildtype=release --wrap-mode=nodownload \
    -Dplatforms=wayland -Dlegacy-wayland=bind-wayland-display \
    -Degl=enabled -Dgles2=enabled -Dgbm=enabled -Dopengl=true \
    -Dglx=disabled -Dgallium-drivers=llvmpipe,softpipe -Dvulkan-drivers=[] \
    -Dllvm=enabled -Dshared-llvm=enabled -Dshared-glapi=enabled -Dglvnd=disabled \
    -Dtools=[] -Dvalgrind=disabled &&
  ninja -C "$B" &&
  rm -rf /tmp/stage-$ARCH /out/stage-$ARCH &&
  DESTDIR=/tmp/stage-$ARCH ninja -C "$B" install &&
  mkdir -p /out/stage-$ARCH &&
  cp -a /tmp/stage-$ARCH/. /out/stage-$ARCH/ &&
  echo "== installed ship-set ==" &&
  ls -la /out/stage-$ARCH/usr/lib/*.so* /out/stage-$ARCH/usr/lib/gbm/*.so* 2>&1
)
RC=$?
echo "=== rc=$RC arch=$ARCH mesa=25.3.6-llvmpipe ==="
