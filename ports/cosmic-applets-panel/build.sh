#!/usr/bin/env bash
# Build the remaining COSMIC panel applets -- Time, Battery, Notifications,
# Status Area, Workspaces, Input Sources, A11y and the dock's App List --
# from the pinned cosmic-applets tree (e544cb5, epoch-1.3.0) with the m6 recipe,
# one cargo invocation for all of them (shared dependency graph):
#   m6-session-bins/build-rust.sh src/cosmic-applets <arch> -p <crate> ...
# Unmodified upstream. Upstream ships one binary per applet crate (the
# `cosmic-applets` multicall crate is not part of its install rules).
#
# Output: ~/code/leandros-artifacts/m6-session-bins/out/<applet>-<arch>
# mkfs stages each as /bin/<applet> together with its .desktop (this
# directory), or neither, with a WARNING.
#
# Usage: build.sh [aarch64|x86_64|both]   (default: both)
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/../../scripts/stage-lib.sh"

M6="$HOME/code/leandros-artifacts/m6-session-bins"
SRC="$M6/src/cosmic-applets"
APPLETS=(cosmic-applet-time cosmic-applet-battery cosmic-applet-notifications
         cosmic-applet-status-area cosmic-applet-workspaces
         cosmic-applet-input-sources cosmic-applet-a11y cosmic-app-list
         cosmic-applet-audio)

case "${1:-both}" in
  aarch64)  ARCHES=(aarch64) ;;
  x86_64)   ARCHES=(x86_64) ;;
  both|"")  ARCHES=(aarch64 x86_64) ;;
  *) echo "usage: $0 [aarch64|x86_64|both]" >&2; exit 2 ;;
esac

if [[ ! -x "$M6/build-rust.sh" || ! -d "$SRC/cosmic-applet-time" ]]; then
  echo "cosmic-applets-panel: no m6 toolchain/source at $M6 -- not built" >&2
  exit 1
fi

PKGS=(); for a in "${APPLETS[@]}"; do PKGS+=(-p "$a"); done
for arch in "${ARCHES[@]}"; do
  if ! "$M6/build-rust.sh" "$SRC" "$arch" "${PKGS[@]}" > /dev/null; then
    echo "cosmic-applets-panel: build failed for $arch (log: $M6/logs/cosmic-applets-$arch.log)" >&2
    exit 1
  fi
  for a in "${APPLETS[@]}"; do
    stage_file 0755 "$SRC/target/$arch-unknown-linux-musl/release/$a" "$M6/out/$a-$arch"
    echo "staged $arch: $M6/out/$a-$arch"
  done
done
