#!/bin/sh
# Write .cargo/config.toml into <src-dir> wiring the dynamic-musl-PIE link recipe
# (same recipe m3-gl-stack used for anvil/cosmic-comp), pointed at:
#   - toolchain: m6-session-bins/toolchain (our own copy)
#   - sysroot:   m3-gl-stack/sysroot-<arch> (read-only reference, not modified)
# Usage: gen-cargo-config.sh <src-dir>
set -e
src="$1"
[ -n "$src" ] || { echo "usage: gen-cargo-config.sh <src-dir>"; exit 2; }
TC=$HOME/code/leandros-artifacts/m6-session-bins/toolchain
S3=$HOME/code/leandros-artifacts/m3-gl-stack
mkdir -p "$src/.cargo"
cat > "$src/.cargo/config.toml" <<EOF
# Dynamic musl link config for LeandrOS (m6-session-bins). Same recipe as
# m3-gl-stack (zig ld.lld against merged sysroot; -crt-static + -pie => ET_DYN + PT_INTERP).
[target.x86_64-unknown-linux-musl]
linker = "$TC/zig-ld-lld"
rustflags = [
  "-C", "linker-flavor=ld",
  "-C", "target-feature=-crt-static",
  "-C", "relocation-model=pic",
  "-C", "link-self-contained=no",
  "-C", "link-args=--sysroot=$S3/sysroot-x86_64 --entry _start --build-id=none --eh-frame-hdr -znow -m elf_x86_64 --dynamic-linker /lib/ld-musl-x86_64.so.1 -pie -L$S3/sysroot-x86_64/usr/lib $S3/sysroot-x86_64/usr/lib/Scrt1.o $S3/sysroot-x86_64/usr/lib/crti.o $S3/sysroot-x86_64/usr/lib/crtn.o -lc",
]

[target.aarch64-unknown-linux-musl]
linker = "$TC/zig-ld-lld"
rustflags = [
  "-C", "linker-flavor=ld",
  "-C", "target-feature=-crt-static",
  "-C", "relocation-model=pic",
  "-C", "link-self-contained=no",
  "-C", "link-args=--sysroot=$S3/sysroot-aarch64 --entry _start --build-id=none --eh-frame-hdr -znow -m aarch64linux --dynamic-linker /lib/ld-musl-aarch64.so.1 -pie -L$S3/sysroot-aarch64/usr/lib $S3/sysroot-aarch64/usr/lib/Scrt1.o $S3/sysroot-aarch64/usr/lib/crti.o $S3/sysroot-aarch64/usr/lib/crtn.o -lc",
]
EOF
echo "wrote $src/.cargo/config.toml"
