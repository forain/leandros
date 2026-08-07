#!/bin/sh
# Runs INSIDE an Alpine 3.21 container. Builds vkrender in the exact idiom of
# venus-lane/build-vktest-alpine.sh.
#
# CORRECTED vs the original build-vkrender-alpine.sh (2026-08-06, measured):
#   1. If /out/vkrender_spv.h already exists it is USED AS-IS and no in-container
#      shader compiler is sought. The original always probed apk first, so on a
#      host that had already generated the SPIR-V the container could still
#      decide "no compiler" and silently build -DVKRENDER_NO_SHADERS, throwing
#      away a perfectly good header. Alpine 3.21's `glslang` package does NOT
#      ship a `glslangValidator` binary usable this way in the aports index used
#      here, so the pre-generated path is the one that actually works.
#   2. /work/vkheaders is expected to contain ONLY a `vulkan/` subdirectory.
#      Mounting a whole /usr/include there would shadow the container's own musl
#      headers, because -I precedes the system include path.
#
# Mounts:
#   /work/vkheaders  a dir containing vulkan/{vulkan_core.h,vk_icd.h,...}  (ro)
#   /out             the artifact lane holding vkrender.c, shaders/,
#                    ssp_guard.c and (optionally) vkrender_spv.h           (rw)
#
# Emits '=== rc=N ... ===' as the LAST line — trust that, not the log body.
ARCH="$1"
(
  set -e

  apk add --no-cache build-base clang19 patchelf file

  # ── shaders ────────────────────────────────────────────────────────────────
  SHADERS_OK=0
  if [ -s /out/vkrender_spv.h ] && grep -q '0x07230203' /out/vkrender_spv.h; then
    echo "== using pre-generated /out/vkrender_spv.h =="
    grep -c '0x07230203' /out/vkrender_spv.h
    SHADERS_OK=1
  else
    GLSL_TOOL=""
    if apk add --no-cache glslang 2>/dev/null && command -v glslangValidator >/dev/null 2>&1; then
      GLSL_TOOL=glslangValidator
    elif apk add --no-cache shaderc 2>/dev/null && command -v glslc >/dev/null 2>&1; then
      GLSL_TOOL=glslc
    fi
    echo "== in-container shader compiler: ${GLSL_TOOL:-NONE} =="
    mkdir -p /tmp/spv
    if [ "$GLSL_TOOL" = "glslangValidator" ]; then
      glslangValidator -V --vn spv_fill_comp -o /tmp/spv/comp.h /out/shaders/fillpattern.comp
      glslangValidator -V --vn spv_tri_vert  -o /tmp/spv/vert.h /out/shaders/triangle.vert
      glslangValidator -V --vn spv_tri_frag  -o /tmp/spv/frag.h /out/shaders/triangle.frag
      cat /tmp/spv/comp.h /tmp/spv/vert.h /tmp/spv/frag.h > /out/vkrender_spv.h
      SHADERS_OK=1
    elif [ "$GLSL_TOOL" = "glslc" ]; then
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
  fi

  SHADER_FLAGS=""
  if [ "$SHADERS_OK" != "1" ]; then
    echo "!! NO SPIR-V — building subtest 0 only (VKRENDER_NO_SHADERS)"
    SHADER_FLAGS="-DVKRENDER_NO_SHADERS"
  fi

  # ── the binary ─────────────────────────────────────────────────────────────
  cc -fPIC -fno-stack-protector -c /out/ssp_guard.c -o /tmp/ssp_guard.o

  mkdir -p /out/stage-$ARCH/usr/bin
  cc /out/vkrender.c -o /out/stage-$ARCH/usr/bin/vkrender \
    -I/work/vkheaders -I/out \
    $SHADER_FLAGS \
    -std=gnu11 -Wall -Wextra -Wno-unused-parameter \
    -fno-stack-protector -U_FORTIFY_SOURCE -D_FORTIFY_SOURCE=0 \
    -static-libgcc /tmp/ssp_guard.o -ldl

  echo "== pre-patchelf NEEDED (vkrender) =="
  patchelf --print-needed /out/stage-$ARCH/usr/bin/vkrender
  patchelf --replace-needed libc.musl-$ARCH.so.1 libc.so \
    /out/stage-$ARCH/usr/bin/vkrender
  echo "== post-patchelf NEEDED =="
  patchelf --print-needed /out/stage-$ARCH/usr/bin/vkrender
  ls -la /out/stage-$ARCH/usr/bin/vkrender
  file /out/stage-$ARCH/usr/bin/vkrender
)
RC=$?
echo "=== rc=$RC arch=$ARCH vkrender ==="
