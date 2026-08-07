#!/bin/sh
# Cross-build a Rust crate DYNAMIC for <arch>-unknown-linux-musl against the merged m3 sysroot.
# Usage: build-rust.sh <manifest-dir> <arch> [extra cargo args...]
# manifest-dir must already contain .cargo/config.toml (see gen-cargo-config.sh).
set -e
D=/Users/forain/code/leandros-artifacts/m6-session-bins
mani="$1"; shift; arch="$1"; shift
[ -n "$mani" ] && [ -n "$arch" ] || { echo "usage: build-rust.sh <manifest-dir> <arch> [args]"; exit 2; }
S=/Users/forain/code/leandros-artifacts/m3-gl-stack/sysroot-$arch
triple=$arch-unknown-linux-musl
export PATH="/opt/homebrew/opt/bison/bin:$PATH"
# pkg-config -> merged sysroot (m3 base+GL ∪ read via same sysroot dirs)
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_SYSROOT_DIR="$S"
export PKG_CONFIG_LIBDIR="$S/usr/lib/pkgconfig"
export PKG_CONFIG_PATH=""
# C toolchain for any build-script C compiles (system-deps / cc crate).
tus=$(echo "$triple" | tr - _)
export CC_${tus}="$D/toolchain/$arch-linux-musl-cc"
export CXX_${tus}="$D/toolchain/$arch-linux-musl-c++"
export AR_${tus}="$D/toolchain/$arch-linux-musl-ar"
export CFLAGS_${tus}="-fno-sanitize=all"
export LIBCLANG_PATH="/opt/homebrew/opt/llvm/lib"
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
