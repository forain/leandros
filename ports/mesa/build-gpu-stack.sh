#!/bin/sh
# build-gpu-stack.sh <x86_64|aarch64> — build the GPU Mesa ship-set
# (zink+virgl+softpipe[+v3d] megadriver, Venus ICD, Vulkan loader) in an Alpine
# container of the target architecture and write it to
#   ~/code/leandros-artifacts/m3-gl-stack/gpu-stage-<arch>/
# which scripts/mkfs-f2fs-populated.py prefers over the softpipe-era sysroot.
#
# The container must run NATIVELY (the build is ~2-5 min native, hours under
# qemu-user): x86_64 on the linux desktop/laptop (podman), aarch64 on the Mac
# (Docker Desktop, arm64 Linux VM). Mesa source defaults to the 25.3.6 tree the
# virgl/zink lanes used; override with MESA_SRC=.
set -eu
ARCH="${1:?usage: $0 <x86_64|aarch64>}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ART="${LEANDROS_ARTIFACTS:-$HOME/code/leandros-artifacts}"
MESA_SRC="${MESA_SRC:-$ART/llvmpipe-lane/src/mesa}"
OUT="$ART/m3-gl-stack"
[ -f "$MESA_SRC/VERSION" ] || { echo "❌ no Mesa source at $MESA_SRC (set MESA_SRC)"; exit 1; }
# macOS tar leaves AppleDouble ._* sidecars that meson's *.wrap glob chokes on.
find "$MESA_SRC" -name '._*' -delete 2>/dev/null || true
if command -v podman >/dev/null 2>&1; then CT=podman
elif command -v docker >/dev/null 2>&1; then CT=docker
else echo "❌ need podman or docker"; exit 1; fi
case "$ARCH" in aarch64) PLAT=linux/arm64 ;; x86_64) PLAT=linux/amd64 ;; *) exit 2 ;; esac
mkdir -p "$OUT"
LOG="$OUT/gpu-stage-$ARCH.log"
echo "building GPU Mesa for $ARCH with $CT (log: $LOG)"
# GPU_BUILD_TMP: host dir for the container's /tmp (the ~2 GB build tree) when
# the container storage lives on a nearly-full root filesystem.
TMPMNT=""
if [ -n "${GPU_BUILD_TMP:-}" ]; then mkdir -p "$GPU_BUILD_TMP"; TMPMNT="-v $GPU_BUILD_TMP:/tmp"; fi
# shellcheck disable=SC2086
"$CT" run --rm --platform "$PLAT" $TMPMNT \
    -v "$MESA_SRC:/work/mesa" -v "$HERE:/src:ro" -v "$OUT:/out" \
    alpine:3.21 sh /src/build-gpu-stack-alpine.sh "$ARCH" >"$LOG" 2>&1 || true
tail -30 "$LOG"
tail -1 "$LOG" | grep -q '=== rc=0 ' || { echo "❌ build failed (see $LOG)"; exit 1; }
echo "✅ $OUT/gpu-stage-$ARCH"
