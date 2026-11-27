#!/bin/bash
# Rebuild cosmic-idle (the m6-session-bins tree, 0.1.0) with ports/cosmic-idle's patches and
# stage it at $ART/m6-session-bins/out/cosmic-idle-<arch>, the file
# scripts/mkfs-f2fs-populated.py packs (LEANDROS_COSMIC_IDLE=<file> overrides it).
#
# Usage: build.sh <aarch64|x86_64> ...   (Mac: uses the m6-session-bins toolchain)
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/../../scripts/stage-lib.sh"
ART=${LEANDROS_ARTIFACTS:-$HOME/code/leandros-artifacts}
. "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib/m6out.sh"
. "$(dirname "${BASH_SOURCE[0]}")/../../scripts/lib/cosmic-cache.sh"
D=$ART/m6-session-bins
HERE=$(cd "$(dirname "$0")" && pwd)
CI=$D/src/cosmic-idle
[ -f "$CI/Cargo.toml" ] || { echo "no cosmic-idle tree at $CI" >&2; exit 1; }
# The macOS default SDK can be unreadable by the host linker (build scripts).
if [ "$(uname)" = Darwin ] && [ -z "${SDKROOT:-}" ]; then
    for s in /Library/Developer/CommandLineTools/SDKs/MacOSX26*.sdk; do [ -d "$s" ] && export SDKROOT=$s; done
fi

# Apply each patch once (the tree is not a git checkout: use patch(1), skip a patch that
# already reverses cleanly).
for p in "$HERE"/0*.patch; do
    if patch -d "$CI" -p1 -R --dry-run -s -f < "$p" >/dev/null 2>&1; then
        echo "already applied: $(basename "$p")"
    else
        patch -d "$CI" -p1 -s -f < "$p"
        echo "applied: $(basename "$p")"
    fi
done
sh "$D/gen-cargo-config.sh" "$CI" >/dev/null

mkdir -p "$M6_OUT"
for arch in "$@"; do
    bin=$CI/target/$arch-unknown-linux-musl/release/cosmic-idle
    cosmic_cache_build cosmic-idle "$arch" "$CI" "" "$(echo "$HERE"/0*.patch)" \
        "$bin|cosmic-idle-$arch" -- sh "$D/build-rust.sh" "$CI" "$arch" || { echo "cosmic-idle build failed for $arch" >&2; exit 1; }
    echo "staged $D/out/cosmic-idle-$arch"
done
