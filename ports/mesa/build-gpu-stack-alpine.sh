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
INTEL="${INTEL:-0}"
OUT_SUFFIX="${OUT_SUFFIX:-}"
DEST="gpu-stage-$ARCH$OUT_SUFFIX"
if [ "$INTEL" = 1 ]; then
  # iris (Gallium GL on Gen8+) + ANV (Vulkan). Both need Mesa's precompiled
  # CL shaders (mesa_clc, built in the LLVM stage below) and glslang for ANV's
  # float64 emulation; the brw compiler itself needs no LLVM at runtime.
  # Ray tracing (Gen12.5+) and the Gen8-only ELK compiler are not needed for
  # the Gen9.5 UHD 620 and are left out.
  DRIVERS="$DRIVERS,iris"
fi
(
  set -e
  [ "$(uname -m)" = "$ARCH" ] || { echo "container is $(uname -m), wanted $ARCH"; exit 3; }
  apk add --no-cache build-base meson samurai bison flex python3 py3-mako \
    py3-packaging py3-yaml libdrm-dev wayland-dev wayland-protocols \
    expat-dev zlib-dev zstd-dev linux-headers pkgconf patchelf file \
    vulkan-headers vulkan-loader patch
  apk add --no-cache -X https://dl-cdn.alpinelinux.org/alpine/edge/main --allow-untrusted \
    libdisplay-info=0.3.0-r1 libdisplay-info-dev=0.3.0-r1
  NVK="${NVK:-1}"
  VKDRV=virtio
  [ "$INTEL" = 1 ] && VKDRV=$VKDRV,intel
  NEEDCLC=0
  if [ "$NVK" = 1 ] || [ "$INTEL" = 1 ]; then NEEDCLC=1; fi
  if [ "$NEEDCLC" = 1 ] && [ "$MODE" != probe ]; then
    [ "$NVK" = 1 ] && VKDRV=$VKDRV,nouveau
    [ "$INTEL" = 1 ] && apk add --no-cache glslang
    # NVK (nouveau ICD) needs: NAK (Rust: rustc >= 1.82, bindgen >= 0.71.1,
    # libclang), Meson >= 1.7 (Alpine 3.21 ships 1.6), and mesa_clc +
    # vtn_bindgen2 (LLVM/libclc/SPIRV-LLVM-Translator). mesa_clc is built in a
    # first, driver-less tree and installed to /tmp/clc-inst; the real tree
    # then uses -Dmesa-clc=system with LLVM DISABLED, so no LLVM ends up in any
    # shipped ELF.
    apk add --no-cache rust cargo clang19-dev clang19-libclang llvm19-dev libclc libclc-dev \
      spirv-llvm-translator-dev spirv-tools-dev py3-pip elfutils-dev cbindgen
    # Under qemu-user (an x86_64 container on an aarch64 Docker host) Alpine's
    # rustc/cargo abort at start-up inside scudo's 32-bit primary allocator
    # ("CHECK failed @ primary32.h"). They only link libscudo for their malloc,
    # so an empty stand-in makes them fall back to musl's malloc. Self-detecting:
    # a native container never takes this branch.
    if ! rustc --version >/dev/null 2>&1; then
      echo "rustc aborts (qemu-user + scudo); replacing libscudo.so with an empty stub"
      echo 'int leandros_no_scudo;' > /tmp/noscudo.c
      cc -shared -fPIC -Wl,-soname,libscudo.so -o /usr/lib/libscudo.so /tmp/noscudo.c
    fi
    pip install --no-cache-dir --break-system-packages 'meson==1.8.2'
    # bindgen-cli: Alpine 3.21's is 0.70 (< 0.71.1). Built once per arch with
    # a pinned version and cached in /cache (host: m3-gl-stack/nvk-cache).
    BINDGEN_VER=0.72.1
    if [ ! -x "/cache/bindgen-$BINDGEN_VER-$ARCH/bin/bindgen" ]; then
      cargo install --locked --version "$BINDGEN_VER" --root "/cache/bindgen-$BINDGEN_VER-$ARCH" bindgen-cli
    fi
    export PATH="/cache/bindgen-$BINDGEN_VER-$ARCH/bin:/tmp/clc-inst/bin:$PATH"
    export LIBCLANG_PATH=/usr/lib/llvm19/lib
    rustc --version; bindgen --version; meson --version
  fi
  cc -fPIC -fno-stack-protector -c /src/ssp_guard.c -o /tmp/ssp_guard.o
  S=/tmp/gpu-stage-$ARCH
  if [ "$MODE" = probe ]; then
    rm -rf "$S"; mkdir -p "$S"; cp -a "/out/$DEST/." "$S/"
  else
  # Patches from ports/mesa/patches go onto a private copy of the tree.
  MESA=/work/mesa
  if ls /src/patches/*.patch >/dev/null 2>&1 || [ "$NEEDCLC" = 1 ]; then
    rm -rf /tmp/mesa-src; cp -a /work/mesa /tmp/mesa-src; MESA=/tmp/mesa-src
    for p in /src/patches/*.patch; do [ -f "$p" ] || continue; echo "applying $p"; patch -d "$MESA" -p1 < "$p"; done
  fi
  if [ "$NEEDCLC" = 1 ]; then
    # Crate wraps: pre-fetched + sha256-verified on the host; --wrap-mode=
    # nodownload resolves them from subprojects/packagecache.
    mkdir -p "$MESA/subprojects/packagecache"
    cp /cache/crates/* "$MESA/subprojects/packagecache/"
    # Stage 1: mesa_clc + vtn_bindgen2 (native tools; LLVM lives only here).
    BC=/tmp/build-clc-$ARCH
    rm -rf "$BC"
    meson setup "$BC" "$MESA" --prefix=/tmp/clc-inst --buildtype=release --wrap-mode=nodownload \
      -Dllvm=enabled -Dmesa-clc=enabled -Dinstall-mesa-clc=true \
      -Dgallium-drivers=[] -Dvulkan-drivers=[] -Dplatforms=[] \
      -Dopengl=false -Degl=disabled -Dgbm=disabled -Dglx=disabled -Dgles1=disabled -Dgles2=disabled \
      -Dtools=[] -Dvalgrind=disabled
    ninja -C "$BC" install
    rm -rf "$BC"
    # -Dmesa-clc=system makes Mesa auto-link SPIRV-Tools (debug SPIR-V dumps
    # only); Alpine's is shared-only and would add an unshipped libSPIRV-Tools.so
    # NEEDED to libgallium and the ICDs, so keep it out of the real tree.
    CLCARG="-Dmesa-clc=system -Dspirv-tools=disabled"
  else
    CLCARG=
  fi
  INTELARG=
  [ "$INTEL" = 1 ] && INTELARG="-Dintel-rt=disabled -Dintel-elk=false"
  B=/tmp/build-gpu-$ARCH
  rm -rf "$B"
  meson setup "$B" "$MESA" --prefix=/usr --buildtype=release --wrap-mode=nodownload \
    -Dplatforms=wayland -Dlegacy-wayland=bind-wayland-display \
    -Degl=enabled -Dgles2=enabled -Dgbm=enabled -Dopengl=true \
    -Dglx=disabled -Dgallium-drivers=$DRIVERS -Dvulkan-drivers=$VKDRV $CLCARG $INTELARG \
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
  # Alpine's prebuilt loader is built -fstack-protector. On aarch64 GCC reads
  # the canary from the GLOBAL __stack_chk_guard (x86_64 uses %fs:0x28), which
  # LeandrOS libc.so does not export, so the loader failed to relocate there.
  # Ship the guard as a tiny shared object and make every prebuilt Alpine ELF
  # we copy that references it depend on it.
  cc -shared -fPIC -fno-stack-protector -Wl,-soname,libleandros_ssp.so.1 \
    -o "$S/usr/lib/libleandros_ssp.so.1" /src/ssp_guard.c
  cd "$S/usr/lib"
  for f in libvulkan.so.1 libzstd.so.1; do
    if readelf --dyn-syms -W "$f" | awk '$7=="UND"{print $8}' | grep -qx '__stack_chk_guard'; then
      patchelf --add-needed libleandros_ssp.so.1 "$f"; echo "ssp shim: $f"
    fi
  done
  for f in $(find . ../bin -type f); do
    if file "$f" | grep -q ELF; then
      patchelf --replace-needed "libc.musl-$ARCH.so.1" libc.so "$f" 2>/dev/null || true
    fi
  done
  echo "== gallium drivers in the megadriver =="
  for d in zink virgl softpipe v3d iris; do
    printf '%s: %s\n' "$d" "$(strings libgallium-25.3.6.so | grep -c "^${d}_\|${d}_create_screen\|pipe_${d}_create_screen")"
  done
  echo "== ICD =="; ls -l libvulkan_virtio.so libvulkan.so.1
  cat "$S/usr/share/vulkan/icd.d/"virtio_icd*.json
  NVKLIB=
  case "$VKDRV" in *nouveau*)
    ls -l libvulkan_nouveau.so
    cat "$S/usr/share/vulkan/icd.d/"nouveau_icd*.json
    NVKLIB=libvulkan_nouveau.so ;;
  esac
  if [ "$INTEL" = 1 ]; then
    ls -l libvulkan_intel.so
    cat "$S/usr/share/vulkan/icd.d/"intel_icd*.json
    NVKLIB="$NVKLIB libvulkan_intel.so"
  fi
  echo "== NEEDED (musl soname must be gone) =="
  for f in libgallium-25.3.6.so libEGL.so.1.0.0 libGLESv2.so.2.0.0 libgbm.so.1.0.0 gbm/dri_gbm.so libvulkan_virtio.so $NVKLIB libvulkan.so.1 libzstd.so.1 libleandros_ssp.so.1; do
    printf '%s: ' "$f"; readelf -d "$f" | awk '/NEEDED/{printf "%s ", $5} END{print ""}'
  done
  if readelf -d libgallium-25.3.6.so libvulkan_virtio.so $NVKLIB libvulkan.so.1 | grep -q 'libc.musl'; then
    echo "musl soname still present"; exit 4
  fi
  # No toolchain-only library may leak into the ship-set's NEEDED lists.
  if readelf -d libgallium-25.3.6.so libvulkan_virtio.so $NVKLIB | grep -E 'NEEDED.*(libLLVM|libclang|libSPIRV|libLLVMSPIRV)'; then
    echo "toolchain library leaked into NEEDED"; exit 6
  fi
  # Every shipped ELF that imports __stack_chk_guard must be able to find it.
  for f in $(find . ../bin -type f); do
    file "$f" | grep -q ELF || continue
    if readelf --dyn-syms -W "$f" | awk '$7=="UND"{print $8}' | grep -qx '__stack_chk_guard'; then
      readelf -d "$f" | grep -q 'libleandros_ssp' || { echo "unresolved __stack_chk_guard: $f"; exit 5; }
    fi
  done
  # Never copy anything but a finished stage: an empty $S here once made
  # `cp -a "$S/."` copy the container's whole root (with /proc) into /out.
  [ -n "$S" ] && [ "$S" != / ] && [ -f "$S/usr/lib/libgallium-25.3.6.so" ] && [ -x "$S/usr/bin/gpuprobe" ]
  rm -rf "/out/$DEST"
  mkdir -p "/out/$DEST"
  cp -a "$S/usr" "/out/$DEST/"
)
echo "=== rc=$? arch=$ARCH ==="
