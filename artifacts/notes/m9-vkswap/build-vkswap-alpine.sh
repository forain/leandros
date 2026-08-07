#!/bin/sh
# Runs INSIDE an Alpine 3.21 container. Builds vkswap in the exact idiom of
# venus-lane/build-vkrender-alpine-fixed.sh, minus the shader machinery
# (vkswap has no SPIR-V).
# Mounts: /work/vkheaders (ro, contains only vulkan/), /out (rw, venus-lane).
# Emits '=== rc=N ... ===' as the LAST line — trust that, not the log body.
ARCH="$1"
(
  set -e
  apk add --no-cache build-base clang19 patchelf file
  cc -fPIC -fno-stack-protector -c /out/ssp_guard.c -o /tmp/ssp_guard.o
  mkdir -p /out/stage-$ARCH/usr/bin
  cc /out/vkswap.c -o /out/stage-$ARCH/usr/bin/vkswap \
    -I/work/vkheaders -I/out \
    -std=gnu11 -Wall -Wextra -Wno-unused-parameter -Wno-unused-function \
    -fno-stack-protector -U_FORTIFY_SOURCE -D_FORTIFY_SOURCE=0 \
    -static-libgcc /tmp/ssp_guard.o -ldl
  echo "== pre-patchelf NEEDED (vkswap) =="
  patchelf --print-needed /out/stage-$ARCH/usr/bin/vkswap
  patchelf --replace-needed libc.musl-$ARCH.so.1 libc.so /out/stage-$ARCH/usr/bin/vkswap
  echo "== post-patchelf NEEDED =="
  patchelf --print-needed /out/stage-$ARCH/usr/bin/vkswap
  ls -la /out/stage-$ARCH/usr/bin/vkswap
  file /out/stage-$ARCH/usr/bin/vkswap
)
RC=$?
echo "=== rc=$RC arch=$ARCH vkswap ==="
