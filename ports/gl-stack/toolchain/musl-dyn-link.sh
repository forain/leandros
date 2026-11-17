#!/bin/sh
# Historical recipe wrapper: forwards to scripts/toolchain/dyn/musl-dyn-link.sh.
exec "$(dirname "$0")/../../../scripts/toolchain/dyn/musl-dyn-link.sh" "$@"
