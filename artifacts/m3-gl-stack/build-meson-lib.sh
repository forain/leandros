#!/bin/sh
# Generic meson cross-build into the merged m3 sysroot (installs .so + headers + .pc).
# Produces dynamic .so via zig cc (shared-lib path is fine; only *executables* need the
# musl-dyn-link.sh workaround). Usage: build-meson-lib.sh <component> <arch>
set -e
D=/Users/forain/.claude-forain/jobs/afde2e74/tmp/m3-gl-stack
comp="$1"; arch="$2"
[ -n "$comp" ] && [ -n "$arch" ] || { echo "usage: build-meson-lib.sh <comp> <arch>"; exit 2; }
export PATH="/opt/homebrew/opt/bison/bin:$PATH"
cross="$D/cross-musl-$arch.ini"
dest="$D/sysroot-$arch"
log="$D/logs/${comp}-${arch}.log"
mkdir -p "$D/logs"
case "$comp" in
  libevdev) srcdir="$D/src/libevdev-1.13.3"
            opts="-Dtests=disabled -Ddocumentation=disabled" ;;
  libinput) srcdir="$D/src/libinput-1.27.1"
            opts="-Dlibwacom=false -Ddebug-gui=false -Dtests=false -Ddocumentation=false" ;;
  *) echo "unknown component $comp"; exit 2 ;;
esac
builddir="$D/build/${comp}-${arch}"
rm -rf "$builddir"
{
  echo "==== $(date) configure $comp $arch ===="
  meson setup "$builddir" "$srcdir" --cross-file "$cross" \
     --prefix=/usr --libdir=lib --buildtype=release --default-library=shared $opts
  echo "==== compile ===="; ninja -C "$builddir"
  echo "==== install (DESTDIR=$dest) ===="; DESTDIR="$dest" ninja -C "$builddir" install
  echo "==== DONE $comp $arch rc=0 ===="
} >"$log" 2>&1
echo "FINISHED $comp $arch -> $log"
