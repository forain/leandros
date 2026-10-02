#!/bin/sh
# build.sh <x86_64|aarch64|all> — stage Alpine's prebuilt Firefox (plus its
# GTK3 closure and fonts) for LeandrOS into ports/firefox/out/<arch>/, which
# scripts/mkfs-f2fs-populated.py overlays onto the image when it exists.
#
# Gecko is NOT compiled: the container just `apk add`s firefox from Alpine 3.21
# (the release the GPU Mesa stack is built on) and ports/firefox/
# build-in-alpine.sh rewrites the ELFs for the guest. Because nothing compiles,
# running the foreign architecture under emulation is fine (a few minutes of
# apk + patchelf), so both arches build on either the Mac or the linux boxes.
#
# Picks podman, else docker, like ports/mesa/build-gpu-stack.sh. Per-arch log:
# ports/firefox/out/<arch>.log, whose LAST line is '=== rc=N arch=A ==='.
set -eu
WHAT="${1:?usage: $0 <x86_64|aarch64|all>}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
OUT="$HERE/out"
if command -v podman >/dev/null 2>&1; then CT=podman
elif command -v docker >/dev/null 2>&1; then CT=docker
else echo "❌ need podman or docker"; exit 1; fi
"$CT" info >/dev/null 2>&1 || { echo "❌ $CT is installed but its daemon/machine is not running"; exit 1; }

case "$WHAT" in
  all) ARCHS="aarch64 x86_64" ;;
  aarch64|x86_64) ARCHS="$WHAT" ;;
  *) echo "usage: $0 <x86_64|aarch64|all>"; exit 2 ;;
esac

mkdir -p "$OUT"
# Run a private COPY of the container script: sh reads a script as it goes,
# so editing the checkout mid-build would otherwise change the running build.
SNAP=$(mktemp -d "${TMPDIR:-/tmp}/firefox-port-src.XXXXXX")
trap 'rm -rf "$SNAP"' EXIT
cp "$HERE/build-in-alpine.sh" "$HERE/icontrace.c" "$HERE/icons.txt" "$ROOT/ports/mesa/ssp_guard.c" "$SNAP/"
# The sonames scripts/mkfs-f2fs-populated.py packs into /usr/lib on its own
# (its usr_lib_files list). A staged library with one of these names is
# dropped at image time and the image's copy is loaded instead, so the
# container's symbol audit must check against that copy. Keep in sync.
cat > "$SNAP/image-sonames.txt" <<'EOF'
libc.so
libEGL.so.1
libGLESv2.so.2
libgbm.so.1
libdrm.so.2
libgallium-25.3.6.so
libexpat.so.1
libz.so.1
libzstd.so.1
libstdc++.so.6
libgcc_s.so.1
libvulkan.so.1
libleandros_ssp.so.1
libwayland-client.so.0
libwayland-server.so.0
libwayland-egl.so.1
libffi.so.8
libpam.so.0
libxkbcommon.so.0
libdisplay-info.so.3
libseat.so.1
libudev.so.1
libinput.so.10
libpixman-1.so.0
libmtdev.so.1
libevdev.so.2
libvulkan_virtio.so
libpipewire-0.3.so.0
EOF

rc=0
for ARCH in $ARCHS; do
  case "$ARCH" in aarch64) PLAT=linux/arm64 ;; x86_64) PLAT=linux/amd64 ;; esac
  LOG="$OUT/$ARCH.log"
  # The guest's libc.so, for the unresolved-symbol audit (optional).
  LIBC_DIR="${LEANDROS_ARTIFACTS:-$HOME/code/leandros-artifacts}/musl-dynamic/sysroot/$ARCH/usr/lib"
  LIBCMNT=""
  [ -f "$LIBC_DIR/libc.so" ] && LIBCMNT="-v $LIBC_DIR:/leandros-libc:ro"
  # The image's own /usr/lib sources, in the order mkfs-f2fs-populated.py
  # prefers them (GPU ship-set, input shims, input blob, GL sysroot): the
  # audit resolves a clashing soname against the copy the guest will load.
  ART="${LEANDROS_ARTIFACTS:-$HOME/code/leandros-artifacts}"
  n=0
  for d in "$ART/m3-gl-stack/gpu-stage-$ARCH/usr/lib" \
           "$ROOT/target/input-stack-sysroot/$ARCH/usr/lib" \
           "$ART/m4-input-ship/$ARCH/usr/lib" \
           "$ART/m3-gl-stack/sysroot-$ARCH/usr/lib" \
           "$ART/pipewire-gap/lib/$ARCH"; do
    [ -d "$d" ] || continue
    LIBCMNT="$LIBCMNT -v $d:/imglib/$n:ro"; n=$((n + 1))
  done
  echo "staging Firefox for $ARCH with $CT (log: $LOG)"
  # shellcheck disable=SC2086
  "$CT" run --rm --platform "$PLAT" $LIBCMNT \
      -v "$SNAP:/src:ro" -v "$OUT:/out" \
      alpine:3.21 sh /src/build-in-alpine.sh "$ARCH" >"$LOG" 2>&1 || true
  tail -12 "$LOG"
  if tail -1 "$LOG" | grep -q '=== rc=0 '; then
    touch "$OUT/$ARCH/.stamp"
    echo "✅ $OUT/$ARCH ($(du -sh "$OUT/$ARCH" | cut -f1))"
  else
    echo "❌ Firefox staging failed for $ARCH (see $LOG)"
    rc=1
  fi
done
exit $rc
