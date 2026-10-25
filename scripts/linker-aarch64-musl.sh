#!/usr/bin/env bash
# Link with zig's static musl, overriding its byte-loop memset with musl's asm
# one (see musl-mem-obj.sh).
obj="$("$(dirname "$0")/musl-mem-obj.sh" aarch64)" || exit 1
exec zig cc -target aarch64-linux-musl "$@" "$obj"
