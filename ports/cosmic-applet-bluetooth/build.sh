#!/usr/bin/env bash
# Build cosmic-applet-bluetooth -- the COSMIC panel's Bluetooth applet -- from the
# pinned cosmic-applets tree (e544cb5, epoch-1.3.0) with the m6 recipe, the
# same way ports/pipewire/build.sh gets cosmic-applet-audio:
#   m6-session-bins/build-rust.sh src/cosmic-applets <arch> -p cosmic-applet-bluetooth
# It is unmodified upstream. It speaks org.bluez only through bluer 0.17.4 (libdbus-1, Alpine dbus-libs, ports/alpine),
# which here reaches leandros-sysbus bluez (ports/sysbus/src/bluez).
#
# Output: ~/code/leandros-artifacts/m6-session-bins/out/cosmic-applet-bluetooth-<arch>
# mkfs stages it as /bin/cosmic-applet-bluetooth together with
# com.system76.CosmicAppletBluetooth.desktop (this directory), or neither.
#
# Usage: build.sh [aarch64|x86_64|both]   (default: both)
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/../../scripts/stage-lib.sh"

. "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib/m6out.sh"
M6="$HOME/code/leandros-artifacts/m6-session-bins"
SRC="$M6/src/cosmic-applets"

case "${1:-both}" in
  aarch64)  ARCHES=(aarch64) ;;
  x86_64)   ARCHES=(x86_64) ;;
  both|"")  ARCHES=(aarch64 x86_64) ;;
  *) echo "usage: $0 [aarch64|x86_64|both]" >&2; exit 2 ;;
esac

if [[ ! -x "$M6/build-rust.sh" || ! -d "$SRC/cosmic-applet-bluetooth" ]]; then
  echo "cosmic-applet-bluetooth: no m6 toolchain/source at $M6 -- not built" >&2
  exit 1
fi

for arch in "${ARCHES[@]}"; do
  # libdbus-1 (Alpine dbus-dev + dbus-libs) must be in the sysroot first (libdbus-sys finds it via pkg-config).
  python3 "$(dirname "$0")/../../scripts/alpine-dev.py" merge "$arch" "${LEANDROS_ARTIFACTS:-$HOME/code/leandros-artifacts}/m3-gl-stack/sysroot-$arch" > /dev/null
  if ! "$M6/build-rust.sh" "$SRC" "$arch" -p cosmic-applet-bluetooth > /dev/null; then
    echo "cosmic-applet-bluetooth: build failed for $arch (log: $M6/logs/cosmic-applets-$arch.log)" >&2
    exit 1
  fi
  stage_file 0755 "$SRC/target/$arch-unknown-linux-musl/release/cosmic-applet-bluetooth" "$M6_OUT/cosmic-applet-bluetooth-$arch"
  echo "staged $arch: $M6_OUT/cosmic-applet-bluetooth-$arch"
done
