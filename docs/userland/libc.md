# C libraries

LeandrOS userland runs on three C runtimes: **relibc**, the Redox OS libc, retargeted to
LeandrOS and used by the in-tree tests that need a complete POSIX library; **leandros-libc**,
a minimal in-tree runtime for small `no_std` programs; and **musl**, both static and as a
real dynamic `ld-musl`, which is what every ported program and the whole desktop run on.
All three issue the same Linux-numbered syscalls; none of them translates anything.

## Overview

| Library | Where | Used by | Linkage |
|---|---|---|---|
| relibc | sibling checkout `../relibc` (override `LEANDROS_RELIBC_DIR`) | `pthreadtest`, `sigtest`, `timertest`, `polltest`, `drmsmoke`, `vttest`, `ping`, … (the `RELIBC_LINKED` list in [scripts/build-userland.sh](../../scripts/build-userland.sh)); MAME | static `librelibc.a`, target `<arch>-unknown-leandros` |
| leandros-libc | [userland/libc](../../userland/libc/src/lib.rs) | `init`, `login`, `shell`, `greeter-launch`, `mount`, `aplay`, `vfstest`, DOOM, … | static, target `<arch>-unknown-none` |
| musl 1.2.5 | Rust's self-contained musl, zig's bundled musl, and a shipped `ld-musl` + `libc.so` | brush, coreutils, busd, greetd, COSMIC, Mesa, PipeWire, Firefox | static or dynamic |

## relibc

LeandrOS ships **relibc**, a full-featured C standard library originally developed for Redox
OS, as the C runtime for in-tree programs that need more than a minimal one:

- A complete POSIX libc implementation in Rust, compiled to a `staticlib`.
- Covers `stdio`, `stdlib`, `string`, `math`, `pthread`, `signal`, `time`, `unistd`,
  `sys/mman`, `dlfcn` and many more headers.
- Backed by `dlmalloc` for heap allocation and `openlibm` for `libm` functions.
- Includes `crt0`, `crti`, `crtn` and a dynamic linker (`ld_so`) for position-independent
  executables.
- Enables porting existing C/C++ software with minimal changes; MAME's LeandrOS build
  compiles against relibc's sysroot ([ports.md](ports.md#mame)).

### How it is retargeted

relibc gains a `target_os = "leandros"` and a platform module,
`src/platform/leandros/`, whose job is deliberately small. Because LeandrOS implements the
Linux syscall ABI — numbers, calling convention, signal-frame layout — relibc's **Linux**
`Pal` implementation is reused as-is rather than maintained as a parallel copy. The LeandrOS
module only supplies what that code expects from the external `sc` crate (syscall trampolines
and number tables, vendored unchanged, because `sc`'s own platform dispatch does not
recognise a custom `target_os`) plus the few LeandrOS-only calls with no Linux equivalent
(IPC ports, `spawn`).

The LeandrOS changes are a short series of commits on top of upstream in the sibling checkout
(currently at `fd1967e1`): target recognition across headers and build config, reuse of the
Linux `Pal`, an aarch64 `rlct_clone`, the per-architecture `epoll_event` ABI,
`timer_getoverrun`, `exit_group` for process exit, an aarch64 thread-entry stack alignment
fix, and Linux semantics for `poll()` on non-epoll and duplicate fds. Two checkouts at
different commits produce different test results (the `poll_linux_semantics` case fails
without `fd1967e1`), which is why the build accepts `LEANDROS_RELIBC_DIR` and records which
checkout it used.

### How it is built and linked

`build_relibc` in [scripts/build-all.sh](../../scripts/build-all.sh) runs
`cargo build --target targets/<arch>-unknown-leandros.json --release -Z build-std=core,alloc,compiler_builtins`
in the sibling checkout. relibc's C sources go through the `cc` crate, which with no `CC` set
falls back to the *host* compiler; the build points it at the same `zig cc` wrapper the rest of
the cross-build uses, because relibc passes the aarch64-only `-mno-outline-atomics` and a host
x86_64 gcc rejects it.

The [target specs](../../targets/) are plain bare-metal targets (`llvm-target`
`<arch>-unknown-none`, static relocation model, no PIE, `rust-lld`) with `os = "leandros"`.
Each relibc-linked test has a `build.rs` that links `librelibc.a` from the sibling with
`-z muldefs` and declares `rerun-if-changed` on the archive — cargo does not track external
native libraries, and without that line every binary silently keeps the libc it was first
linked against.

