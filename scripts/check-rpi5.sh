#!/usr/bin/env bash
# Fast compile-only check of the kernel's Raspberry Pi 5 variant (aarch64).
# Runs `cargo check` (seconds, no image, no userland) with the SAME cargo
# feature set build-all.sh --rpi5 uses: `rpi5` plus whatever the Kconfig
# (configs/rpi5_defconfig, overridden by $ROOT/.config) turns on. Matching the
# Kconfig features matters: code such as kernel/src/kmod is gated on them, so a
# bare `--features rpi5` check misses breakage like a drifted blkdev_sdhci.rs.
# Exits non-zero on any error (including a failed feature derivation).
# CHECK_FEATURE=raspi4b selects the Pi 4 variant; CHECK_TAIL sets lines shown.
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"
board="${CHECK_FEATURE:-rpi5}"
arch=aarch64
kdir="$ROOT_DIR/target/kconfig-check-$board"
mkdir -p "$kdir"
if [[ -f "$ROOT_DIR/.config" ]]; then
    cat "$ROOT_DIR/configs/${board}_defconfig" "$ROOT_DIR/.config" > "$kdir/.config.in"
    python3 "$ROOT_DIR/scripts/kconfig/to-cargo.py" olddefconfig --arch "$arch" --config "$kdir/.config.in" --out "$kdir/.config" >&2
else
    python3 "$ROOT_DIR/scripts/kconfig/to-cargo.py" defconfig --arch "$arch" --defconfig "$ROOT_DIR/configs/${board}_defconfig" --out "$kdir/.config" >&2
fi
kc="$(python3 "$ROOT_DIR/scripts/kconfig/to-cargo.py" features --arch "$arch" --config "$kdir/.config" --kmod-list "$kdir/kmod-list")"
feats="$board"
IFS=',' read -r -a kcf <<< "$kc"
for f in "${kcf[@]}"; do [[ -n "$f" && ",$feats," != *",$f,"* ]] && feats="$feats,$f"; done
echo "check-rpi5: features=$feats"
spec="$ROOT_DIR/targets/aarch64-unknown-kernel.json"
rc=0
RUSTFLAGS="-C link-arg=-T$ROOT_DIR/linkers/aarch64.ld" \
cargo check -p kernel --features "$feats" --target "$spec" \
    --target-dir "target/check-aarch64-$board" --release \
    -Z build-std=core,alloc -Zbuild-std-features=compiler-builtins-mem -Zjson-target-spec \
    >"$kdir/check.log" 2>&1 || rc=$?
tail -n "${CHECK_TAIL:-20}" "$kdir/check.log"
# Host unit tests of the Pi logic (PM_RSTS boot partition, RESTART2 "[N] [tryboot]",
# SD MBR F2FS-root pick): the module is self-contained, so plain rustc --test.
if rustc --edition 2021 --test "$ROOT_DIR/drivers/src/rpi_misc_logic.rs" -o "$kdir/rpi_misc_logic-test" >"$kdir/hosttest.log" 2>&1 \
   && "$kdir/rpi_misc_logic-test" -q >>"$kdir/hosttest.log" 2>&1; then
    echo "check-rpi5: rpi_misc_logic host tests ok"
else
    tail -n 20 "$kdir/hosttest.log"; echo "check-rpi5: rpi_misc_logic host tests FAILED"; rc=1
fi
exit "$rc"
