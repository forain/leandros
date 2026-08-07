#!/bin/sh
# Runs INSIDE an Alpine 3.21 container (native toolchain, NO zig). Builds
# vkrender — the M3 GPU-submission test — in the exact idiom of
# venus-lane/build-vktest-alpine.sh, which produced the currently shipping
# /bin/vktest.
#
# Differences from build-vktest-alpine.sh, and only these:
#   1. `glslang` is added to the apk line, and the three GLSL shaders in
#      shaders/ are compiled to SPIR-V and emitted as a C header of
#      `const uint32_t[]` arrays, so the guest binary has no runtime shader
#      dependency.
#   2. If no shader compiler can be installed, the build still succeeds with
#      -DVKRENDER_NO_SHADERS: subtest 0 (which needs no SPIR-V) is the
#      shippable core and subtests 1 and 2 report SKIP rather than failing.
#
# Mounts (same as the vktest build):
#   /work/vkheaders  Mesa's include/ dir, so <vulkan/vulkan_core.h> and
#                    <vulkan/vk_icd.h> resolve                          (ro)
#   /out             the artifact lane holding vkrender.c, shaders/ and
#                    ssp_guard.c                                        (rw)
#
# Invoke from the host (Docker Desktop; aarch64 is native on an Apple Silicon
# Mac, x86_64 is emulated via qemu-user and is slow — background it):
#
#   ART=$HOME/code/leandros-artifacts
#   cd $ART/m3-vkrender                       # this directory, once copied there
#   cp $ART/venus-lane/ssp_guard.c .          # reuse the vktest guard object
#   docker run --rm --platform linux/arm64 \
#     -v $ART/llvmpipe-lane/src/mesa/include:/work/vkheaders:ro \
#     -v $PWD:/out \
#     alpine:3.21 sh /out/build-vkrender-alpine.sh aarch64
#   docker run --rm --platform linux/amd64 \
#     -v $ART/llvmpipe-lane/src/mesa/include:/work/vkheaders:ro \
#     -v $PWD:/out \
#     alpine:3.21 sh /out/build-vkrender-alpine.sh x86_64
#
# Emits an explicit '=== rc=N ...' trailer as the LAST line — trust that, not
# log content.
ARCH="$1"
(
  set -e

  apk add --no-cache build-base clang19 patchelf file

  # ── shaders ────────────────────────────────────────────────────────────────
  # Alpine's glslang package ships glslangValidator; shaderc ships glslc. Try
  # both, in that order. If neither is installable the build degrades to
  # subtest 0 only rather than failing.
  SHADERS_OK=0
  GLSL_TOOL=""
  if apk add --no-cache glslang 2>/dev/null && command -v glslangValidator >/dev/null 2>&1; then
    GLSL_TOOL=glslangValidator
  elif apk add --no-cache shaderc 2>/dev/null && command -v glslc >/dev/null 2>&1; then
    GLSL_TOOL=glslc
  fi
  echo "== shader compiler: ${GLSL_TOOL:-NONE} =="

  mkdir -p /tmp/spv
  if [ "$GLSL_TOOL" = "glslangValidator" ]; then
    # --vn writes a C header containing `const uint32_t <name>[] = {...};`
    glslangValidator -V --vn spv_fill_comp -o /tmp/spv/comp.h /out/shaders/fillpattern.comp
    glslangValidator -V --vn spv_tri_vert  -o /tmp/spv/vert.h /out/shaders/triangle.vert
    glslangValidator -V --vn spv_tri_frag  -o /tmp/spv/frag.h /out/shaders/triangle.frag
    cat /tmp/spv/comp.h /tmp/spv/vert.h /tmp/spv/frag.h > /out/vkrender_spv.h
    SHADERS_OK=1
  elif [ "$GLSL_TOOL" = "glslc" ]; then
    # glslc has no --vn; -mfmt=c emits a bare braced initializer list, so wrap it.
    glslc -fshader-stage=compute  -o /tmp/spv/comp.inc -mfmt=c /out/shaders/fillpattern.comp
    glslc -fshader-stage=vertex   -o /tmp/spv/vert.inc -mfmt=c /out/shaders/triangle.vert
    glslc -fshader-stage=fragment -o /tmp/spv/frag.inc -mfmt=c /out/shaders/triangle.frag
    {
      printf '#include <stdint.h>\n'
      printf 'const uint32_t spv_fill_comp[] = '; cat /tmp/spv/comp.inc; printf ';\n'
      printf 'const uint32_t spv_tri_vert[]  = '; cat /tmp/spv/vert.inc; printf ';\n'
      printf 'const uint32_t spv_tri_frag[]  = '; cat /tmp/spv/frag.inc; printf ';\n'
    } > /out/vkrender_spv.h
    SHADERS_OK=1
  fi

  SHADER_FLAGS=""
  if [ "$SHADERS_OK" = "1" ]; then
    echo "== generated /out/vkrender_spv.h =="
    # First word of every array must be the SPIR-V magic number 0x07230203.
    grep -c '0x07230203' /out/vkrender_spv.h
    wc -l /out/vkrender_spv.h
  else
    echo "!! NO SHADER COMPILER — building subtest 0 only (VKRENDER_NO_SHADERS)"
    SHADER_FLAGS="-DVKRENDER_NO_SHADERS"
  fi

  # ── the binary ─────────────────────────────────────────────────────────────
  # Same fixup as every other LeandrOS-targeted binary: LeandrOS musl libc.so
  # lacks __stack_chk_guard, so build with the stack protector off AND link in
  # a local guard symbol.
  cc -fPIC -fno-stack-protector -c /out/ssp_guard.c -o /tmp/ssp_guard.o

  mkdir -p /out/stage-$ARCH/usr/bin
  cc /out/vkrender.c -o /out/stage-$ARCH/usr/bin/vkrender \
    -I/work/vkheaders -I/out \
    $SHADER_FLAGS \
    -std=c11 -Wall -Wextra -Wno-unused-parameter \
    -fno-stack-protector -U_FORTIFY_SOURCE -D_FORTIFY_SOURCE=0 \
    -static-libgcc /tmp/ssp_guard.o -ldl

  echo "== pre-patchelf NEEDED (vkrender) =="
  patchelf --print-needed /out/stage-$ARCH/usr/bin/vkrender
  # LeandrOS's musl libc is /lib/libc.so, not Alpine's libc.musl-<arch>.so.1.
  patchelf --replace-needed libc.musl-$ARCH.so.1 libc.so \
    /out/stage-$ARCH/usr/bin/vkrender
  echo "== post-patchelf NEEDED =="
  patchelf --print-needed /out/stage-$ARCH/usr/bin/vkrender
  ls -la /out/stage-$ARCH/usr/bin/vkrender
  file /out/stage-$ARCH/usr/bin/vkrender
)
RC=$?
echo "=== rc=$RC arch=$ARCH vkrender ==="
