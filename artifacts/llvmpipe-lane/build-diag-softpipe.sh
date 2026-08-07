#!/bin/sh
# Runs INSIDE an Alpine 3.21 container. Builds Mesa 25.3.6 SOFTPIPE-ONLY (no LLVM,
# no glvnd) matching the SHIPPED LeandrOS softpipe ship-set config exactly, then
# patchelf's libgallium's libc soname (Alpine libc.musl-<arch>.so.1 -> libc.so)
# so it is a drop-in for the current ship set. Emits stage-diag-<arch>/usr/lib/
# with the freshly built libgallium-25.3.6.so (contains the W2DIAG-instrumented
# softpipe). Also usable UNMODIFIED for the clean FIX build (same config).
# Trust the '=== rc=N ...' trailer, not log content.
ARCH="$1"
(
  # NOTE: zstd-dev deliberately OMITTED so Mesa disables the zstd disk-cache
  # (shipped cross-build had no zstd -> no libzstd.so.1 NEEDED). Static libstdc++
  # + libgcc so the produced libgallium keeps the SHIPPED NEEDED set exactly
  # (libz,libexpat,libdrm,libc.so) — a true drop-in with no new runtime deps.
  apk add --no-cache build-base clang19 meson samurai bison flex python3 \
    py3-mako py3-packaging py3-yaml libdrm-dev wayland-dev wayland-protocols \
    expat-dev zlib-dev linux-headers pkgconf patchelf file &&
  # Provide __stack_chk_guard locally (LeandrOS musl libc.so lacks it; Alpine
  # static libstdc++/libgcc reference it). Compiled -fno-stack-protector +PIC.
  cc -fPIC -fno-stack-protector -c /out/ssp_guard.c -o /tmp/ssp_guard.o &&
  B=/tmp/build-$ARCH &&
  rm -rf "$B" &&
  meson setup "$B" /work/mesa --prefix=/usr --buildtype=release --wrap-mode=nodownload \
    -Dplatforms=wayland -Dlegacy-wayland=bind-wayland-display \
    -Degl=enabled -Dgles2=enabled -Dgbm=enabled -Dopengl=true \
    -Dglx=disabled -Dgallium-drivers=softpipe -Dvulkan-drivers=[] \
    -Dllvm=disabled -Dshared-glapi=enabled -Dglvnd=disabled \
    -Dtools=[] -Dvalgrind=disabled \
    "-Dc_args=['-fno-stack-protector','-U_FORTIFY_SOURCE','-D_FORTIFY_SOURCE=0']" \
    "-Dcpp_args=['-fno-stack-protector','-U_FORTIFY_SOURCE','-D_FORTIFY_SOURCE=0']" \
    "-Dc_link_args=['-static-libgcc','/tmp/ssp_guard.o']" \
    "-Dcpp_link_args=['-static-libstdc++','-static-libgcc','/tmp/ssp_guard.o']" &&
  ninja -C "$B" &&
  rm -rf /tmp/stage-$ARCH /out/stage-diag-$ARCH &&
  DESTDIR=/tmp/stage-$ARCH ninja -C "$B" install &&
  mkdir -p /out/stage-diag-$ARCH/usr/lib &&
  cp -a /tmp/stage-$ARCH/usr/lib/libgallium-25.3.6.so /out/stage-diag-$ARCH/usr/lib/ &&
  echo "== pre-patchelf NEEDED ==" &&
  patchelf --print-needed /out/stage-diag-$ARCH/usr/lib/libgallium-25.3.6.so &&
  patchelf --replace-needed libc.musl-$ARCH.so.1 libc.so \
    /out/stage-diag-$ARCH/usr/lib/libgallium-25.3.6.so &&
  echo "== post-patchelf NEEDED ==" &&
  patchelf --print-needed /out/stage-diag-$ARCH/usr/lib/libgallium-25.3.6.so &&
  echo "== soname ==" &&
  patchelf --print-soname /out/stage-diag-$ARCH/usr/lib/libgallium-25.3.6.so &&
  ls -la /out/stage-diag-$ARCH/usr/lib/libgallium-25.3.6.so &&
  file /out/stage-diag-$ARCH/usr/lib/libgallium-25.3.6.so
)
RC=$?
echo "=== rc=$RC arch=$ARCH mesa=25.3.6-softpipe-diag ==="
