#!/usr/bin/env bash
# install.sh [toolchain-dir...]  -- (re)write the one-line forwarders that make the $ART dynamic toolchain dirs
# ({m6-session-bins,m3-gl-stack}/toolchain, and m6's gen-cargo-config.sh) call the versioned wrappers in this dir.
# Idempotent. Run from the checkout that should be canonical (main). Generated .cargo configs name ld.lld (regenerate with gen-cargo-config.sh). Rerun after moving the checkout.
set -e
HERE=$(cd "$(dirname "$0")" && pwd)
. "$HERE/../env.sh"
ART=$(cd "$TC_ROOT/.." && pwd)
dirs=("$@"); [ ${#dirs[@]} -gt 0 ] || dirs=("$ART/m6-session-bins/toolchain" "$ART/m3-gl-stack/toolchain")
fwd() { printf '#!/bin/sh\nexec "%s/%s" "$@"\n' "$HERE" "$2" > "$1.tmp" && chmod 755 "$1.tmp" && mv "$1.tmp" "$1"; }
for d in "${dirs[@]}"; do
    mkdir -p "$d"
    for a in aarch64 x86_64; do for t in cc c++ ar ranlib; do fwd "$d/$a-linux-musl-$t" "$a-linux-musl-$t"; done; done
    fwd "$d/musl-dyn-link.sh" musl-dyn-link.sh
    fwd "$d/ld.lld" ld.lld
    echo "installed forwarders in $d"
    gen="$(dirname "$d")/gen-cargo-config.sh"
    if [ -f "$gen" ] || [ "$(basename "$(dirname "$d")")" = m6-session-bins ]; then
        printf '#!/bin/sh\nDYN_TC="%s" exec "%s/gen-cargo-config.sh" "$@"\n' "$d" "$HERE" > "$gen.tmp" && chmod 755 "$gen.tmp" && mv "$gen.tmp" "$gen"
        echo "installed $gen"
    fi
done
