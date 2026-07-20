#!/usr/bin/env bash
# C compiler wrapper for cross-building musl userland (uutils/coreutils pulls in
# C and .S sources via blake3 and oniguruma).
#
# This is deliberately separate from linker-x86_64-musl.sh: cc-rs appends its own
# `--target=x86_64-unknown-linux-musl` to every invocation, and zig only accepts
# its own triple spelling (`x86_64-linux-musl`) — it rejects the 4-component GNU
# form with "UnknownOperatingSystem". The linker path never hits this because
# rustc invokes the wrapper without a --target flag, so the shared linker script
# stays untouched.
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
exec zig cc -target x86_64-linux-musl "${args[@]}"
