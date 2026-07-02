#!/usr/bin/env bash
# Build Leandros user-space programs.

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

CARGO_ARGS=(--target "$TARGET" --manifest-path userland/Cargo.toml --workspace)

if [[ "$MODE" == "release" ]]; then
    CARGO_ARGS+=(--release)
fi

if $CHECK; then
    echo "[userland] cargo check …"
    cargo check "${CARGO_ARGS[@]}" --exclude pthreadtest
    
    if [[ "$TARGET" == "aarch64-unknown-none" ]]; then
        LEANDROS_TARGET="targets/aarch64-unknown-leandros.json"
    else
        LEANDROS_TARGET="targets/x86_64-unknown-leandros.json"
    fi
    cargo +nightly check --manifest-path userland/Cargo.toml -p pthreadtest --target "$LEANDROS_TARGET" -Z build-std=core,alloc -Zjson-target-spec
    
    echo "[userland] OK — type-check passed"
    exit 0
fi

echo "[userland] cargo build …"
RUSTFLAGS="-C link-arg=--entry=_start -C link-arg=-static -C linker=rust-lld -C relocation-model=static" \
cargo build "${CARGO_ARGS[@]}" --exclude pthreadtest

echo "[userland] Building pthreadtest..."
if [[ "$TARGET" == "aarch64-unknown-none" ]]; then
    LEANDROS_TARGET="targets/aarch64-unknown-leandros.json"
    LEANDROS_TARGET_NAME="aarch64-unknown-leandros"
else
    LEANDROS_TARGET="targets/x86_64-unknown-leandros.json"
    LEANDROS_TARGET_NAME="x86_64-unknown-leandros"
fi

RUSTFLAGS="-C link-arg=--entry=_start -C link-arg=-static -C linker=rust-lld -C relocation-model=static" \
cargo +nightly build --manifest-path userland/Cargo.toml -p pthreadtest --target "$LEANDROS_TARGET" -Z build-std=core,alloc -Zjson-target-spec --release

# Copy output to where build-all.sh expects it
OUT="userland/target/${TARGET}/${MODE}"
mkdir -p "$OUT"
cp "userland/target/${LEANDROS_TARGET_NAME}/release/pthreadtest" "${OUT}/pthreadtest"

echo ""
echo "[userland] Build complete in ${OUT}"

