#!/bin/sh
# build.sh <x86_64|aarch64|all> — build /usr/bin/nvktest (and nvkmstest, the
# KMS test) in an Alpine 3.21
# container of the target architecture (venus-lane vkrender idiom: native cc,
# -fno-stack-protector, a local __stack_chk_guard, -ldl, then patchelf the musl
# soname to LeandrOS's libc.so). Output:
#   ${LEANDROS_ARTIFACTS:-~/code/leandros-artifacts}/nvk-tests/stage-<arch>/usr/bin/nvktest
# which scripts/mkfs-f2fs-populated.py stages as /bin/nvktest.
#
# The SPIR-V is pre-generated (nvktest_spv.h, see gen-spv.sh), so no shader
# compiler is needed. Vulkan headers come from the Mesa tree the GPU stack is
# built from (MESA_SRC, default $ART/llvmpipe-lane/src/mesa); only its
# include/vulkan and include/vk_video are mounted, so they cannot shadow the
# container's own libc headers. The x86_64 build runs as an emulated amd64
# container on an arm64 host — fine for a single C file.
#
# GPU_BUILD_IMAGE=<image> replaces alpine:3.21 (same as ports/mesa).
# Each arch ends with '=== rc=N arch=A nvktest ===' — trust that line.
#
# Internal: `build.sh --in-container <arch>` is what runs inside the container.
set -eu

if [ "${1:-}" = "--in-container" ]; then
  ARCH="$2"
  set +e   # the subshell fails fast; the rc line below must still print
  (
    set -e
    apk add --no-cache build-base patchelf file >/dev/null
    grep -c 0x07230203 /src/nvktest_spv.h
    cc -fPIC -fno-stack-protector -c /src/ssp_guard.c -o /tmp/ssp_guard.o
    mkdir -p /out/stage-$ARCH/usr/bin
    cc -O2 /src/nvktest.c -o /out/stage-$ARCH/usr/bin/nvktest \
      -I/vkheaders -I/src \
      -std=gnu11 -Wall -Wextra -Wno-unused-parameter -Wno-missing-field-initializers \
      -fno-stack-protector -U_FORTIFY_SOURCE -D_FORTIFY_SOURCE=0 \
      -static-libgcc /tmp/ssp_guard.o -ldl
    # nvkmstest: the KMS test (lane nvkms); raw DRM ioctls, Vulkan headers
    # for the optional NVK-rendered buffers.
    cc -O2 /src/nvkmstest.c -o /out/stage-$ARCH/usr/bin/nvkmstest \
      -I/vkheaders -I/src \
      -std=gnu11 -Wall -Wextra -Wno-unused-parameter -Wno-missing-field-initializers \
      -fno-stack-protector -U_FORTIFY_SOURCE -D_FORTIFY_SOURCE=0 \
      -static-libgcc /tmp/ssp_guard.o -ldl
    for b in nvktest nvkmstest; do
      strip /out/stage-$ARCH/usr/bin/$b
      echo "== pre-patchelf NEEDED ($b) =="
      patchelf --print-needed /out/stage-$ARCH/usr/bin/$b
      patchelf --replace-needed libc.musl-$ARCH.so.1 libc.so /out/stage-$ARCH/usr/bin/$b
      echo "== post-patchelf NEEDED ($b) =="
      patchelf --print-needed /out/stage-$ARCH/usr/bin/$b
      patchelf --print-interpreter /out/stage-$ARCH/usr/bin/$b
      ls -la /out/stage-$ARCH/usr/bin/$b
      file /out/stage-$ARCH/usr/bin/$b
    done
  )
  RC=$?
  echo "=== rc=$RC arch=$ARCH nvktest ==="
  exit $RC
fi

WHAT="${1:?usage: $0 <x86_64|aarch64|all>}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ART="${LEANDROS_ARTIFACTS:-$HOME/code/leandros-artifacts}"
MESA_SRC="${MESA_SRC:-$ART/llvmpipe-lane/src/mesa}"
# NVK_TESTS_OUT: build somewhere other than the shared artifacts tree (a lane
# testing its own test tools must not swap the binaries other checkouts ship;
# scripts/mkfs-f2fs-populated.py reads the same variable).
OUT="${NVK_TESTS_OUT:-$ART/nvk-tests}"
[ -f "$MESA_SRC/include/vulkan/vulkan_core.h" ] || {
  echo "no Vulkan headers at $MESA_SRC/include/vulkan (set MESA_SRC)"; exit 1; }
. "$HERE/../../scripts/container-lib.sh"
leandros_pick_container || { echo "no usable container tool: $CT_WHY"; exit 1; }
mkdir -p "$OUT"

# Private copy of the sources: editing the checkout mid-build cannot change it.
SNAP=$(mktemp -d "${TMPDIR:-/tmp}/nvk-tests-src.XXXXXX")
trap 'rm -rf "$SNAP"' EXIT
cp "$HERE/build.sh" "$HERE/nvktest.c" "$HERE/nvkmstest.c" "$HERE/nvktest_spv.h" "$HERE/ssp_guard.c" "$SNAP/"

build_one() {
  _arch=$1
  case "$_arch" in aarch64) _plat=linux/arm64 ;; x86_64) _plat=linux/amd64 ;;
    *) echo "unknown arch $_arch"; return 2 ;; esac
  _log="$OUT/build-$_arch.log"
  echo "building nvktest for $_arch with $CT ($_plat, log: $_log)"
  "$CT" run --rm --platform "$_plat" \
      -v "$SNAP:/src:ro" \
      -v "$MESA_SRC/include/vulkan:/vkheaders/vulkan:ro" \
      -v "$MESA_SRC/include/vk_video:/vkheaders/vk_video:ro" \
      -v "$OUT:/out" \
      "${GPU_BUILD_IMAGE:-alpine:3.21}" sh /src/build.sh --in-container "$_arch" >"$_log" 2>&1 || true
  tail -12 "$_log"
  tail -1 "$_log" | grep -q '=== rc=0 ' || { echo "=== rc=1 arch=$_arch nvktest === (see $_log)"; return 1; }
  [ -x "$OUT/stage-$_arch/usr/bin/nvktest" ] || { echo "missing $OUT/stage-$_arch/usr/bin/nvktest"; return 1; }
  [ -x "$OUT/stage-$_arch/usr/bin/nvkmstest" ] || { echo "missing $OUT/stage-$_arch/usr/bin/nvkmstest"; return 1; }
  return 0
}

RC=0
case "$WHAT" in
  all) build_one aarch64 || RC=1; build_one x86_64 || RC=1 ;;
  *)   build_one "$WHAT" || RC=1 ;;
esac
exit $RC
