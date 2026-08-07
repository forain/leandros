#!/bin/sh
# Write .cargo/config.toml for the daemon build. Same dynamic-musl-PIE recipe as
# m6 gen-cargo-config.sh, but adds:
#   -L<pipewire-gap/lib/<arch>>      so ld.lld finds the stub libpipewire-0.3.so.0
#   --error-limit=0                  so a link pass reports ALL undefined syms
#   optional --unresolved-symbols=ignore-all  (PASS1 enumeration) via $UNRES
# Usage: gen-config.sh <arch: x86_64|aarch64> <src-dir> [ignore]
set -e
arch="$1"; src="$2"; mode="$3"
[ -n "$arch" ] && [ -n "$src" ] || { echo "usage: gen-config.sh <arch> <src> [ignore]"; exit 2; }
TC=$HOME/code/leandros-artifacts/m6-session-bins/toolchain
S3=$HOME/code/leandros-artifacts/m3-gl-stack/sysroot-$arch
PG=$HOME/code/leandros-artifacts/pipewire-gap
case "$arch" in
  x86_64)  elfm=elf_x86_64 ;;
  aarch64) elfm=aarch64linux ;;
  *) echo "bad arch"; exit 2 ;;
esac
UNRES=""
[ "$mode" = "ignore" ] && UNRES="--unresolved-symbols=ignore-all "
mkdir -p "$src/.cargo"
cat > "$src/.cargo/config.toml" <<EOF
[target.$arch-unknown-linux-musl]
linker = "$TC/zig-ld-lld"
rustflags = [
  "-C", "linker-flavor=ld",
  "-C", "target-feature=-crt-static",
  "-C", "relocation-model=pic",
  "-C", "link-self-contained=no",
  "-C", "link-args=--sysroot=$S3 --entry _start --build-id=none --eh-frame-hdr -znow --error-limit=0 ${UNRES}-m $elfm --dynamic-linker /lib/ld-musl-$arch.so.1 -pie -L$S3/usr/lib -L$PG/lib/$arch $S3/usr/lib/Scrt1.o $S3/usr/lib/crti.o $S3/usr/lib/crtn.o -lc",
]
EOF
echo "wrote $src/.cargo/config.toml (arch=$arch mode=${mode:-strict})"
