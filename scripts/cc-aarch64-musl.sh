#!/usr/bin/env bash
# C compiler wrapper for cross-building musl userland. See cc-x86_64-musl.sh for
# why this is separate from linker-aarch64-musl.sh (cc-rs appends a --target=
# spelling that zig rejects).
args=()
skip_next=0
for a in "$@"; do
    if (( skip_next )); then skip_next=0; continue; fi
    case "$a" in
        --target=*) continue ;;
        -target)    skip_next=1; continue ;;
    esac
    args+=("$a")
done
exec zig cc -target aarch64-linux-musl "${args[@]}"
