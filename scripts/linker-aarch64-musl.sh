#!/usr/bin/env bash
# Link with the clang/lld musl toolchain (scripts/toolchain): static ET_EXEC by
# default, Alpine musl libc.a (asm memset/memcpy), compiler-rt builtins, and
# libunwind.a for Rust std (rustc passes -lunwind itself).
exec "${LEANDROS_TC_SRC:-$(dirname "$0")/toolchain}/aarch64-linux-musl-cc" "$@"
