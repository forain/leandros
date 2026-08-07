#!/bin/sh
# Runs INSIDE an Alpine 3.21 container (native toolchain, NO zig). Builds
# vktest, a minimal Vulkan smoke-test binary for LeandrOS that dlopen()s the
# Venus ICD (libvulkan_virtio.so) and drives it directly through the
# loader<->ICD interface (LeandrOS ships no Khronos Vulkan loader).
# Mounts:  /work/vkheaders (mesa's include/ dir, so <vulkan/vulkan_core.h>
#          resolves, ro)   /out (artifact lane, rw)
# Emits an explicit '=== rc=N ...' trailer as the LAST line — trust that, not log content.
ARCH="$1"
(
  apk add --no-cache build-base clang19 patchelf file &&
  # Same fixup as every other LeandrOS-targeted binary: LeandrOS musl libc.so
  # lacks __stack_chk_guard, so build with the stack protector off AND link
  # in a local guard symbol (belt-and-braces in case any static/system lib
  # pulled in references it).
  cc -fPIC -fno-stack-protector -c /out/ssp_guard.c -o /tmp/ssp_guard.o &&
  mkdir -p /out/stage-$ARCH/usr/bin &&
  cc /out/vktest.c -o /out/stage-$ARCH/usr/bin/vktest \
    -I/work/vkheaders \
    -fno-stack-protector -U_FORTIFY_SOURCE -D_FORTIFY_SOURCE=0 \
    -static-libgcc /tmp/ssp_guard.o -ldl &&
  echo "== pre-patchelf NEEDED (vktest) ==" &&
  patchelf --print-needed /out/stage-$ARCH/usr/bin/vktest &&
  patchelf --replace-needed libc.musl-$ARCH.so.1 libc.so \
    /out/stage-$ARCH/usr/bin/vktest &&
  echo "== post-patchelf NEEDED ==" &&
  patchelf --print-needed /out/stage-$ARCH/usr/bin/vktest &&
  ls -la /out/stage-$ARCH/usr/bin/vktest &&
  file /out/stage-$ARCH/usr/bin/vktest
)
RC=$?
echo "=== rc=$RC arch=$ARCH vktest ==="
