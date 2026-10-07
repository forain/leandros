#!/bin/bash
# Build the $ARCH (default x86_64) standard (Limine) kernel exactly like scripts/build-all.sh /
# scripts/m7z2-kernel-only.sh do, optionally with extra cargo features.
#   ports/openrm/build-kernel.sh            -> default kernel
#   ports/openrm/build-kernel.sh nvidia     -> kernel + OpenRM (needs build.sh first)
# Output: target/final-$ARCH/kernel (+ .sha256 printed).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"
FEATURES="${1:-}"
ARCH="${ARCH:-x86_64}"
SPEC="$ROOT/targets/$ARCH-unknown-kernel.json"
LINKER="$ROOT/linkers/$ARCH.ld"
STD="target/build-$ARCH-standard"
FARG=()
[[ -n "$FEATURES" ]] && FARG=(--features "$FEATURES")
RUSTFLAGS="-C link-arg=-T$LINKER -C link-arg=-z -C link-arg=max-page-size=0x1000 -C link-arg=-z -C link-arg=norelro -Z emit-stack-sizes" \
cargo build -p kernel ${FARG[@]+"${FARG[@]}"} --target "$SPEC" --target-dir "$STD" --release \
    -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec
mkdir -p target/final-$ARCH
cp "$STD/$ARCH-unknown-kernel/release/kernel" target/final-$ARCH/kernel
python3 scripts/check-stack-frames.py target/final-$ARCH/kernel --budget 49152
shasum -a 256 target/final-$ARCH/kernel
ls -l target/final-$ARCH/kernel
