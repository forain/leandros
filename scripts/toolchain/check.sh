#!/usr/bin/env bash
# check.sh [arch...]  -- exit 1 with a fix-it message when the toolchain is missing or stale.
. "$(dirname "${BASH_SOURCE[0]}")/env.sh" || exit 1
rc=0
for arch in "${@:-aarch64 x86_64}"; do for a in $arch; do
    [ -x "$CLANG" ] || { echo "toolchain: clang missing ($CLANG): brew install llvm / pacman -S clang" >&2; exit 1; }
    cur=$("$CLANG" -print-resource-dir)
    if [ ! -f "$TC_ROOT/$a/stamp" ] || ! grep -q "^resource=$cur\$" "$TC_ROOT/$a/stamp" \
        || ! grep -q "^lock=$(tc_sha256 "$TC_REPO/ports/alpine/toolchain.lock")\$" "$TC_ROOT/$a/stamp"; then
        echo "toolchain: $a sysroot missing or stale; fix: scripts/toolchain/mk-sysroot.sh $a" >&2; rc=1
    elif [ ! -x "$LLD" ]; then
        echo "toolchain: ld.lld missing ($LLD); fix: scripts/toolchain/mk-sysroot.sh $a (rust-lld fallback) or brew install lld" >&2; rc=1
    fi
done; done
exit $rc
