#!/usr/bin/env bash
# Archiver for cross-building musl userland with C dependencies.
#
# cc-rs defaults to the host `ar`, which on macOS emits a Mach-O-format archive
# (`__.SYMDEF SORTED`). ld.lld cannot read one, so every symbol from that
# archive comes back undefined at link time even though the objects inside were
# correctly compiled as ELF by zig cc. `zig ar` is llvm-ar and emits a normal
# ELF archive. Architecture-independent, so both targets share this script.
exec zig ar "$@"
