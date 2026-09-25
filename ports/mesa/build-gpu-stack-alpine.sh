#!/bin/sh
# build-gpu-stack-alpine.sh — the GPU Mesa ship-set for LeandrOS COSMIC sessions.
#
# Runs INSIDE an Alpine 3.21 container of the TARGET architecture (native
# musl toolchain, no zig, no LLVM). Driven by ports/mesa/build-gpu-stack.sh,
# which picks podman/docker and mounts:
#   /work/mesa   Mesa 25.3.6 source (read-only is fine)
#   /src         this directory (for ssp_guard.c)
#   /out         where gpu-stage-<arch>/ is written
#
# One megadriver carries every renderer a LeandrOS guest can meet:
#   zink      OpenGL on Vulkan — with the Venus ICD below, cosmic-comp's GLES
#             becomes Vulkan work on the host GPU (the proven 20 fps path).
#   virgl     OpenGL passthrough to the host's virglrenderer (virtio-vga-gl /
#             virtio-gpu-gl-pci without Venus — laptop ANV-less hosts, UTM).
#   softpipe  kept ONLY so a guest with no host 3D can still bring up a text
#             console session or a headless test; a COSMIC session on it is
#             refused by /bin/gpu-env unless explicitly allowed.
#   v3d       (aarch64 only) the Raspberry Pi 5 target; costs compile time only.
# plus the Venus Vulkan ICD (libvulkan_virtio.so) and the Khronos loader
# (libvulkan.so.1) that zink dlopen()s. Built in ONE tree so zink and the ICD
# are ABI-consistent.
#
# Link recipe (from the venus/zink lanes): LeandrOS's musl libc.so lacks
# __stack_chk_guard, which Alpine's static libstdc++/libgcc reference, so the
# guard is provided locally and the C++ runtime is linked statically. Every
# shipped ELF has its libc.musl-<arch>.so.1 DT_NEEDED rewritten to libc.so.
#
# Emits '=== rc=N arch=A ===' as the LAST line — trust that, not log content.
ARCH="$1"
MODE="${2:-all}"   # all | probe (rebuild only gpuprobe against an existing stage)
case "$ARCH" in
  aarch64) DRIVERS=zink,virgl,softpipe,v3d ;;
  x86_64)  DRIVERS=zink,virgl,softpipe ;;
  *) echo "usage: $0 <x86_64|aarch64>"; echo "=== rc=2 arch=$ARCH ==="; exit 2 ;;
esac
(
  set -e
  [ "$(uname -m)" = "$ARCH" ] || { echo "container is $(uname -m), wanted $ARCH"; exit 3; }
  apk add --no-cache build-base meson samurai bison flex python3 py3-mako \
    py3-packaging py3-yaml libdrm-dev wayland-dev wayland-protocols \
    expat-dev zlib-dev zstd-dev linux-headers pkgconf patchelf file \
    vulkan-headers vulkan-loader
  apk add --no-cache -X https://dl-cdn.alpinelinux.org/alpine/edge/main --allow-untrusted \
    libdisplay-info=0.3.0-r1 libdisplay-info-dev=0.3.0-r1
  cc -fPIC -fno-stack-protector -c /src/ssp_guard.c -o /tmp/ssp_guard.o
  S=/tmp/gpu-stage-$ARCH
  if [ "$MODE" = probe ]; then
    rm -rf "$S"; mkdir -p "$S"; cp -a "/out/gpu-stage-$ARCH/." "$S/"
  else
  B=/tmp/build-gpu-$ARCH
  rm -rf "$B"
  meson setup "$B" /work/mesa --prefix=/usr --buildtype=release --wrap-mode=nodownload \
    -Dplatforms=wayland -Dlegacy-wayland=bind-wayland-display \
    -Degl=enabled -Dgles2=enabled -Dgbm=enabled -Dopengl=true \
    -Dglx=disabled -Dgallium-drivers=$DRIVERS -Dvulkan-drivers=virtio \
    -Dvulkan-icd-dir=/usr/share/vulkan/icd.d \
    -Dllvm=disabled -Dshared-glapi=enabled -Dglvnd=disabled \
    -Dtools=[] -Dvalgrind=disabled \
    "-Dc_args=['-fno-stack-protector','-U_FORTIFY_SOURCE','-D_FORTIFY_SOURCE=0']" \
    "-Dcpp_args=['-fno-stack-protector','-U_FORTIFY_SOURCE','-D_FORTIFY_SOURCE=0']" \
    "-Dc_link_args=['-static-libgcc','/tmp/ssp_guard.o']" \
    "-Dcpp_link_args=['-static-libstdc++','-static-libgcc','/tmp/ssp_guard.o']"
  ninja -C "$B"
  rm -rf "$S"
  DESTDIR="$S" ninja -C "$B" install
  fi
  # gpuprobe: linked against the libraries it ships with. Alpine's PT_INTERP
  # (/lib/ld-musl-<arch>.so.1) is already the guest's; only the libc soname
  # needs rewriting, done by the loop below.
  mkdir -p "$S/usr/bin"
  cc -O2 -fno-stack-protector -o "$S/usr/bin/gpuprobe" /src/gpuprobe.c \
    -I"$S/usr/include" -L"$S/usr/lib" -Wl,-rpath-link,"$S/usr/lib" \
    -lEGL -lGLESv2 -lgbm /tmp/ssp_guard.o
  # The loader zink dlopen()s, and the zstd runtime the shader cache needs
  # (the older softpipe-only sysroots never had it).
  cp -L /usr/lib/libvulkan.so.1 "$S/usr/lib/"
  cp -L /usr/lib/libzstd.so.1 "$S/usr/lib/"
  cd "$S/usr/lib"
  for f in $(find . ../bin -type f); do
    if file "$f" | grep -q ELF; then
      patchelf --replace-needed "libc.musl-$ARCH.so.1" libc.so "$f" 2>/dev/null || true
    fi
  done
  echo "== gallium drivers in the megadriver =="
  for d in zink virgl softpipe v3d; do
    printf '%s: %s\n' "$d" "$(strings libgallium-25.3.6.so | grep -c "^${d}_\|${d}_create_screen\|pipe_${d}_create_screen")"
  done
  echo "== ICD =="; ls -l libvulkan_virtio.so libvulkan.so.1
  cat "$S/usr/share/vulkan/icd.d/"virtio_icd*.json
  echo "== NEEDED (musl soname must be gone) =="
  for f in libgallium-25.3.6.so libEGL.so.1.0.0 libGLESv2.so.2.0.0 libgbm.so.1.0.0 gbm/dri_gbm.so libvulkan_virtio.so libvulkan.so.1 libzstd.so.1; do
    printf '%s: ' "$f"; readelf -d "$f" | awk '/NEEDED/{printf "%s ", $5} END{print ""}'
  done
  if readelf -d libgallium-25.3.6.so libvulkan_virtio.so libvulkan.so.1 | grep -q 'libc.musl'; then
    echo "musl soname still present"; exit 4
  fi
  rm -rf "/out/gpu-stage-$ARCH"
  mkdir -p "/out/gpu-stage-$ARCH"
  cp -a "$S/." "/out/gpu-stage-$ARCH/"
)
echo "=== rc=$? arch=$ARCH ==="
