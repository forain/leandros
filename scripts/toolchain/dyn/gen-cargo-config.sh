#!/bin/sh
# Write .cargo/config.toml into <src-dir> wiring the dynamic-musl-PIE link recipe (raw ld.lld against the merged m3 sysroot).
# Usage: gen-cargo-config.sh <src-dir>     (DYN_TC overrides the toolchain dir; default = the $ART m6-session-bins one)
set -e
src="$1"
[ -n "$src" ] || { echo "usage: gen-cargo-config.sh <src-dir>"; exit 2; }
TC=${DYN_TC:-$HOME/code/leandros-artifacts/m6-session-bins/toolchain}
S3=${DYN_S3:-$HOME/code/leandros-artifacts/m3-gl-stack}
mkdir -p "$src/.cargo"
cat > "$src/.cargo/config.toml" <<EOT
# Dynamic musl link config for LeandrOS (scripts/toolchain/dyn/gen-cargo-config.sh): raw ld.lld against the
# merged sysroot; -crt-static + -pie => ET_DYN + PT_INTERP.
[target.x86_64-unknown-linux-musl]
linker = "$TC/ld.lld"
rustflags = [
  "-C", "linker-flavor=ld",
  "-C", "target-feature=-crt-static",
  "-C", "relocation-model=pic",
  "-C", "link-self-contained=no",
  "-C", "link-args=--sysroot=$S3/sysroot-x86_64 --entry _start --build-id=none --eh-frame-hdr -znow -m elf_x86_64 --dynamic-linker /lib/ld-musl-x86_64.so.1 -pie -L$S3/sysroot-x86_64/usr/lib $S3/sysroot-x86_64/usr/lib/Scrt1.o $S3/sysroot-x86_64/usr/lib/crti.o $S3/sysroot-x86_64/usr/lib/crtn.o -lc",
]

[target.aarch64-unknown-linux-musl]
linker = "$TC/ld.lld"
rustflags = [
  "-C", "linker-flavor=ld",
  "-C", "target-feature=-crt-static",
  "-C", "relocation-model=pic",
  "-C", "link-self-contained=no",
  "-C", "link-args=--sysroot=$S3/sysroot-aarch64 --entry _start --build-id=none --eh-frame-hdr -znow -m aarch64linux --dynamic-linker /lib/ld-musl-aarch64.so.1 -pie -L$S3/sysroot-aarch64/usr/lib $S3/sysroot-aarch64/usr/lib/Scrt1.o $S3/sysroot-aarch64/usr/lib/crti.o $S3/sysroot-aarch64/usr/lib/crtn.o -lc",
]
EOT
echo "wrote $src/.cargo/config.toml"
