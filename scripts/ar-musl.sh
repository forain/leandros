#!/usr/bin/env bash
# Archiver for cross-building musl userland with C dependencies.
#
# cc-rs defaults to the host `ar`, which on macOS emits a Mach-O-format archive
# (`__.SYMDEF SORTED`) that ld.lld cannot read. llvm-ar emits a normal ELF
# archive. Architecture-independent, so both targets share this script.
. "$(dirname "$0")/toolchain/env.sh" || exit 1
exec "$LLVM_AR" "$@"
