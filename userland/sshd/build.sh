#!/usr/bin/env bash
# Build sshd, the LeandrOS SSH server (russh) -- static musl, one arch per
# invocation: ./build.sh aarch64 | ./build.sh x86_64
#
# Mirrors userland/claude-run/build.sh (pinned nightly, zig-cc linker wrapper,
# link-self-contained=no, relocation-model=static => ET_EXEC), plus the cc/ar
# wrappers for ring's C and assembly (as build_coreutils does for blake3):
# cc-rs would otherwise reach for the host compiler and the macOS ar.
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
cc_var="CC_${target_triple//-/_}"
ar_var="AR_${target_triple//-/_}"

echo "Building sshd for $arch ($target_triple)..."
(
    cd "$HERE"
    env "$cc_var=$ROOT_DIR/scripts/cc-$arch-musl.sh" \
        "$ar_var=$ROOT_DIR/scripts/ar-musl.sh" \
    RUSTFLAGS="-C linker=$ROOT_DIR/scripts/linker-$arch-musl.sh -C link-self-contained=no -C relocation-model=static" \
        cargo "$toolchain" build --target "$target_triple" --release
)
echo "built: $HERE/target/$target_triple/release/sshd"
