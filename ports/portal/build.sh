#!/bin/sh
# build.sh <x86_64|aarch64|all> — org.freedesktop.portal.Desktop for LeandrOS.
#
#   1. build-in-alpine.sh (podman/docker, Alpine 3.21 of the target arch):
#      the xdg-desktop-portal 1.18 frontend + xdg-permission-store + their
#      GLib closure, ELF-fixed for the guest -> out/<arch>.alpine/, and a GLib
#      SDK -> out/sdk-<arch>/. Prebuilt, like ports/firefox: nothing compiles
#      but two stubs, so the foreign arch under emulation is fine.
#   2. build-backend.sh: xdg-desktop-portal-cosmic, cross-built unmodified on
#      the host with the m6-session-bins toolchain -> out/backend-<arch>/.
#      Needs cargo +nightly, zig and ~/code/leandros-artifacts (the Mac, or a
#      box with those); LEANDROS_PORTAL_BACKEND=<file> uses a prebuilt binary.
#   3. Assemble out/<arch>/: the rootfs-shaped tree that
#      scripts/mkfs-f2fs-populated.py overlays onto the image (same rules as
#      ports/firefox), with the D-Bus .service files, cosmic.portal and the
#      portals.conf files from ports/portal/data.
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
ART="${LEANDROS_ARTIFACTS:-$HOME/code/leandros-artifacts}"

stage_alpine() {
  ARCH=$1
  # Bounded probe (scripts/container-lib.sh): never hangs on a dead daemon.
  . "$ROOT/scripts/container-lib.sh"
  leandros_pick_container || { echo "no usable container tool: $CT_WHY"; return 1; }
  case "$ARCH" in aarch64) PLAT=linux/arm64 ;; x86_64) PLAT=linux/amd64 ;; esac
  SNAP=$(mktemp -d "${TMPDIR:-/tmp}/portal-port-src.XXXXXX")
  cp "$HERE/build-in-alpine.sh" "$HERE/pw-stub-symbols.txt" "$ROOT/ports/mesa/ssp_guard.c" "$SNAP/"
  # sonames the image packs itself (keep in sync with ports/firefox/build.sh)
  sed -n '/^cat > "\$SNAP\/image-sonames.txt"/,/^EOF/p' "$ROOT/ports/firefox/build.sh" \
    | sed '1d;$d' > "$SNAP/image-sonames.txt"
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
  # shellcheck disable=SC2086
  "$CT" run --rm --platform "$PLAT" $MNT -v "$SNAP:/src:ro" -v "$OUT:/out" \
      alpine:3.21 sh /src/build-in-alpine.sh "$ARCH" > "$OUT/$ARCH.alpine.log" 2>&1 || true
  rm -rf "$SNAP"
  tail -1 "$OUT/$ARCH.alpine.log" | grep -q '=== rc=0 '
}

rc=0
for ARCH in $ARCHS; do
  LOG="$OUT/$ARCH.log"
  (
    set -e
    echo "== $ARCH: Alpine frontend + GLib SDK =="
    stage_alpine "$ARCH" || { echo "Alpine staging failed (see $OUT/$ARCH.alpine.log)"; exit 1; }
    grep -E '^(package|pipewire stub|unresolved strong)' "$OUT/$ARCH.alpine.log" || true
    echo "== $ARCH: xdg-desktop-portal-cosmic =="
    BE="${LEANDROS_PORTAL_BACKEND:-}"
    if [ -z "$BE" ]; then
      "$HERE/build-backend.sh" "$ARCH"
      BE="$OUT/backend-$ARCH/xdg-desktop-portal-cosmic"
    fi
    [ -x "$BE" ] || { echo "no backend binary $BE"; exit 1; }
    T="$OUT/$ARCH"
    rm -rf "$T.new"
    cp -a "$OUT/$ARCH.alpine" "$T.new"
    cp "$BE" "$T.new/usr/libexec/xdg-desktop-portal-cosmic"
    chmod 0755 "$T.new/usr/libexec/xdg-desktop-portal-cosmic"
    mkdir -p "$T.new/usr/share/dbus-1/services" "$T.new/usr/share/xdg-desktop-portal/portals"
    cp "$HERE"/data/*.service "$T.new/usr/share/dbus-1/services/"
    cp "$HERE/data/cosmic.portal" "$T.new/usr/share/xdg-desktop-portal/portals/"
    cp "$HERE/data/cosmic-portals.conf" "$HERE/data/portals.conf" "$T.new/usr/share/xdg-desktop-portal/"
    chmod 0644 "$T.new"/usr/share/dbus-1/services/* "$T.new"/usr/share/xdg-desktop-portal/*.conf \
               "$T.new"/usr/share/xdg-desktop-portal/portals/*
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
