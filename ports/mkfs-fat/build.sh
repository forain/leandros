#!/usr/bin/env bash
# Build the LeandrOS mkfs.fat (dosfstools-CLI-compatible FAT32 formatter) --
# static musl, one arch per invocation: ./build.sh aarch64 | ./build.sh x86_64
#
# Mirrors build_brush in scripts/build-all.sh: the pinned nightly toolchain,
# the zig-cc-based linker wrapper, and link-self-contained=no so rustc's own
# musl CRT objects (not zig's) get linked in. Produces ET_EXEC, statically
# linked, no PT_INTERP, exactly like ports/greetd/build.sh's greetd binary.
#
# Unlike greetd/busd this crate is entirely ours (no upstream source to
# fetch/patch), so there is no $WORK clone step -- it builds directly out of
# ports/mkfs-fat/.
set -euo pipefail

if [[ $# -ne 1 || ( "$1" != "aarch64" && "$1" != "x86_64" ) ]]; then
    echo "usage: $0 <aarch64|x86_64>" >&2
    exit 1
fi
arch="$1"

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$HERE/../.." && pwd)"

target_triple="${arch}-unknown-linux-musl"

echo "Building mkfs.fat for $arch ($target_triple)..."
(
    cd "$HERE"
    RUSTFLAGS="-C linker=$ROOT_DIR/scripts/linker-$arch-musl.sh -C link-self-contained=no" \
        cargo +nightly-2026-04-16 build --target "$target_triple" --release
)

out_dir="$HERE/target/$target_triple/release"
install -m 0755 "$out_dir/mkfs-fat" "$out_dir/mkfs.fat"
echo "built: $out_dir/mkfs.fat"
