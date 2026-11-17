#!/usr/bin/env bash
# Build leandros-installer (libcosmic) with the m6 recipe, sharing cosmic-files' cargo target dir.
# Usage: build.sh [aarch64|x86_64|both]
# Output: m6-session-bins/out/leandros-installer-<arch>
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/../../scripts/stage-lib.sh"
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
[[ -f "$HERE/.cargo/config.toml" ]] || sh "$M6/gen-cargo-config.sh" "$HERE" > /dev/null
for arch in "${ARCHES[@]}"; do
  if ! "$M6/build-rust.sh" "$HERE" "$arch" --no-default-features --features wgpu,wayland > /dev/null; then
    echo "leandros-installer: build failed for $arch (log: $M6/logs/leandros-installer-$arch.log)" >&2
    exit 1
  fi
  stage_file 0755 "$HERE/target/$arch-unknown-linux-musl/release/leandros-installer" "$M6/out/leandros-installer-$arch"
  echo "leandros-installer: $arch -> $M6/out/leandros-installer-$arch"
done
