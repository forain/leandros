#!/bin/sh
# Compile the libseat + libudev shims into versioned shared objects with the
# correct soname and version script, for both arches, and install into the
# per-arch sysroot (usr/lib + usr/include + usr/lib/pkgconfig).
set -e
D=/Users/forain/.claude-forain/jobs/afde2e74/tmp/d3-input-stack

build_one() {
  arch="$1"; name="$2"; ver="$3"   # e.g. libseat 1 ; libudev 1
  cc="$D/toolchain/$arch-linux-musl-cc"
  src="$D/shims/$name/$name.c"
  hdr="$D/shims/$name/$name.h"
  map="$D/shims/$name/$name.map"
  soname="$name.so.$ver"
  outdir="$D/sysroot/$arch/usr/lib"
  incdir="$D/sysroot/$arch/usr/include"
  pcdir="$outdir/pkgconfig"
  mkdir -p "$outdir" "$incdir" "$pcdir"
  # Full versioned file: libX.so.1.0.0 ; soname libX.so.1 ; devlink libX.so
  full="$soname.0.0"
  "$cc" -shared -fPIC -O2 -Wall -Wextra -std=c11 \
        -D_GNU_SOURCE \
        -Wl,-soname,"$soname" \
        -Wl,--version-script "$map" \
        -o "$outdir/$full" "$src"
  ln -sf "$full" "$outdir/$soname"
  ln -sf "$soname" "$outdir/$name.so"
  cp "$hdr" "$incdir/"
  # minimal pkg-config file so downstream (libinput etc.) finds the shim
  cat > "$pcdir/$name.pc" <<EOF
prefix=/usr
libdir=\${prefix}/lib
includedir=\${prefix}/include

Name: $name
Description: LeandrOS $name ABI shim
Version: ${ver}.0
Libs: -L\${libdir} -l$(echo "$name" | sed 's/^lib//')
Cflags: -I\${includedir}
EOF
  echo "  built $arch/$full  (soname $soname)"
}

for arch in x86_64 aarch64; do
  echo "== $arch =="
  build_one "$arch" libseat 1
  build_one "$arch" libudev 1
done
echo "shims built."
