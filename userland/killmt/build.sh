#!/usr/bin/env bash
# Build the killmt regression test -- static musl, one arch per
# invocation: ./build.sh aarch64 | ./build.sh x86_64
#
# Mirrors ports/mkfs-fat/build.sh: the pinned nightly toolchain, the zig-cc
# linker wrapper, link-self-contained=no so rustc's own musl CRT objects are
# linked in, relocation-model=static so the loader gets an ET_EXEC.
set -euo pipefail

if [[ $# -ne 1 || ( "$1" != "aarch64" && "$1" != "x86_64" ) ]]; then
    echo "usage: $0 <aarch64|x86_64>" >&2
    exit 1
fi
arch="$1"

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$HERE/../.." && pwd)"
toolchain="+$(sed -n 's/^channel = "\(.*\)"/\1/p' "$ROOT_DIR/rust-toolchain.toml" | head -1)"

target_triple="${arch}-unknown-linux-musl"

echo "Building killmt for $arch ($target_triple)..."
(
    cd "$HERE"
    RUSTFLAGS="-C linker=$ROOT_DIR/scripts/linker-$arch-musl.sh -C link-self-contained=no -C relocation-model=static" \
        cargo "$toolchain" build --target "$target_triple" --release
)
echo "built: $HERE/target/$target_triple/release/killmt"
