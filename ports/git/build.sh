#!/bin/sh
# build.sh <x86_64|aarch64|all> -- git (and less, nano, sftp-server) for LeandrOS.
#
#   1. build-in-alpine.sh (Alpine 3.21 of the target arch: podman/docker on the
#      host, a chroot staged with apk.static inside LeandrOS): Alpine's
#      prebuilt git 2.47 + its DT_NEEDED closure, ELF-fixed for the guest
#      (libc soname, the __stack_chk_guard shim) -> out/<arch>.alpine/.
#   2. Assemble out/<arch>/: the rootfs-shaped tree scripts/mkfs-f2fs-populated.py
#      overlays onto the image (same rules as ports/pipewire), plus
#      /etc/gitconfig from data/.
# Per-arch log: out/<arch>.log ('=== rc=N arch=A ===' last).
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

stage_alpine() {
  ARCH=$1
  . "$ROOT/scripts/container-lib.sh"
  leandros_pick_alpine || { echo "no Alpine backend: $ALPINE_WHY"; return 1; }
  SNAP=$(mktemp -d "${TMPDIR:-/tmp}/git-port-src.XXXXXX")
  cp "$HERE/build-in-alpine.sh" "$ROOT/ports/mesa/ssp_guard.c" "$SNAP/"
  leandros_alpine_run --arch "$ARCH" -v "$SNAP:/src:ro" -v "$OUT:/out" \
      -- sh /src/build-in-alpine.sh "$ARCH" > "$OUT/$ARCH.alpine.log" 2>&1 || true
  rm -rf "$SNAP"
  tail -1 "$OUT/$ARCH.alpine.log" | grep -q '=== rc=0 '
}

rc=0
for ARCH in $ARCHS; do
  LOG="$OUT/$ARCH.log"
  (
    set -e
    echo "== $ARCH: Alpine git + less + nano + sftp-server =="
    stage_alpine "$ARCH" || { echo "Alpine staging failed (see $OUT/$ARCH.alpine.log)"; exit 1; }
    grep -E '^(package|unresolved)' "$OUT/$ARCH.alpine.log" || true
    T="$OUT/$ARCH"
    rm -rf "$T.new"
    cp -a "$OUT/$ARCH.alpine" "$T.new"
    mkdir -p "$T.new/etc"
    cp "$HERE/data/gitconfig" "$T.new/etc/gitconfig"
    chmod -R a+rX "$T.new"
    rm -rf "$T"; mv "$T.new" "$T"
    touch "$T/.stamp"
    echo "staged $T ($(du -sh "$T" | cut -f1))"
  ) > "$LOG" 2>&1 || true
  tail -4 "$LOG"
  if tail -1 "$LOG" | grep -q '^staged '; then
    echo "=== rc=0 arch=$ARCH ===" | tee -a "$LOG"
  else
    echo "=== rc=1 arch=$ARCH ===" | tee -a "$LOG"; rc=1
  fi
done
exit $rc
