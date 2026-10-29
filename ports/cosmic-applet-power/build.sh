#!/usr/bin/env bash
# Build cosmic-applet-power -- the COSMIC panel's Power menu (lock, log out,
# suspend, restart, shut down) -- from the pinned cosmic-applets tree
# (e544cb5, epoch-1.3.0) with the m6 recipe:
#   m6-session-bins/build-rust.sh src/cosmic-applets <arch> -p cosmic-applet-power
# It is unmodified upstream. It talks to leandros-sysbus login1 (system bus)
# and to com.system76.CosmicSession (session bus) for Log Out.
#
# Output: ~/code/leandros-artifacts/m6-session-bins/out/cosmic-applet-power-<arch>
# mkfs stages it as /bin/cosmic-applet-power together with
# com.system76.CosmicAppletPower.desktop (this directory), or neither.
#
# Usage: build.sh [aarch64|x86_64|both]   (default: both)
set -euo pipefail

M6="$HOME/code/leandros-artifacts/m6-session-bins"
SRC="$M6/src/cosmic-applets"

case "${1:-both}" in
  aarch64)  ARCHES=(aarch64) ;;
  x86_64)   ARCHES=(x86_64) ;;
  both|"")  ARCHES=(aarch64 x86_64) ;;
  *) echo "usage: $0 [aarch64|x86_64|both]" >&2; exit 2 ;;
esac

if [[ ! -x "$M6/build-rust.sh" || ! -d "$SRC/cosmic-applet-power" ]]; then
  echo "cosmic-applet-power: no m6 toolchain/source at $M6 -- not built" >&2
  exit 1
fi

for arch in "${ARCHES[@]}"; do
  if ! "$M6/build-rust.sh" "$SRC" "$arch" -p cosmic-applet-power > /dev/null; then
    echo "cosmic-applet-power: build failed for $arch (log: $M6/logs/cosmic-applets-$arch.log)" >&2
    exit 1
  fi
  install -m 0755 "$SRC/target/$arch-unknown-linux-musl/release/cosmic-applet-power" \
    "$M6/out/cosmic-applet-power-$arch"
  echo "staged $arch: $M6/out/cosmic-applet-power-$arch"
done
