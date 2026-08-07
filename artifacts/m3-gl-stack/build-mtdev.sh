#!/bin/sh
# mtdev 1.1.6 is autotools-only; libtool on a darwin build host cannot emit a
# Linux ELF .so. Bypass autotools entirely: compile the 5 core .c with zig cc
# -fPIC and link a proper soname'd shared object with zig ld.lld. Install the
# headers + a hand-written mtdev.pc into the merged sysroot. Usage: <arch>
set -e
D=/Users/forain/.claude-forain/jobs/afde2e74/tmp/m3-gl-stack
arch="$1"; [ -n "$arch" ] || { echo "usage: build-mtdev.sh <arch>"; exit 2; }
S=$D/sysroot-$arch
CC=$D/toolchain/$arch-linux-musl-cc
M=$D/src/mtdev-1.1.6
B=$D/build/mtdev-$arch
mkdir -p "$B"
case "$arch" in x86_64) em=elf_x86_64;; aarch64) em=aarch64linux;; esac
objs=""
for s in caps core iobuf match match_four; do
  "$CC" -O2 -fPIC -fno-sanitize=all -I"$M/include" -I"$M/src" -c "$M/src/$s.c" -o "$B/$s.o"
  objs="$objs $B/$s.o"
done
# soname libmtdev.so.1 (libtool version-info 1:0:0)
zig ld.lld --error-limit=0 --sysroot="$S" --eh-frame-hdr -znow -m "$em" -shared \
  -soname libmtdev.so.1 -o "$B/libmtdev.so.1.0.0" \
  "$S/usr/lib/crti.o" $objs "$S/usr/lib/crtn.o" -L"$S/usr/lib" -lc
# install
install -m755 "$B/libmtdev.so.1.0.0" "$S/usr/lib/libmtdev.so.1.0.0"
ln -sf libmtdev.so.1.0.0 "$S/usr/lib/libmtdev.so.1"
ln -sf libmtdev.so.1     "$S/usr/lib/libmtdev.so"
cp "$M/include/mtdev.h" "$M/include/mtdev-mapping.h" "$M/include/mtdev-plumbing.h" "$S/usr/include/"
cat > "$S/usr/lib/pkgconfig/mtdev.pc" <<EOF
prefix=/usr
exec_prefix=\${prefix}
libdir=\${prefix}/lib
includedir=\${prefix}/include

Name: mtdev
Description: Multitouch Protocol Translation Library
Version: 1.1.6
Libs: -L\${libdir} -lmtdev
Cflags: -I\${includedir}
EOF
echo "BUILT libmtdev.so.1 [$arch]"
