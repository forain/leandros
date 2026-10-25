#!/usr/bin/env bash
# Link with zig's static musl, overriding its byte-loop memset with musl's asm
# one (see musl-mem-obj.sh).
obj="$("$(dirname "$0")/musl-mem-obj.sh" x86_64)" || exit 1
exec zig cc -target x86_64-linux-musl "$@" "$obj"
