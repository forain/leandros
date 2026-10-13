#!/bin/sh
# build-i915gltest.sh [x86_64|aarch64|all] — build /bin/i915gltest (lane
# gpuctx: deterministic 3D on the real i915) with `zig cc` against the GPU
# Mesa stage's libEGL/libGLESv2/libgbm. No container needed: zig's musl
# target already names libc.so and /lib/ld-musl-<arch>.so.1 like LeandrOS.
# Output: ${LEANDROS_ARTIFACTS:-~/code/leandros-artifacts}/i915gltest/stage-<arch>/i915gltest
# (scripts/mkfs-f2fs-populated.py stages it as /bin/i915gltest).
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
ART="${LEANDROS_ARTIFACTS:-$HOME/code/leandros-artifacts}"
WHAT="${1:-all}"
build_one() {
  a=$1
  st="$ART/m3-gl-stack/gpu-stage-$a"
  if [ "$a" = x86_64 ] && [ -d "$ART/m3-gl-stack/gpu-stage-x86_64-intel" ]; then st="$ART/m3-gl-stack/gpu-stage-x86_64-intel"; fi
  st="${LEANDROS_GPU_STAGE:-$st}"
  out="$ART/i915gltest/stage-$a"
  mkdir -p "$out"
  zig cc -target "$a-linux-musl" -O2 -std=gnu11 -Wall -Wextra -Wno-unused-parameter \
    -fno-stack-protector -D_FORTIFY_SOURCE=0 -I"$st/usr/include" -L"$st/usr/lib" \
    -Wl,--allow-shlib-undefined -s -o "$out/i915gltest.tmp" "$HERE/i915gltest.c" -lEGL -lGLESv2 -lgbm
  mv "$out/i915gltest.tmp" "$out/i915gltest"
  echo "built $out/i915gltest (stage $st)"
}
case "$WHAT" in
  all) build_one x86_64; build_one aarch64 ;;
  *) build_one "$WHAT" ;;
esac
