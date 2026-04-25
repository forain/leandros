#!/usr/bin/env bash
# Build Cyanos user-space programs.
#
# Usage:
#   ./scripts/build-userland.sh           # build all userland crates
#   ./scripts/build-userland.sh --check   # type-check only (fast)
#   ./scripts/build-userland.sh --release # optimised build
#
# Output:
#   userland/target/aarch64-linux-android/[debug|release]/
#     libcyanos_libc.a    — static C runtime archive
#     hello               — example ELF binary (static, no dynamic deps)
#
# The resulting ELF can be loaded by the Cyanos ELF loader and run as a
# user-space process.  It makes raw Linux-ABI syscalls (SVC #0) which
# Cyanos's kernel dispatches through its in-kernel VFS/net/TTY servers.

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
# Build shell first so init can embed it
cargo build "${CARGO_ARGS[@]}" -p cyanos-shell \
    --config "target.${TARGET}.rustflags=[\"-C\",\"link-arg=--entry=_start\",\"-C\",\"link-arg=-static\",\"-C\",\"linker=rust-lld\",\"-C\",\"relocation-model=static\"]"

cargo build "${CARGO_ARGS[@]}" \
    --config "target.${TARGET}.rustflags=[\"-C\",\"link-arg=--entry=_start\",\"-C\",\"link-arg=-static\",\"-C\",\"linker=rust-lld\",\"-C\",\"relocation-model=static\"]"

OUT="userland/target/${TARGET}/${MODE}"
echo ""
echo "[userland] Build complete."
echo "  Library : ${OUT}/libcyanos_libc.a"
echo "  Binary  : ${OUT}/hello"
echo ""
echo "  To inspect the ELF:"
echo "    llvm-objdump -d ${OUT}/hello | head -60"
echo "    llvm-readelf -h ${OUT}/hello"
echo ""
echo "  Stage 2 note:"
echo "  When VFS/net servers move to user space, replace cyanos-libc's"
echo "  syscall wrappers with direct IPC calls to server ports stored in"
echo "  TLS (initialised from auxv AT_CYANOS_VFS_PORT / AT_CYANOS_NET_PORT)."