Only `cargo build` runs, never relibc's own `make sysroot`, so a C program that wants relibc
needs its headers (generated with `cbindgen`; relibc's `make headers` uses a GNU-only `find`
flag and silently breaks on macOS), `crt0`, and the small C shims in `src/c/stdlib.c`
(`strtold` and friends) built by hand. The recipe is recorded with the MAME port.

## leandros-libc

`leandros-libc` ([userland/libc](../../userland/libc/)) is the thin in-tree runtime: Rust,
`no_std`, exporting C-linkage symbols so that native Rust programs and C code (DOOM) can share
one interface. It provides `_start` and `__libc_start_main`, raw inline-asm syscall wrappers
with the Linux numbers, `open`/`read`/`write`/`close`/`mmap`, process calls, a bump allocator,
string and `printf`-family functions, `errno`, time, and the LeandrOS-specific pieces:
`ipc_call`, `get_vfs_port`, `get_net_port`, `get_audio_port` and the other port-discovery helpers that read
server port ids from the ELF auxiliary vector ([../kernel/ipc.md](../kernel/ipc.md)).

It exists because PID 1 and the login path should depend on as little as possible: `init` is
1.5 k lines on top of it and runs before any filesystem beyond the initrd is mounted.
Programs built on it are static `ET_EXEC` images with `--entry=_start`.

## musl

Ported third-party software — Mesa, D-Bus/busd, COSMIC, brush, coreutils — is built for
`*-unknown-linux-musl`. This is not a second-class path: it is how the desktop runs, and it
works precisely because the kernel implements the real Linux syscall numbers rather than a
translation layer. Nothing in those projects is patched to know it is running on LeandrOS.

### Static musl

Rust crates (busd, greetd, `leandros-sysbus`, `mkfs.xfs`, `killmt`, `spawnwedge`) are built
with `cargo +nightly` for the musl targets using Rust's self-contained musl CRT and
`rust-lld`; no cross-gcc is needed. Sibling projects (brush, coreutils, bottom, disks-rs) link
through `zig cc -target <arch>-linux-musl` wrappers
([scripts/linker-aarch64-musl.sh](../../scripts/linker-aarch64-musl.sh)), with `cc`/`ar`
wrappers for crates that compile C (`AR` matters: cc-rs otherwise uses the macOS `ar`, whose
archives ld.lld cannot read, and every C symbol comes back undefined at link time).

The build recipes pass `-C relocation-model=static`. Left to its own devices, x86_64 musl
defaults to **static-PIE** (`ET_DYN`, first `PT_LOAD` at vaddr 0) while aarch64 defaults to
`ET_EXEC` — a silent per-arch asymmetry. The original LeandrOS loader mapped `p_vaddr`
literally with no bias, so a static-PIE binary landed on the null page and died. The loader
has since learned to load any `ET_DYN` main executable at a 2 MiB bias (`MAIN_DYN_BASE`, the
same address `ET_EXEC` images use) and musl's `rcrt1` self-relocates, so static-PIE now runs;
the flag remains as the convention because it makes both architectures produce the same kind
of image.

### Dynamic musl

The desktop needs dynamic linking: `dlopen` sits on the critical path in cosmic-comp's EGL
loading, Mesa's GBM/DRI loader and cosmic-panel's `use_system_lib`. So the image ships a real
`ld-musl-<arch>.so.1` and `libc.so` (musl 1.2.5), and the ported stack is dynamically linked
against them.

The kernel side is in [../kernel/processes.md](../kernel/processes.md): an `ET_DYN` main
executable loads at its bias, a `PT_INTERP` interpreter is mapped at `INTERP_BASE`
(768 MiB) and entered first, and the auxiliary vector carries `AT_BASE`, `AT_PHDR` and
`AT_ENTRY`. musl has **no** non-PIE dynamic executable — every dynamic program is
`ET_DYN` + PIE — so biasing ordinary executables was required, not optional.

Building that musl world on macOS had two traps worth recording:

- `zig cc` substitutes its bundled musl, forces static linking and refuses
  `-Wl,-dynamic-linker`, so the dynamic world compiles with `zig cc` but links with
  `zig ld.lld` directly.
