#!/bin/sh
# Build wlclient (wl_shm + xdg_shell test client) as a dynamic musl ELF against
# the m3 gl-stack merged sysroot, mirroring build-kmscube.sh. Usage:
#   build-wlclient.sh <x86_64|aarch64>
set -e
D="$HOME/code/leandros-artifacts/m3-gl-stack"
C="$HOME/code/leandros-artifacts/m4-client"
arch="$1"; [ -n "$arch" ] || { echo "usage: build-wlclient.sh <arch>"; exit 2; }
S="$D/sysroot-$arch"
CC="$D/toolchain/$arch-linux-musl-cc"
B="$C/build-$arch"
mkdir -p "$B"

CFLAGS="-O2 -fPIC -std=gnu11 -Wall -Wno-unused-parameter"
INCS="-I$S/usr/include -I$C"

objs=""
for s in xdg-shell-protocol.c wlclient.c; do
  o="$B/${s%.c}.o"
  "$CC" $CFLAGS $INCS -c "$C/$s" -o "$o"
  objs="$objs $o"
done

LIBS="-L$S/usr/lib -lwayland-client -lm"
sh "$D/toolchain/musl-dyn-link.sh" "$arch" exe "$S" "$C/wlclient-$arch" $objs -- $LIBS
echo "BUILT $C/wlclient-$arch"
