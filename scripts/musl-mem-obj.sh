#!/usr/bin/env bash
# Print the path of musl's asm memset assembled for <arch> (cached). zig's
# bundled static musl uses its compiler_rt memset, a byte loop on aarch64
# (13x slower than musl's NEON memset); linking this object overrides it.
arch="$1"
here="$(cd "$(dirname "$0")" && pwd)"
src="$here/memset-$arch.S"
dir="${LEANDROS_MUSL_MEM_DIR:-${TMPDIR:-/tmp}/leandros-musl-mem}"
obj="$dir/memset-$arch.o"
if [[ ! -f "$obj" || "$src" -nt "$obj" ]]; then
    mkdir -p "$dir"
    tmp="$obj.$$"
    zig cc -target "$arch-linux-musl" -c "$src" -o "$tmp" && mv -f "$tmp" "$obj"
fi
echo "$obj"
