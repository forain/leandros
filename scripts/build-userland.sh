#!/usr/bin/env bash
# Build Cyanos user-space programs.

set -euo pipefail
cd "$(dirname "$0")/.."

TARGET="aarch64-unknown-none"
MODE="debug"
CHECK=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --check)   CHECK=true ;;
        --release) MODE="release" ;;
        --target)
            shift
            case "$1" in
                amd64|x86_64) TARGET="x86_64-unknown-none" ;;
                aarch64) TARGET="aarch64-unknown-none" ;;
                *) echo "❌ Invalid target: $1. Use aarch64, x86_64, or amd64"; exit 1 ;;
            esac
            ;;
        *) echo "❌ Unknown option: $1"; exit 1 ;;
    esac
    shift
done

CARGO_ARGS=(--target "$TARGET" --manifest-path userland/Cargo.toml)

if [[ "$MODE" == "release" ]]; then
    CARGO_ARGS+=(--release)
fi

if $CHECK; then
    echo "[userland] cargo check …"
    cargo check "${CARGO_ARGS[@]}"
    echo "[userland] OK — type-check passed"
    exit 0
fi

echo "[userland] cargo build …"
RUSTFLAGS="-C link-arg=--entry=_start -C link-arg=-static -C linker=rust-lld -C relocation-model=static" \
cargo build "${CARGO_ARGS[@]}"

OUT="userland/target/${TARGET}/${MODE}"
echo ""
echo "[userland] Build complete in ${OUT}"
