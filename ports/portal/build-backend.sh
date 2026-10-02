#!/bin/bash
# build-backend.sh <x86_64|aarch64> — cross-build xdg-desktop-portal-cosmic
# (the org.freedesktop.impl.portal.desktop.cosmic backend: FileChooser,
# Screenshot, Settings, Access) for LeandrOS, UNMODIFIED, from the pinned
# ../cosmic-epoch checkout (submodule xdg-desktop-portal-cosmic, epoch-1.3.0).
#
# Same dynamic-musl-PIE recipe as every other COSMIC client
# (m6-session-bins: zig ld.lld against the m3 GL sysroot), plus:
#   * GLib/GIO from ports/portal/out/sdk-<arch> (Alpine 3.21's, made by
#     build-in-alpine.sh): the backend links cosmic-files with its "gvfs"
#     feature, which the portal's Cargo.toml hardcodes. system-deps env
#     overrides, so the m3 sysroot keeps serving pkg-config for the rest.
#   * PipeWire headers from the pipewire-gap tree and the inert stub
#     libpipewire built by build-in-alpine.sh (ScreenCast only; the portal
#     never touches PipeWire for the interfaces LeandrOS advertises).
# No source file is touched: the cargo config is passed with --config and the
# target dir lives under ports/portal/out.
#
# Output: ports/portal/out/backend-<arch>/xdg-desktop-portal-cosmic
set -uo pipefail
arch=${1:?usage: build-backend.sh <x86_64|aarch64>}
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
ART=${LEANDROS_ARTIFACTS:-$HOME/code/leandros-artifacts}
D=$ART/m6-session-bins
S=$ART/m3-gl-stack/sysroot-$arch
PG=$ART/pipewire-gap
SDK=$HERE/out/sdk-$arch
OUT=$HERE/out/backend-$arch
triple=$arch-unknown-linux-musl
# cosmic-epoch lookup order, as scripts/mkfs-f2fs-populated.py
SRC=""
for c in "${LEANDROS_COSMIC_EPOCH:-}" "$ROOT/../cosmic-epoch" "$HOME/code/cosmic-epoch"; do
    [ -n "$c" ] && [ -f "$c/xdg-desktop-portal-cosmic/Cargo.toml" ] && { SRC="$c/xdg-desktop-portal-cosmic"; break; }
done
[ -n "$SRC" ] || { echo "no cosmic-epoch/xdg-desktop-portal-cosmic checkout (set LEANDROS_COSMIC_EPOCH)"; exit 2; }
for p in "$D/toolchain/zig-ld-lld" "$S/usr/lib/libc.so" "$PG/inc/pipewire-0.3" "$SDK/usr/lib/libgio-2.0.so" \
         "$HERE/out/$arch.alpine/usr/lib/libpipewire-0.3.so.0"; do
    [ -e "$p" ] || { echo "missing $p (run ports/portal/build.sh first / check leandros-artifacts)"; exit 2; }
done
case "$arch" in x86_64) elfm=elf_x86_64 ;; aarch64) elfm=aarch64linux ;; esac
mkdir -p "$OUT" "$HERE/out/stub-$arch"
ln -sf "$HERE/out/$arch.alpine/usr/lib/libpipewire-0.3.so.0" "$HERE/out/stub-$arch/libpipewire-0.3.so"
cfg=$HERE/out/cargo-$arch.toml
cat > "$cfg" <<EOF
[target.$triple]
linker = "$D/toolchain/zig-ld-lld"
rustflags = [
  "-C", "linker-flavor=ld",
  "-C", "target-feature=-crt-static",
  "-C", "relocation-model=pic",
  "-C", "link-self-contained=no",
  "-C", "link-args=--sysroot=$S --entry _start --build-id=none --eh-frame-hdr -znow -m $elfm --dynamic-linker /lib/ld-musl-$arch.so.1 -pie -L$S/usr/lib -L$SDK/usr/lib -L$HERE/out/stub-$arch $S/usr/lib/Scrt1.o $S/usr/lib/crti.o $S/usr/lib/crtn.o -lc",
]
EOF
export PATH="/opt/homebrew/opt/bison/bin:$PATH"
export PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_SYSROOT_DIR="$S" PKG_CONFIG_LIBDIR="$S/usr/lib/pkgconfig" PKG_CONFIG_PATH=""
gi="$SDK/usr/include/glib-2.0:$SDK/usr/lib/glib-2.0/include"
for dep in GLIB_2_0:glib-2.0 GOBJECT_2_0:gobject-2.0 GIO_2_0:gio-2.0; do
    n=${dep%%:*}; l=${dep#*:}
    export SYSTEM_DEPS_${n}_NO_PKG_CONFIG=1
    export SYSTEM_DEPS_${n}_SEARCH_NATIVE="$SDK/usr/lib"
    export SYSTEM_DEPS_${n}_LIB="$l"
    export SYSTEM_DEPS_${n}_INCLUDE="$gi"
done
export SYSTEM_DEPS_LIBPIPEWIRE_NO_PKG_CONFIG=1 SYSTEM_DEPS_LIBPIPEWIRE_SEARCH_NATIVE="$HERE/out/stub-$arch" \
       SYSTEM_DEPS_LIBPIPEWIRE_LIB="pipewire-0.3" SYSTEM_DEPS_LIBPIPEWIRE_INCLUDE="$PG/inc/pipewire-0.3:$PG/inc/spa-0.2"
export SYSTEM_DEPS_LIBSPA_NO_PKG_CONFIG=1 SYSTEM_DEPS_LIBSPA_SEARCH_NATIVE="$HERE/out/stub-$arch" \
       SYSTEM_DEPS_LIBSPA_LIB="pipewire-0.3" SYSTEM_DEPS_LIBSPA_INCLUDE="$PG/inc/spa-0.2"
export BINDGEN_EXTRA_CLANG_ARGS="--target=$arch-linux-musl --sysroot=$S -isystem $S/usr/include"
tus=$(echo "$triple" | tr - _)
export CC_${tus}="$PG/cc/$arch-cc" CXX_${tus}="$D/toolchain/$arch-linux-musl-c++" AR_${tus}="$D/toolchain/$arch-linux-musl-ar"
export CFLAGS_${tus}="-fno-sanitize=all -I$PG/inc/pipewire-0.3 -I$PG/inc/spa-0.2"
export LIBCLANG_PATH="/opt/homebrew/opt/llvm/lib"
export CARGO_TARGET_DIR="$HERE/out/target"
log=$HERE/out/backend-$arch.log
echo "=== cargo +nightly build $triple ($SRC @ $(git -C "$SRC" rev-parse --short HEAD 2>/dev/null)) ===" | tee "$log"
( cd "$SRC" && cargo +nightly build --release --locked --target "$triple" --config "$cfg" ) >> "$log" 2>&1
rc=$?
if [ $rc -eq 0 ]; then
    cp -f "$CARGO_TARGET_DIR/$triple/release/xdg-desktop-portal-cosmic" "$OUT/xdg-desktop-portal-cosmic"
    git -C "$SRC" rev-parse HEAD > "$OUT/SOURCE-REV" 2>/dev/null || true
fi
tail -5 "$log"
echo "=== rc=$rc arch=$arch ===" | tee -a "$log"
exit $rc
