#!/usr/bin/env bash
# C compiler wrapper for cross-building musl userland (cc-rs C/.S sources, e.g.
# blake3 and oniguruma). Same toolchain as linker-x86_64-musl.sh; the driver strips
# cc-rs's --target spelling so one triple/resource dir applies.
exec "${LEANDROS_TC_SRC:-$(dirname "$0")/toolchain}/x86_64-linux-musl-cc" "$@"
