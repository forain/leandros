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
# virgl/zink lanes used; override with MESA_SRC=. With NVK the x86_64 build
# also works as a qemu-user container on the aarch64 Mac (~25 min; the alpine
# script works around rustc's scudo abort there). GPU_BUILD_IMAGE=<image>
# replaces alpine:3.21 with a pre-baked image of the same packages.
#
# INTEL=1 also builds iris (GL) and ANV (libvulkan_intel.so); off by default
# until a build has been verified (lane i915). OUT_SUFFIX names a side output
# dir (see below).
# NVK (NVIDIA Vulkan, nouveau ICD) is built too unless NVK=0. It needs Mesa's
# Rust shader compiler NAK, whose crates are fetched HERE on the host (the
# container then runs --wrap-mode=nodownload against a copy of that cache):
# every crates.io wrap in the Mesa tree is downloaded once into
#   $ART/m3-gl-stack/nvk-cache/crates/<source_filename>
# and verified against the wrap's sha256 (a mismatch aborts). The same cache
# directory holds the bindgen-cli binary the container builds once per arch.
set -eu
ARCH="${1:?usage: $0 <x86_64|aarch64>}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ART="${LEANDROS_ARTIFACTS:-$HOME/code/leandros-artifacts}"
MESA_SRC="${MESA_SRC:-$ART/llvmpipe-lane/src/mesa}"
OUT="$ART/m3-gl-stack"
[ -f "$MESA_SRC/VERSION" ] || { echo "❌ no Mesa source at $MESA_SRC (set MESA_SRC)"; exit 1; }
# macOS tar leaves AppleDouble ._* sidecars that meson's *.wrap glob chokes on.
find "$MESA_SRC" -name '._*' -delete 2>/dev/null || true
# Bounded probe: a hung daemon fails fast instead of hanging the build.
. "$HERE/../../scripts/container-lib.sh"
leandros_pick_container || { echo "❌ no usable container tool: $CT_WHY"; exit 1; }
case "$ARCH" in aarch64) PLAT=linux/arm64 ;; x86_64) PLAT=linux/amd64 ;; *) exit 2 ;; esac
mkdir -p "$OUT"
NVK="${NVK:-1}"
# INTEL=1 adds Mesa's Intel drivers: Gallium iris + ANV (x86_64 targets).
INTEL="${INTEL:-0}"
# OUT_SUFFIX=-intel writes gpu-stage-<arch>-intel (+ .log) instead of the shared dir.
OUT_SUFFIX="${OUT_SUFFIX:-}"
CACHE="$OUT/nvk-cache"
mkdir -p "$CACHE/crates"
if [ "$NVK" = 1 ]; then
  for w in "$MESA_SRC"/subprojects/*-rs.wrap; do
    url=$(sed -n 's/^source_url *= *//p' "$w"); fn=$(sed -n 's/^source_filename *= *//p' "$w")
    want=$(sed -n 's/^source_hash *= *//p' "$w")
    [ -n "$url" ] && [ -n "$fn" ] && [ -n "$want" ] || continue
    if [ ! -f "$CACHE/crates/$fn" ]; then
      echo "fetching crate $fn"
      curl -fsSL --retry 3 -o "$CACHE/crates/$fn.part" "$url" || { echo "❌ cannot fetch $url"; rm -f "$CACHE/crates/$fn.part"; exit 1; }
      mv "$CACHE/crates/$fn.part" "$CACHE/crates/$fn"
    fi
    got=$(shasum -a 256 "$CACHE/crates/$fn" | cut -d' ' -f1)
    [ "$got" = "$want" ] || { echo "❌ sha256 mismatch for $fn (got $got, want $want)"; rm -f "$CACHE/crates/$fn"; exit 1; }
  done
fi
LOG="$OUT/gpu-stage-$ARCH$OUT_SUFFIX.log"
echo "building GPU Mesa for $ARCH with $CT (log: $LOG)"
# Run a private COPY of the container scripts: sh reads a script as it goes,
# so editing the checkout mid-build would otherwise change the running build.
SNAP=$(mktemp -d "${TMPDIR:-/tmp}/gpu-stack-src.XXXXXX")
cp "$HERE/build-gpu-stack-alpine.sh" "$HERE/gpuprobe.c" "$HERE/ssp_guard.c" "$SNAP/"
# LeandrOS Mesa patches (applied to a private copy inside the container; the
# shared Mesa checkout is never modified).
[ -d "$HERE/patches" ] && cp -R "$HERE/patches" "$SNAP/patches"
trap 'rm -rf "$SNAP"' EXIT
# GPU_BUILD_TMP: host dir for the container's /tmp (the ~2 GB build tree) when
# the container storage lives on a nearly-full root filesystem.
TMPMNT=""
if [ -n "${GPU_BUILD_TMP:-}" ]; then mkdir -p "$GPU_BUILD_TMP"; TMPMNT="-v $GPU_BUILD_TMP:/tmp"; fi
# shellcheck disable=SC2086
"$CT" run --rm --platform "$PLAT" $TMPMNT \
    -e NVK="$NVK" -e INTEL="$INTEL" -e OUT_SUFFIX="$OUT_SUFFIX" -v "$CACHE:/cache" \
    -v "$MESA_SRC:/work/mesa" -v "$SNAP:/src:ro" -v "$OUT:/out" \
    "${GPU_BUILD_IMAGE:-alpine:3.21}" sh /src/build-gpu-stack-alpine.sh "$ARCH" "${2:-all}" >"$LOG" 2>&1 || true
tail -30 "$LOG"
tail -1 "$LOG" | grep -q '=== rc=0 ' || { echo "❌ build failed (see $LOG)"; exit 1; }
echo "✅ $OUT/gpu-stage-$ARCH$OUT_SUFFIX"
