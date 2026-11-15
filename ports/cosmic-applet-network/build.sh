#!/usr/bin/env bash
# Build cosmic-applet-network -- the COSMIC panel's Network applet -- from the
# pinned cosmic-applets tree (e544cb5, epoch-1.3.0) with the m6 recipe, the
# same way ports/pipewire/build.sh gets cosmic-applet-audio:
#   m6-session-bins/build-rust.sh src/cosmic-applets <arch> -p cosmic-applet-network
# It is unmodified upstream. It speaks NetworkManager only through nmrs 3.4.0,
# which here reaches leandros-sysbus networkmanager (ports/sysbus/src/nm).
#
# Output: ~/code/leandros-artifacts/m6-session-bins/out/cosmic-applet-network-<arch>
# mkfs stages it as /bin/cosmic-applet-network together with
# com.system76.CosmicAppletNetwork.desktop (this directory), or neither.
#
# Usage: build.sh [aarch64|x86_64|both]   (default: both)
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/../../scripts/stage-lib.sh"

M6="$HOME/code/leandros-artifacts/m6-session-bins"
SRC="$M6/src/cosmic-applets"

case "${1:-both}" in
  aarch64)  ARCHES=(aarch64) ;;
  x86_64)   ARCHES=(x86_64) ;;
  both|"")  ARCHES=(aarch64 x86_64) ;;
  *) echo "usage: $0 [aarch64|x86_64|both]" >&2; exit 2 ;;
esac

if [[ ! -x "$M6/build-rust.sh" || ! -d "$SRC/cosmic-applet-network" ]]; then
  echo "cosmic-applet-network: no m6 toolchain/source at $M6 -- not built" >&2
  exit 1
fi

for arch in "${ARCHES[@]}"; do
  if ! "$M6/build-rust.sh" "$SRC" "$arch" -p cosmic-applet-network > /dev/null; then
    echo "cosmic-applet-network: build failed for $arch (log: $M6/logs/cosmic-applets-$arch.log)" >&2
    exit 1
  fi
  stage_file 0755 "$SRC/target/$arch-unknown-linux-musl/release/cosmic-applet-network" "$M6/out/cosmic-applet-network-$arch"
  echo "staged $arch: $M6/out/cosmic-applet-network-$arch"
done
