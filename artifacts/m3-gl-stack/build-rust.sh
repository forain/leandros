#!/bin/sh
# Cross-build a Rust crate DYNAMIC for <arch>-unknown-linux-musl against the merged m3 sysroot.
# Usage: build-rust.sh <manifest-dir> <arch> [extra cargo args...]
# Env wiring: pkg-config points at the merged sysroot so -sys crates resolve our .so/.pc;
# cc for any C in build scripts uses our zig cc wrapper; libclang available if any bindgen fires.
set -e
D=/Users/forain/.claude-forain/jobs/afde2e74/tmp/m3-gl-stack
mani="$1"; shift; arch="$1"; shift
[ -n "$mani" ] && [ -n "$arch" ] || { echo "usage: build-rust.sh <manifest-dir> <arch> [args]"; exit 2; }
S=$D/sysroot-$arch
triple=$arch-unknown-linux-musl
export PATH="/opt/homebrew/opt/bison/bin:$PATH"
# pkg-config -> merged sysroot
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_SYSROOT_DIR="$S"
export PKG_CONFIG_LIBDIR="$S/usr/lib/pkgconfig"
export PKG_CONFIG_PATH=""
# C toolchain for any build-script C compiles (system-deps / cc crate).
# cc-crate env vars use the UNDERSCORED triple form (shell can't export hyphens).
tus=$(echo "$triple" | tr - _)
export CC_${tus}="$D/toolchain/$arch-linux-musl-cc"
export CXX_${tus}="$D/toolchain/$arch-linux-musl-c++"
export AR_${tus}="$D/toolchain/$arch-linux-musl-ar"
export CFLAGS_${tus}="-fno-sanitize=all"
# bindgen fallback (most -sys crates ship pregenerated bindings; harmless if unused)
export LIBCLANG_PATH="/opt/homebrew/opt/llvm/lib"
# xkbcommon / input crates: prefer dynamic system libs
export CARGO_NET_OFFLINE=false
log="$D/logs/$(basename $mani)-$arch.log"
mkdir -p "$D/logs"
echo "=== cargo +nightly build $triple ($mani) ===" | tee "$log"
cd "$mani"
set +e
cargo +nightly build --release --target "$triple" "$@" 2>&1 | tee -a "$log"
rc=${PIPESTATUS:-$?}
echo "=== rc=$rc ===" | tee -a "$log"
exit $rc
