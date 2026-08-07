#!/bin/sh
# Compile the libseat + libudev shims into versioned shared objects with the
# correct soname and version script, for both arches, and install into a
# per-arch sysroot (usr/lib + usr/include + usr/lib/pkgconfig).
#
# Toolchain: scripts/linker-<arch>-musl.sh, the same zig-cc musl wrapper
# scripts/build-all.sh already uses to cross-build coreutils/brush/bottom/
# relibc. If `zig` isn't on PATH, skip with a warning instead of failing the
# whole build — matches the "source not found, skipping" idiom build-all.sh
# uses for its other out-of-repo build dependencies.
set -e
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
OUT_ROOT="$ROOT_DIR/target/input-stack-sysroot"

if ! command -v zig >/dev/null 2>&1; then
  echo "⚠️  zig not found on PATH; skipping input-stack shim build (libseat/libudev)"
  exit 0
fi

build_one() {
  arch="$1"; name="$2"; ver="$3"   # e.g. libseat 1 ; libudev 1
  cc="$ROOT_DIR/scripts/linker-$arch-musl.sh"
  src="$SCRIPT_DIR/shims/$name/$name.c"
  hdr="$SCRIPT_DIR/shims/$name/$name.h"
  map="$SCRIPT_DIR/shims/$name/$name.map"
  soname="$name.so.$ver"
  outdir="$OUT_ROOT/$arch/usr/lib"
  incdir="$OUT_ROOT/$arch/usr/include"
  pcdir="$outdir/pkgconfig"
  mkdir -p "$outdir" "$incdir" "$pcdir"
  # Full versioned file: libX.so.1.0.0 ; soname libX.so.1 ; devlink libX.so
  full="$soname.0.0"
  # --version-script must reach the linker as a single =-joined token: split
  # across two argv entries ("-Wl,--version-script" "$map"), clang/zig treat
  # the path as a second, unrelated positional input rather than the script's
  # argument.
  "$cc" -shared -fPIC -O2 -Wall -Wextra -std=c11 \
        -D_GNU_SOURCE \
        -Wl,-soname,"$soname" \
        -Wl,--version-script="$map" \
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
echo "shims built -> $OUT_ROOT"
