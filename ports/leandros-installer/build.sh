#!/usr/bin/env bash
# Build leandros-installer (libcosmic) with the m6 recipe, sharing cosmic-files' cargo target dir.
# Usage: build.sh [aarch64|x86_64|both]
# Output: m6-session-bins/out/leandros-installer-<arch>
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/../../scripts/stage-lib.sh"
. "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib/m6out.sh"
M6="$HOME/code/leandros-artifacts/m6-session-bins"
HERE="$(cd "$(dirname "$0")" && pwd)"
case "${1:-both}" in
  aarch64) ARCHES=(aarch64) ;;
  x86_64)  ARCHES=(x86_64) ;;
  both|"") ARCHES=(aarch64 x86_64) ;;
  *) echo "usage: $0 [aarch64|x86_64|both]" >&2; exit 2 ;;
esac
[[ -x "$M6/build-rust.sh" ]] || { echo "leandros-installer: no m6 toolchain at $M6" >&2; exit 1; }
[[ -e "$HERE/target" ]] || ln -s "$M6/src/cosmic-files/target" "$HERE/target"
sh "$M6/gen-cargo-config.sh" "$HERE" > /dev/null
for arch in "${ARCHES[@]}"; do
  cosmic_cache_build leandros-installer "$arch" "$HERE" "--no-default-features --features wgpu,wayland" "" \
    "$HERE/target/$arch-unknown-linux-musl/release/leandros-installer|leandros-installer-$arch" -- \
    sh -c '"$0" "$@" > /dev/null' "$M6/build-rust.sh" "$HERE" "$arch" --no-default-features --features wgpu,wayland || {
    echo "leandros-installer: build failed for $arch (log: $M6/logs/leandros-installer-$arch.log)" >&2
    exit 1
  }
  echo "leandros-installer: $arch -> $M6_OUT/leandros-installer-$arch"
done
