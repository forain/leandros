#!/usr/bin/env bash
# Build and stage leandros-sysbus (org.freedesktop.login1 / locale1 / UPower
# for the COSMIC session) -- static musl, both arches, same recipe as
# ports/busd/build.sh (rust-lld, -C relocation-model=static => ET_EXEC; the
# LeandrOS loader maps a static-PIE at vaddr 0 onto the null page).
#
# Output: ~/code/leandros-artifacts/m5-session-ship/<arch>/usr/libexec/leandros-sysbus
# The .service files that make busd activate it live in
# ports/dbus/session-pkg/services/ and are staged by ports/busd/build.sh.
#
# Usage: build.sh [aarch64|x86_64|both]   (default: both)
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/../../scripts/darwin-sdk.sh"

HERE="$(cd "$(dirname "$0")" && pwd)"
SHIP="$HOME/code/leandros-artifacts/m5-session-ship"

case "${1:-both}" in
  aarch64)  ARCHES=(aarch64) ;;
  x86_64)   ARCHES=(x86_64) ;;
  both|"")  ARCHES=(aarch64 x86_64) ;;
  *) echo "usage: $0 [aarch64|x86_64|both]" >&2; exit 2 ;;
esac

for arch in "${ARCHES[@]}"; do
  target="${arch}-unknown-linux-musl"
  ( cd "$HERE" && \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
    RUSTFLAGS="-C relocation-model=static" \
    cargo +nightly build --release --locked --target "$target" )
  install -d "$SHIP/$arch/usr/libexec"
  install -m 0755 "$HERE/target/$target/release/leandros-sysbus" \
    "$SHIP/$arch/usr/libexec/leandros-sysbus"
  echo "staged $arch: usr/libexec/leandros-sysbus"
done
