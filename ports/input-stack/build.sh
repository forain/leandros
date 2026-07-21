#!/bin/sh
# D3 input-stack cross-build driver.
# Usage: build.sh <component> <arch>
#   component: pixman | libdisplay-info | libxkbcommon
#   arch:      x86_64 | aarch64
# Proven env from S3: bison>=3 ahead of PATH; homebrew python for meson.
set -e
D=/Users/forain/.claude-forain/jobs/afde2e74/tmp/d3-input-stack
comp="$1"; arch="$2"
[ -n "$comp" ] && [ -n "$arch" ] || { echo "usage: build.sh <comp> <arch>"; exit 2; }

export PATH="/opt/homebrew/opt/bison/bin:$PATH"
# native hwdata.pc (pnp.ids) for libdisplay-info build-time codegen
export PKG_CONFIG_PATH="$D/ref/hwdata:$PKG_CONFIG_PATH"

cross="$D/cross-musl-$arch.ini"
dest="$D/sysroot/$arch"
log="$D/logs/${comp}-${arch}.log"
mkdir -p "$D/logs" "$dest"

case "$comp" in
  pixman)          srcdir="$D/src/pixman-pixman-0.44.2"
                   opts="-Dtests=disabled -Ddemos=disabled -Dgtk=disabled -Dlibpng=disabled -Dgnu-inline-asm=disabled" ;;
  libdisplay-info) srcdir="$D/src/libdisplay-info-0.3.0"
                   opts="" ;;
  libxkbcommon)    srcdir="$D/src/libxkbcommon-xkbcommon-1.8.0"
                   opts="-Denable-x11=false -Denable-wayland=false -Denable-docs=false -Denable-xkbregistry=false -Denable-bash-completion=false -Denable-tools=false" ;;
  *) echo "unknown component $comp"; exit 2 ;;
esac

builddir="$D/build/${comp}-${arch}"
rm -rf "$builddir"
{
  echo "==== $(date) configure $comp $arch ===="
  meson setup "$builddir" "$srcdir" \
     --cross-file "$cross" \
     --prefix=/usr --libdir=lib --buildtype=release \
     --default-library=shared \
     $opts
  echo "==== compile ===="
  ninja -C "$builddir"
  echo "==== install (DESTDIR=$dest) ===="
  DESTDIR="$dest" ninja -C "$builddir" install
  echo "==== DONE $comp $arch rc=0 ===="
} >"$log" 2>&1
echo "FINISHED $comp $arch -> see $log"
