#!/usr/bin/env bash
# C compiler wrapper for cross-building musl userland (cc-rs C/.S sources, e.g.
# blake3 and oniguruma). Same toolchain as linker-aarch64-musl.sh; the driver strips
# cc-rs's --target spelling so one triple/resource dir applies.
exec "${LEANDROS_TC_SRC:-$(dirname "$0")/toolchain}/aarch64-linux-musl-cc" "$@"
