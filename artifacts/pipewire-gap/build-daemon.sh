#!/bin/sh
# Build cosmic-settings-daemon against the pipewire stub for <arch>.
# system-deps env overrides bypass pkg-config for libpipewire-0.3 / libspa-0.2
# (so PKG_CONFIG_SYSROOT_DIR can still serve the daemon's libudev probe from the
# m3 sysroot without mangling our host paths). bindgen is pointed at the musl
# sysroot so it parses the pipewire/spa headers with correct linux/musl types.
# Usage: build-daemon.sh <arch: x86_64|aarch64>
set -e
arch="$1"
[ -n "$arch" ] || { echo "usage: build-daemon.sh <arch>"; exit 2; }
PG=$HOME/code/leandros-artifacts/pipewire-gap
D=$HOME/code/leandros-artifacts/m6-session-bins
S=$HOME/code/leandros-artifacts/m3-gl-stack/sysroot-$arch
triple=$arch-unknown-linux-musl
mani=$PG/build/cosmic-settings-daemon

export PATH="/opt/homebrew/opt/bison/bin:$PATH"
# pkg-config (for the daemon's libudev probe) -> m3 sysroot
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_SYSROOT_DIR="$S"
export PKG_CONFIG_LIBDIR="$S/usr/lib/pkgconfig"
export PKG_CONFIG_PATH=""
# system-deps overrides: our stub libpipewire-0.3 + header-only libspa-0.2
export SYSTEM_DEPS_LIBPIPEWIRE_NO_PKG_CONFIG=1
export SYSTEM_DEPS_LIBPIPEWIRE_SEARCH_NATIVE="$PG/lib/$arch"
export SYSTEM_DEPS_LIBPIPEWIRE_LIB="pipewire-0.3"
export SYSTEM_DEPS_LIBPIPEWIRE_INCLUDE="$PG/inc/pipewire-0.3:$PG/inc/spa-0.2"
export SYSTEM_DEPS_LIBSPA_NO_PKG_CONFIG=1
export SYSTEM_DEPS_LIBSPA_SEARCH_NATIVE="$PG/lib/$arch"
# libspa is header-only upstream; its real runtime symbols live in libpipewire,
# so point its link lib at the same stub .so (dedup'd in DT_NEEDED).
export SYSTEM_DEPS_LIBSPA_LIB="pipewire-0.3"
export SYSTEM_DEPS_LIBSPA_INCLUDE="$PG/inc/spa-0.2"
# bindgen: parse headers with musl target + sysroot for correct types/layout
export BINDGEN_EXTRA_CLANG_ARGS="--target=$arch-linux-musl --sysroot=$S -isystem $S/usr/include"
# C toolchain for cc-crate build-scripts (libspa-sys reexports)
tus=$(echo "$triple" | tr - _)
export CC_${tus}="$PG/cc/$arch-cc"
export CXX_${tus}="$PG/cc/$arch-cc"
export AR_${tus}="$D/toolchain/$arch-linux-musl-ar"
export CFLAGS_${tus}="-fno-sanitize=all -I$PG/inc/pipewire-0.3 -I$PG/inc/spa-0.2"
export LIBCLANG_PATH="/opt/homebrew/opt/llvm/lib"
export CARGO_NET_OFFLINE=false
# openssl gap (orthogonal to pipewire): stub libssl/libcrypto + homebrew headers.
# openssl-sys emits rustc-link-search=native=$OPENSSL_LIB_DIR so ld.lld finds the stubs.
export OPENSSL_LIB_DIR="$PG/openssl/lib/$arch"
export OPENSSL_INCLUDE_DIR="/opt/homebrew/opt/openssl@3/include"
export OPENSSL_NO_VENDOR=1

log="$PG/logs/daemon-$arch.log"
mkdir -p "$PG/logs"
echo "=== cargo +nightly build $triple (cosmic-settings-daemon) ===" | tee "$log"
cd "$mani"
set +e
cargo +nightly build --release --target "$triple" 2>&1 | tee -a "$log"
rc=${PIPESTATUS:-$?}
echo "=== rc=$rc ===" | tee -a "$log"
exit $rc