- zig injects `libcompiler_rt`, whose *weak* `memcpy`/`memset`/libm symbols collide with
  musl's strong ones; ld.lld then demoted the *winning* strong symbol to local hidden and
  dropped it from `.dynsym`. The library looked structurally perfect and failed only at
  run-time symbol resolution. musl is therefore built with a patched compiler-rt copy whose
  colliding symbols are localised.

Alpine's prebuilt packages (Firefox, PipeWire, the portal frontend) link `libc.musl-<arch>.so.1`;
their ELFs are rewritten to the shipped `libc.so` soname. aarch64 GCC reads the stack canary
from a global `__stack_chk_guard`, which the shipped `libc.so` does not export, so a tiny
`libleandros_ssp.so.1` defining it is added as a `DT_NEEDED` to the libraries that import it;
the build fails if any shipped ELF still imports the guard without the shim.

## Threading (pthreads)

Full pthread support — `pthread_create`/`join`, mutexes, condition variables, thread-local
storage and cleanup handlers — is verified on both architectures.

Following the split real Linux libcs use, only a handful of generic primitives live in the
kernel, and everything POSIX-shaped is built on top of them in userspace:

- **Kernel primitives** — `clone` (the `CLONE_VM` branch spawns a true OS thread sharing the
  caller's address space, distinct from the `CLONE_VM`-clear `fork` path) and `futex`
  (`FUTEX_WAIT`/`FUTEX_WAIT_BITSET`, `FUTEX_WAKE`, and the PI family for musl/Firefox) are the
  thread-related kernel syscalls. See [../kernel/threads-and-futex.md](../kernel/threads-and-futex.md).
- **Userspace** — in relibc, `RlctMutex`/`RlctCondvar` (built on `futex`), a `#[thread_local]`
  TSD map with destructor-on-exit support, and `pthread_cleanup_push`/`pop`; in musl, its own
  equivalents. This is the same layering glibc and musl use on Linux: the kernel has no mutex,
  condvar or TSD syscalls of its own.
- Both `x86_64` and `aarch64` implement relibc's raw thread-spawn path (`rlct_clone`) that
  `pthread_create` calls into; a contended mutex, condvar wait/signal and TSD destructor
  ordering are exercised by `pthreadtest` in QEMU on both targets.

The libc-level contracts the kernel had to honour exactly are the instructive part: musl
needs `CLONE_PARENT_SETTID` for every thread's tid (`8897e02`; without it a keybinding spawn
deadlocked cosmic-comp — `userland/spawnwedge`), `CLONE_CHILD_CLEARTID` wakes for `join`,
`FUTEX_WAIT_BITSET`'s timespec is an **absolute** deadline (Rust std's `park_timeout` uses
it), and private futexes are keyed per process, not by address alone.

## Limitations & open work

- relibc's full sysroot (headers, `crt0`, C shims) is not produced by the build; C programs
  against relibc need the manual recipe.
- No in-tree crate is a shared library today; `scripts/build-userland.sh` emits static
  binaries only. A Rust `libpam.so.0` replacing the C lock-screen shim would be a new build
  shape ([../desktop/limitations.md](../desktop/limitations.md#login-and-privilege)).

## Code & tests

| Where | What |
|---|---|
| [userland/libc/src/](../../userland/libc/src/) | leandros-libc |
| [targets/](../../targets/) | the `<arch>-unknown-leandros` and kernel target specs |
| [scripts/build-userland.sh](../../scripts/build-userland.sh) | which programs link relibc vs leandros-libc |
| [scripts/build-all.sh](../../scripts/build-all.sh) | `build_relibc`, the musl builds of sibling projects |
| [elf/src/lib.rs](../../elf/src/lib.rs), [kernel/src/syscall.rs](../../kernel/src/syscall.rs) | loader bias, `PT_INTERP`, auxv |
| [userland/pthreadtest](../../userland/pthreadtest/), [userland/spawnwedge](../../userland/spawnwedge/), [userland/killmt](../../userland/killmt/) | thread-layer regressions |

## See also

- [README.md](README.md), [programs.md](programs.md), [ports.md](ports.md)
- [../kernel/processes.md](../kernel/processes.md) — the ELF loader and `execve`
- [../kernel/threads-and-futex.md](../kernel/threads-and-futex.md)
- [../getting-started/prerequisites.md](../getting-started/prerequisites.md) — the musl cross toolchain
