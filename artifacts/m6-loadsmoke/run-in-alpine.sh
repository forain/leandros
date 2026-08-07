#!/bin/sh
# Runs INSIDE Alpine 3.21. Load-smokes M6 COSMIC session binaries using ONLY
# our staged musl libc.so (invoked directly as the dynamic loader, musl's
# "libc.so is also ld.so" trick) + our staged runtime libs via
# --library-path. Alpine's own /usr/lib is never consulted for resolution:
# we never set LD_LIBRARY_PATH, invoke our libc.so by full path, and run
# under `env -i` so no inherited env leaks in.
#
# Mount expected: /art -> host ~/code/leandros-artifacts (rw)
# Usage: sh /art/m6-loadsmoke/run-in-alpine.sh <arch>   (arch = x86_64 | aarch64)
set -u
ARCH="$1"
LOADER="/art/m6-loadsmoke/libs-$ARCH/libc.so"
LIBDIR="/art/m6-loadsmoke/libs-$ARCH"
OUTDIR="/art/m6-loadsmoke/results"
mkdir -p "$OUTDIR"
RESFILE="$OUTDIR/raw-$ARCH.txt"
: > "$RESFILE"

BINS="cosmic-session cosmic-panel cosmic-notifications cosmic-bg cosmic-osd cosmic-launcher cosmic-applibrary cosmic-settings cosmic-settings-daemon"

path_for() {
  case "$1" in
    cosmic-settings-daemon) echo "/art/pipewire-gap/out/cosmic-settings-daemon-$ARCH" ;;
    *) echo "/art/m6-session-bins/out/$1-$ARCH" ;;
  esac
}

# run_with_watchdog ENVARGS... -- BIN > outfile ; sets $rc
run_with_watchdog() {
  outfile="$1"; shift
  "$@" > "$outfile" 2>&1 &
  pid=$!
  ( sleep 6; kill -KILL "$pid" 2>/dev/null ) &
  killer=$!
  wait "$pid" 2>/dev/null
  rc=$?
  kill "$killer" 2>/dev/null
  wait "$killer" 2>/dev/null
}

for name in $BINS; do
  bin="$(path_for "$name")"

  echo "##### BEGIN $name / $ARCH / bare-env #####" >> "$RESFILE"
  if [ ! -f "$bin" ]; then
    echo "!! MISSING BINARY: $bin" >> "$RESFILE"
    echo "##### END rc=NOFILE #####" >> "$RESFILE"
    echo "##### BEGIN $name / $ARCH / xdg-runtime-dir #####" >> "$RESFILE"
    echo "!! MISSING BINARY: $bin" >> "$RESFILE"
    echo "##### END rc=NOFILE #####" >> "$RESFILE"
    continue
  fi

  # --- mode 1: bare env (no WAYLAND_DISPLAY, no XDG_RUNTIME_DIR at all) ---
  run_with_watchdog /tmp/o1 env -i PATH=/usr/bin:/bin HOME=/root \
    "$LOADER" --library-path "$LIBDIR" "$bin"
  cat /tmp/o1 >> "$RESFILE"
  echo "##### END rc=$rc #####" >> "$RESFILE"

  # --- mode 2: XDG_RUNTIME_DIR set to a fresh tmpdir, WAYLAND_DISPLAY unset ---
  RTD="/tmp/xdgrt-$$"
  mkdir -p "$RTD"; chmod 700 "$RTD"
  echo "##### BEGIN $name / $ARCH / xdg-runtime-dir #####" >> "$RESFILE"
  run_with_watchdog /tmp/o2 env -i PATH=/usr/bin:/bin HOME=/root XDG_RUNTIME_DIR="$RTD" \
    "$LOADER" --library-path "$LIBDIR" "$bin"
  cat /tmp/o2 >> "$RESFILE"
  echo "##### END rc=$rc #####" >> "$RESFILE"
  rm -rf "$RTD"
done

echo "=== rc=0 arch=$ARCH loadsmoke-complete ==="
