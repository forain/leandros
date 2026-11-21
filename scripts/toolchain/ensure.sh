#!/usr/bin/env bash
# ensure.sh <arch>...  -- idempotent: build the clang/musl sysroot for each arch when missing or stale.
# Safe under concurrent builds on one host (flock where available). Used by build-userland.sh so
# hosts such as the box farm need no manual setup (sysroot root defaults to <artifacts>/llvm-musl,
# or ~/code/leandros-artifacts/llvm-musl; set LEANDROS_TOOLCHAIN to override).
set -euo pipefail
D=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
if [ -z "${LEANDROS_TOOLCHAIN:-}" ]; then
    . "$D/env.sh" 2>/dev/null || { mkdir -p "${XDG_CACHE_HOME:-$HOME/.cache}/leandros"; export LEANDROS_TOOLCHAIN=${XDG_CACHE_HOME:-$HOME/.cache}/leandros/llvm-musl; }
fi
for a in "$@"; do
    "$D/check.sh" "$a" 2>/dev/null && continue
    mkdir -p "${LEANDROS_TOOLCHAIN:-${XDG_CACHE_HOME:-$HOME/.cache}/leandros/llvm-musl}"
    lk=${LEANDROS_TOOLCHAIN:-${XDG_CACHE_HOME:-$HOME/.cache}/leandros/llvm-musl}/.mk-sysroot.lock
    if command -v flock >/dev/null 2>&1; then
        flock "$lk" "$D/mk-sysroot.sh" "$a"
    else
        "$D/mk-sysroot.sh" "$a"
    fi
    "$D/check.sh" "$a"
done
