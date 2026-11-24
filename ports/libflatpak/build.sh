#!/bin/sh
# build.sh <x86_64|aarch64|all> -- libflatpak (Alpine 3.24 flatpak) + closure for
# cosmic-store's `flatpak` feature. build-in-alpine.sh (Alpine 3.24 container)
# stages out/<arch>.alpine/ (the image tree, overlaid by scripts/mkfs-f2fs-populated.py)
# and out/<arch>.sysroot/ (-dev files merged into the cross sysroot
# m3-gl-stack/sysroot-<arch> by this script, no clobber).
set -eu
WHAT="${1:?usage: $0 <x86_64|aarch64|all>}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
OUT="$HERE/out"
case "$WHAT" in
  all) ARCHS="aarch64 x86_64" ;;
  aarch64|x86_64) ARCHS="$WHAT" ;;
  *) echo "usage: $0 <x86_64|aarch64|all>"; exit 2 ;;
esac
mkdir -p "$OUT"
ART="${LEANDROS_ARTIFACTS:-$HOME/code/leandros-artifacts}"
stage_alpine() {
  ARCH=$1
  . "$ROOT/scripts/container-lib.sh"
  leandros_pick_alpine || { echo "no Alpine backend: $ALPINE_WHY"; return 1; }
  PLAT="${LEANDROS_PW_PLATFORM:-$ARCH}"
  SNAP=$(mktemp -d "${TMPDIR:-/tmp}/libflatpak-port-src.XXXXXX")
  cp "$HERE/build-in-alpine.sh" "$SNAP/"
  : > "$SNAP/image-sonames.txt"
  MNT=""
  LIBC_DIR="$ART/musl-dynamic/sysroot/$ARCH/usr/lib"
  [ -f "$LIBC_DIR/libc.so" ] && MNT="-v $LIBC_DIR:/leandros-libc:ro"
  n=0
  for d in "$ART/m3-gl-stack/gpu-stage-$ARCH/usr/lib" \
           "$ROOT/target/input-stack-sysroot/$ARCH/usr/lib" \
           "$ART/m4-input-ship/$ARCH/usr/lib" \
           "$ART/m3-gl-stack/sysroot-$ARCH/usr/lib"; do
    [ -d "$d" ] || continue
    MNT="$MNT -v $d:/imglib/$n:ro"; n=$((n + 1))
  done
  # Concurrent builds on one host (shared podman storage, mirrors) can lose a
  # container start without a trace (log = only the rc line); retry those.
  _try=1
  while :; do
    # shellcheck disable=SC2086
    leandros_alpine_run --arch "$PLAT" $MNT -v "$SNAP:/src:ro" -v "$OUT:/out" \
        -- sh /src/build-in-alpine.sh "$ARCH" > "$OUT/$ARCH.alpine.log" 2>&1 || true
    tail -1 "$OUT/$ARCH.alpine.log" | grep -q '=== rc=0 ' && break
    [ "$_try" -ge 3 ] && break
    echo "libflatpak $ARCH: staging attempt $_try failed, retrying" >&2
    cp "$OUT/$ARCH.alpine.log" "$OUT/$ARCH.alpine.log.try$_try" 2>/dev/null || true
    sleep $((_try * 5 + $$ % 7)); _try=$((_try + 1))
  done
  rm -rf "$SNAP"
  tail -1 "$OUT/$ARCH.alpine.log" | grep -q '=== rc=0 '
}

rc=0
for ARCH in $ARCHS; do
  LOG="$OUT/$ARCH.log"
  (
    set -e
    stage_alpine "$ARCH" || { echo "Alpine staging failed (see $OUT/$ARCH.alpine.log)"; exit 1; }
    grep -E '^(package|unresolved strong|sonames where|sysroot dev)' "$OUT/$ARCH.alpine.log" || true
    T="$OUT/$ARCH"; rm -rf "$T.new"; cp -a "$OUT/$ARCH.alpine" "$T.new"
    chmod -R a+rX "$T.new/usr"
    rm -rf "$T"; mv "$T.new" "$T"; touch "$T/.stamp"
    SYS="$ART/m3-gl-stack/sysroot-$ARCH"
    if [ -d "$SYS" ]; then leandros_sysroot_merge "$OUT/$ARCH.sysroot" "$SYS"; echo "sysroot merged into $SYS"; fi
    echo "staged $T ($(du -sh "$T" | cut -f1))"
  ) > "$LOG" 2>&1 || true
  tail -4 "$LOG"
  if tail -1 "$LOG" | grep -q '^staged '; then echo "=== rc=0 arch=$ARCH ===" | tee -a "$LOG"; else echo "=== rc=1 arch=$ARCH ===" | tee -a "$LOG"; rc=1; fi
done
exit $rc
