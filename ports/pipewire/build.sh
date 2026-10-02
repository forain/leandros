#!/bin/sh
# build.sh <x86_64|aarch64|all> — PipeWire + WirePlumber for LeandrOS.
#
#   1. build-in-alpine.sh (podman/docker, Alpine 3.21 of the target arch):
#      prebuilt pipewire 1.2.7 + wireplumber 0.5 + their closure, ELF-fixed for
#      the guest, plus leandros-snd-sink (and pw-screencast-probe) compiled
#      against pipewire-dev -> out/<arch>.alpine/.
#   2. Assemble out/<arch>/: the rootfs-shaped tree scripts/mkfs-f2fs-populated.py
#      overlays onto the image (same rules as ports/portal and ports/firefox),
#      with the config drop-ins from ports/pipewire/data and a 440 Hz test tone.
#      The real libpipewire-0.3.so.0 here REPLACES the inert stub that
#      cosmic-settings-daemon and the portal were linked against (same 1.2.7 ABI).
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
  if command -v podman >/dev/null 2>&1; then CT=podman
  elif command -v docker >/dev/null 2>&1; then CT=docker
  else echo "need podman or docker"; return 1; fi
  "$CT" info >/dev/null 2>&1 || { echo "$CT is installed but not running"; return 1; }
  case "$ARCH" in aarch64) PLAT=linux/arm64 ;; x86_64) PLAT=linux/amd64 ;; esac
  # LEANDROS_PW_PLATFORM=linux/amd64 builds aarch64 on an x86_64 box with no
  # binfmt emulation (build-in-alpine.sh then installs into a foreign root).
  PLAT="${LEANDROS_PW_PLATFORM:-$PLAT}"
  SNAP=$(mktemp -d "${TMPDIR:-/tmp}/pipewire-port-src.XXXXXX")
  cp "$HERE/build-in-alpine.sh" "$HERE"/*.c "$ROOT/ports/mesa/ssp_guard.c" "$SNAP/"
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

# 10 s of a 440 Hz sine, 48 kHz stereo S16LE, -12 dBFS: the glitch-test signal
# (a pure tone makes every dropout or repeat a measurable phase jump).
make_tone() {
  python3 - "$1" <<'EOF'
import math, struct, sys
rate, secs, f, amp = 48000, 10, 440.0, 0.25
n = rate * secs
pcm = bytearray()
for i in range(n):
    v = int(amp * 32767 * math.sin(2 * math.pi * f * i / rate))
    pcm += struct.pack('<hh', v, v)
hdr = b'RIFF' + struct.pack('<I', 36 + len(pcm)) + b'WAVEfmt ' + struct.pack(
    '<IHHIIHH', 16, 1, 2, rate, rate * 4, 4, 16) + b'data' + struct.pack('<I', len(pcm))
open(sys.argv[1], 'wb').write(hdr + pcm)
EOF
}

rc=0
for ARCH in $ARCHS; do
  LOG="$OUT/$ARCH.log"
  (
    set -e
    echo "== $ARCH: Alpine pipewire + wireplumber + leandros-snd-sink =="
    stage_alpine "$ARCH" || { echo "Alpine staging failed (see $OUT/$ARCH.alpine.log)"; exit 1; }
    grep -E '^(package|unresolved strong|sonames where)' "$OUT/$ARCH.alpine.log" || true
    T="$OUT/$ARCH"
    rm -rf "$T.new"
    cp -a "$OUT/$ARCH.alpine" "$T.new"
    mkdir -p "$T.new/usr/share/pipewire/pipewire.conf.d" \
             "$T.new/usr/share/wireplumber/wireplumber.conf.d" \
             "$T.new/usr/share/sounds/leandros"
    cp "$HERE/data/50-leandros.conf" "$T.new/usr/share/pipewire/pipewire.conf.d/"
    cp "$HERE/data/50-leandros-wireplumber.conf" \
       "$T.new/usr/share/wireplumber/wireplumber.conf.d/50-leandros.conf"
    make_tone "$T.new/usr/share/sounds/leandros/tone-440-10s.wav"
    # cosmic-applet-audio (the panel's Sound applet): unmodified upstream,
    # cross-built from the pinned cosmic-applets tree with the m6 recipe
    # (m6-session-bins/build-rust.sh src/cosmic-applets <arch> -p cosmic-applet-audio).
    # It talks only to cosmic-settings-daemon (varlink), never to PipeWire.
    # LEANDROS_AUDIO_APPLET=<file> uses a prebuilt binary.
    AP="${LEANDROS_AUDIO_APPLET:-$ART/m6-session-bins/src/cosmic-applets/target/$ARCH-unknown-linux-musl/release/cosmic-applet-audio}"
    if [ -x "$AP" ]; then
      mkdir -p "$T.new/usr/share/applications"
      cp "$AP" "$T.new/usr/bin/cosmic-applet-audio"
      cp "$HERE/data/com.system76.CosmicAppletAudio.desktop" "$T.new/usr/share/applications/"
      echo "cosmic-applet-audio: $AP"
    else
      echo "cosmic-applet-audio: not built ($AP), panel Sound applet stays absent"
    fi
    chmod -R a+rX "$T.new/usr/share"
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
