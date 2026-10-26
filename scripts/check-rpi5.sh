#!/usr/bin/env bash
# Compile-check the kernel's Raspberry Pi 5 variant (aarch64, --features rpi5).
# The rpi5 code is cfg-gated, so a normal build never compiles it and drivers
# such as blkdev_sdhci drift silently. `cargo check` is much cheaper than a
# full build; it uses its own target dir.
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"
spec="$ROOT_DIR/targets/aarch64-unknown-kernel.json"
RUSTFLAGS="-C link-arg=-T$ROOT_DIR/linkers/aarch64.ld" \
cargo check -p kernel --features rpi5 --target "$spec" \
    --target-dir target/check-aarch64-rpi5 --release \
    -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec 2>&1 | tail -n "${CHECK_TAIL:-20}"
exit "${PIPESTATUS[0]}"
