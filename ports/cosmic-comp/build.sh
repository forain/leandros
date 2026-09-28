#!/bin/bash
# Rebuild cosmic-comp dec1ee86 with ports/cosmic-comp's patches and stage it at
# $ART/m6-session-bins/out/cosmic-comp-<arch>. scripts/mkfs-f2fs-populated.py
# prefers that path over the original M5 probe build (m3-gl-stack/out), and
# LEANDROS_COSMIC_COMP=<file> overrides both.
#
# smithay is NOT patched: cosmic-comp keeps its pinned git rev efeb597.
#
# Usage: build.sh <aarch64|x86_64> ...   (Mac: uses the m6-session-bins toolchain)
set -euo pipefail
ART=${LEANDROS_ARTIFACTS:-$HOME/code/leandros-artifacts}
D=$ART/m6-session-bins
HERE=$(cd "$(dirname "$0")" && pwd)
COMP_REV=dec1ee863368737c6900e1ae75424813e27e24af
CC=$D/src/cosmic-comp
# The macOS default SDK can be unreadable by the host linker (build scripts).
if [ "$(uname)" = Darwin ] && [ -z "${SDKROOT:-}" ]; then
    for s in /Library/Developer/CommandLineTools/SDKs/MacOSX26*.sdk; do [ -d "$s" ] && export SDKROOT=$s; done
fi

if [ ! -d "$CC/.git" ]; then
    git clone -q https://github.com/pop-os/cosmic-comp.git "$CC"
    git -C "$CC" checkout -q "$COMP_REV"
fi
[ "$(git -C "$CC" rev-parse HEAD)" = "$COMP_REV" ] || { echo "cosmic-comp is not at $COMP_REV" >&2; exit 1; }

# Apply each patch once (idempotent: skip a patch that is already applied).
for p in "$HERE"/0*.patch; do
    if git -C "$CC" apply --reverse --check "$p" 2>/dev/null; then
        echo "already applied: $(basename "$p")"
    else
        git -C "$CC" apply "$p"
        echo "applied: $(basename "$p")"
    fi
done
sh "$D/gen-cargo-config.sh" "$CC" >/dev/null

mkdir -p "$D/out"
for arch in "$@"; do
    bin=$CC/target/$arch-unknown-linux-musl/release/cosmic-comp
    rm -f "$bin"   # build-rust.sh's exit status does not reflect cargo's
    sh "$D/build-rust.sh" "$CC" "$arch" --no-default-features
    [ -x "$bin" ] || { echo "cosmic-comp build failed for $arch" >&2; exit 1; }
    cp "$bin" "$D/out/cosmic-comp-$arch"
    echo "staged $D/out/cosmic-comp-$arch"
done
