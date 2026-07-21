#!/bin/sh
# Cross-build libffi 3.4.x for <arch>-linux-musl into sysroot-<arch>.
# autotools builds objects + static lib fine with zig cc, but libtool on a
# darwin BUILD host cannot emit an ELF .so (it leaves dangling symlinks), so we
# link libffi.so directly from the PIC objects. Usage: build-libffi.sh <arch>
set -e
ARCH="$1"; [ -n "$ARCH" ] || { echo "usage: $0 <x86_64|aarch64>"; exit 2; }
ROOT="$(cd "$(dirname "$0")" && pwd)"
SRC="$ROOT/src/libffi-$ARCH"; D="$ROOT/sysroot-$ARCH"
export CC="$ROOT/toolchain/$ARCH-linux-musl-cc"   CXX="$ROOT/toolchain/$ARCH-linux-musl-c++"
export AR="$ROOT/toolchain/$ARCH-linux-musl-ar"   RANLIB="$ROOT/toolchain/$ARCH-linux-musl-ranlib"
cd "$SRC"
./configure --host="$ARCH-linux-musl" --prefix=/usr --disable-docs --with-pic --enable-shared --enable-static
make -j4
make DESTDIR="$D" install || true   # install trips on the dangling .so symlink; headers/.a/.pc land
PICOBJS=$(find . -path "*/.libs/*.o" | sort)
zig cc -target "$ARCH-linux-musl" -shared -fPIC -Wl,-soname,libffi.so.8 -o libffi.so.8.1.4 $PICOBJS
mkdir -p "$D/usr/lib/pkgconfig" "$D/usr/include"
install -m755 libffi.so.8.1.4 "$D/usr/lib/libffi.so.8.1.4"
ln -sf libffi.so.8.1.4 "$D/usr/lib/libffi.so.8"
ln -sf libffi.so.8.1.4 "$D/usr/lib/libffi.so"
find . -name libffi.a  -path "*/.libs/*" -exec install -m644 {} "$D/usr/lib/libffi.a" \;
find . -name ffi.h        -maxdepth 3     -exec install -m644 {} "$D/usr/include/" \;
find . -name ffitarget.h  -maxdepth 3     -exec install -m644 {} "$D/usr/include/" \;
find . -name libffi.pc                     -exec cp -f {} "$D/usr/lib/pkgconfig/libffi.pc" \;
file "$D/usr/lib/libffi.so.8.1.4"
