# scripts/toolchain — clang/lld + Alpine musl sysroot (replaces `zig cc`)

Lane L0 of docs: orch/designs/zig2clang.md. Nothing in the build calls this yet (L1-L4 migrate callers).

- `<arch>-linux-musl-{cc,c++}` — drop-in for `zig cc -target <arch>-linux-musl` (static ET_EXEC, non-PIE codegen,
  no aarch64 outline atomics, `-DNDEBUG` at -O2/-O3/-Os/-Oz; cc-rs `--target` stripped; `-Wl,--version-script <p>` joined).
  `-shared`/`-pie`/`-dynamic` links stay dynamic. C++ = libc++ + libc++abi + libunwind, static.
- `mk-sysroot.sh <arch>` — builds `$LEANDROS_TOOLCHAIN/<arch>/{sysroot,resource}` (default `<artifacts>/llvm-musl`)
  from `ports/alpine/toolchain.lock` (musl-dev == image musl, linux-headers, compiler-rt builtins, llvm-libunwind, libc++).
  `toolchain-lock.py refresh` re-pins the lock (run after `alpine-lock.py refresh`).
- `check.sh [arch]` — fails with a fix-it line when sysroot is missing/stale (clang upgrade, lock change).
- `selftest.sh <arch> [--guest]` — G1. `--guest` runs the binaries in the LeandrOS guest (QEMU); otherwise natively
  on a matching Linux host or under qemu-<arch> user mode.
- Host tools: Mac = Homebrew `llvm` (clang, llvm-ar/nm/strip) + `lld` if installed, else a rust-lld shim
  (`$LEANDROS_TOOLCHAIN/bin/ld.lld`, written by mk-sysroot.sh, `-flavor gnu`). Linux = /usr/bin (clang, lld, llvm).
