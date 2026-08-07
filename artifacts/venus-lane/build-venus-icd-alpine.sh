#!/bin/sh
# Runs INSIDE an Alpine 3.21 container (native toolchain, NO zig). Builds Mesa
# 25.3.6's Venus Vulkan ICD (libvulkan_virtio.so, src/virtio/vulkan) for
# LeandrOS, modeled directly on ../llvmpipe-lane/build-diag-softpipe.sh.
# Mounts:  /work/mesa (source, ro)   /out (artifact lane, rw)
# Emits an explicit '=== rc=N ...' trailer as the LAST line — trust that, not log content.
#
# -Dgallium-drivers: EMPTY was tried first per the task brief and meson REFUSED
# ("Feature egl cannot be enabled: EGL requires DRI, Haiku, Windows or Android" —
# with_egl requires with_dri, which requires with_gallium, i.e. a non-empty
# gallium-drivers list, because -Degl=enabled/-Dgbm=enabled are kept to mirror
# the existing ship-set config). softpipe was added back to satisfy that, same
# as build-diag-softpipe.sh already does — libvulkan_virtio.so itself does NOT
# link against gallium (see verify step 3: it is not in its NEEDED list); only
# meson's global feature-gating requires a gallium driver to be present.
ARCH="$1"
(
  apk add --no-cache build-base clang19 meson samurai bison flex python3 \
    py3-mako py3-packaging py3-yaml libdrm-dev wayland-dev wayland-protocols \
    expat-dev zlib-dev linux-headers pkgconf patchelf file &&
  # Alpine 3.21's libdisplay-info is 0.2.0 (soname libdisplay-info.so.2), but
  # the LeandrOS x86_64 sysroot (m3-gl-stack) already ships 0.3.0 (soname
  # libdisplay-info.so.3). Pin to edge's 0.3.0-r1 so libvulkan_virtio.so links
  # against .so.3 and needs NO new libdisplay-info staged on target.
  apk add --no-cache -X https://dl-cdn.alpinelinux.org/alpine/edge/main --allow-untrusted \
    libdisplay-info=0.3.0-r1 libdisplay-info-dev=0.3.0-r1 &&
  # Provide __stack_chk_guard locally (LeandrOS musl libc.so lacks it; Alpine
  # static libstdc++/libgcc reference it). Compiled -fno-stack-protector +PIC.
  cc -fPIC -fno-stack-protector -c /out/ssp_guard.c -o /tmp/ssp_guard.o &&
  B=/tmp/build-$ARCH &&
  rm -rf "$B" &&
  meson setup "$B" /work/mesa --prefix=/usr --buildtype=release --wrap-mode=nodownload \
    -Dplatforms=wayland -Dlegacy-wayland=bind-wayland-display \
    -Degl=enabled -Dgles2=enabled -Dgbm=enabled -Dopengl=true \
    -Dglx=disabled -Dgallium-drivers=softpipe -Dvulkan-drivers=virtio \
    -Dvulkan-icd-dir=/usr/share/vulkan/icd.d \
    -Dllvm=disabled -Dshared-glapi=enabled -Dglvnd=disabled \
    -Dtools=[] -Dvalgrind=disabled \
    "-Dc_args=['-fno-stack-protector','-U_FORTIFY_SOURCE','-D_FORTIFY_SOURCE=0']" \
    "-Dcpp_args=['-fno-stack-protector','-U_FORTIFY_SOURCE','-D_FORTIFY_SOURCE=0']" \
    "-Dc_link_args=['-static-libgcc','/tmp/ssp_guard.o']" \
    "-Dcpp_link_args=['-static-libstdc++','-static-libgcc','/tmp/ssp_guard.o']" &&
  ninja -C "$B" &&
  rm -rf /tmp/stage-$ARCH /out/stage-$ARCH &&
  DESTDIR=/tmp/stage-$ARCH ninja -C "$B" install &&
  mkdir -p /out/stage-$ARCH &&
  cp -a /tmp/stage-$ARCH/. /out/stage-$ARCH/ &&
  echo "== pre-patchelf NEEDED (libvulkan_virtio.so) ==" &&
  patchelf --print-needed /out/stage-$ARCH/usr/lib/libvulkan_virtio.so &&
  patchelf --replace-needed libc.musl-$ARCH.so.1 libc.so \
    /out/stage-$ARCH/usr/lib/libvulkan_virtio.so &&
  echo "== post-patchelf NEEDED ==" &&
  patchelf --print-needed /out/stage-$ARCH/usr/lib/libvulkan_virtio.so &&
  echo "== soname ==" &&
  patchelf --print-soname /out/stage-$ARCH/usr/lib/libvulkan_virtio.so &&
  echo "== icd entrypoint symbol ==" &&
  nm -D /out/stage-$ARCH/usr/lib/libvulkan_virtio.so | grep -i icd &&
  ls -la /out/stage-$ARCH/usr/lib/libvulkan_virtio.so &&
  file /out/stage-$ARCH/usr/lib/libvulkan_virtio.so &&
  echo "== ICD json ==" &&
  find /out/stage-$ARCH -iname 'virtio_icd*' -exec sh -c 'echo "-- {} --"; cat "{}"' \;
)
RC=$?
echo "=== rc=$RC arch=$ARCH mesa=25.3.6-venus-icd ==="
