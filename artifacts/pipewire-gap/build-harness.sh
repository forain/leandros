#!/bin/sh
# Build a scoped cargo target inside the daemon workspace against the pipewire stub.
# Usage: build-scoped.sh <arch> <logtag> [extra cargo args...]
# e.g.   build-scoped.sh x86_64 pw-tests -p cosmic-pipewire --tests
set -e
arch="$1"; shift; tag="$1"; shift
[ -n "$arch" ] && [ -n "$tag" ] || { echo "usage: build-scoped.sh <arch> <logtag> [cargo args]"; exit 2; }
PG=$HOME/code/leandros-artifacts/pipewire-gap
D=$HOME/code/leandros-artifacts/m6-session-bins
S=$HOME/code/leandros-artifacts/m3-gl-stack/sysroot-$arch
triple=$arch-unknown-linux-musl
mani=$PG/harness

export PATH="/opt/homebrew/opt/bison/bin:$PATH"
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_SYSROOT_DIR="$S"
export PKG_CONFIG_LIBDIR="$S/usr/lib/pkgconfig"
export PKG_CONFIG_PATH=""
export SYSTEM_DEPS_LIBPIPEWIRE_NO_PKG_CONFIG=1
export SYSTEM_DEPS_LIBPIPEWIRE_SEARCH_NATIVE="$PG/lib/$arch"
export SYSTEM_DEPS_LIBPIPEWIRE_LIB="pipewire-0.3"
export SYSTEM_DEPS_LIBPIPEWIRE_INCLUDE="$PG/inc/pipewire-0.3:$PG/inc/spa-0.2"
export SYSTEM_DEPS_LIBSPA_NO_PKG_CONFIG=1
export SYSTEM_DEPS_LIBSPA_SEARCH_NATIVE="$PG/lib/$arch"
export SYSTEM_DEPS_LIBSPA_LIB="pipewire-0.3"
export SYSTEM_DEPS_LIBSPA_INCLUDE="$PG/inc/spa-0.2"
export BINDGEN_EXTRA_CLANG_ARGS="--target=$arch-linux-musl --sysroot=$S -isystem $S/usr/include"
tus=$(echo "$triple" | tr - _)
export CC_${tus}="$PG/cc/$arch-cc"
export CXX_${tus}="$PG/cc/$arch-cc"
export AR_${tus}="$D/toolchain/$arch-linux-musl-ar"
export CFLAGS_${tus}="-fno-sanitize=all -I$PG/inc/pipewire-0.3 -I$PG/inc/spa-0.2"
export LIBCLANG_PATH="/opt/homebrew/opt/llvm/lib"
export CARGO_NET_OFFLINE=false

log="$PG/logs/$tag-$arch.log"
mkdir -p "$PG/logs"
echo "=== cargo +nightly build $triple ($tag: $*) ===" | tee "$log"
cd "$mani"
set +e
cargo +nightly build --release --target "$triple" "$@" 2>&1 | tee -a "$log"
rc=${PIPESTATUS:-$?}
echo "=== rc=$rc ===" | tee -a "$log"
exit $rc
