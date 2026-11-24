#!/usr/bin/env bash
# Build the sudotest regression test (su + sudo, sudo-rs) -- static musl, one arch per
# invocation: ./build.sh aarch64 | ./build.sh x86_64
#
# Mirrors ports/mkfs-fat/build.sh: the pinned nightly toolchain, the clang/lld
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

echo "Building sudotest for $arch ($target_triple)..."
# Dynamic musl PIE linked against Alpine's real libpam.so.0 (scripts/lib/realpam.sh).
source "$ROOT_DIR/scripts/lib/realpam.sh"
WORK="$HERE/target/linkstub-$arch"
realpam_stub "$arch" "$WORK"
DYN_TC="$ROOT_DIR/scripts/toolchain/dyn" sh "$ROOT_DIR/scripts/toolchain/dyn/gen-cargo-config.sh" "$HERE" >/dev/null
sed -i "s|^rustflags = \[|rustflags = [\n  \"-L\", \"native=$WORK\",|" "$HERE/.cargo/config.toml"
(
    cd "$HERE"
    cargo "$toolchain" build --target "$target_triple" --release
)
echo "built: $HERE/target/$target_triple/release/sudotest"
