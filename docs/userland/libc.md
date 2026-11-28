# C libraries

LeandrOS userland runs on two C runtimes: **leandros-libc**, a minimal in-tree runtime for
small `no_std` programs, and **musl**, both static and as a real dynamic `ld-musl`, which is
what every ported program, the whole desktop and the in-tree tests that need a complete POSIX
library run on. Both issue the same Linux-numbered syscalls; neither translates anything.
(The Redox `relibc` used to be a third runtime; it was retired once every test moved to static
musl, and the `../relibc` sibling checkout is no longer needed.)

## Overview

| Library | Where | Used by | Linkage |
|---|---|---|---|
| leandros-libc | [userland/libc](../../userland/libc/src/lib.rs) | `init`, `login`, `shell`, `greeter-launch`, `mount`, `aplay`, `vfstest`, DOOM, … | static, target `<arch>-unknown-none` |
| musl 1.2.5 | Rust's self-contained musl, Alpine's musl (static, via the clang wrapper), and a shipped `ld-musl` + `libc.so` | brush, coreutils, busd, greetd, COSMIC, Mesa, PipeWire, Firefox, MAME, and the `MUSL_LINKED` tests in [scripts/build-userland.sh](../../scripts/build-userland.sh) | static or dynamic |

The `MUSL_LINKED` tests (`pthreadtest`, `sigtest`, `timertest`, `polltest`, `drmsmoke`, `vttest`,
`ping`, …) are `no_std` crates linked against static `<arch>-unknown-linux-musl` through the clang/lld
linker wrapper; their start-up is musl's `crt1` and `userland/muslrt` holds the shared pieces.

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

Rust crates (busd, greetd, `leandros-sysbus`, `killmt`, `spawnwedge`) are built
with `cargo +nightly` for the musl targets using Rust's self-contained musl CRT and
`rust-lld`; no cross-gcc is needed. Sibling projects (brush, coreutils, bottom, disks-rs) link
through `scripts/toolchain/<arch>-linux-musl-cc` wrappers
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

- The dynamic world compiles with clang against the merged sysroot and links with
  `ld.lld` directly (`scripts/toolchain/dyn/musl-dyn-link.sh`); the retired zig toolchain
  forced static linking and refused `-Wl,-dynamic-linker`.
- The retired zig toolchain injected `libcompiler_rt`, whose *weak* `memcpy`/`memset`/libm symbols collide with
  musl's strong ones; ld.lld then demoted the *winning* strong symbol to local hidden and
  dropped it from `.dynsym`. The library looked structurally perfect and failed only at
  run-time symbol resolution. musl is therefore built with a patched compiler-rt copy whose
  colliding symbols are localised.

#### Alpine musl as the runtime libc

The image's libc is unmodified Alpine
`musl` package (the version and sha256 are pinned in `ports/alpine/<arch>.lock`; cached `.apk` under
`~/.cache/leandros/apk/<arch>/`) and is owned by apk as a real package
([ports.md](ports.md#live-root-apk)). The previous zig-built musl 1.2.5 libc (and its `LEANDROS_LIBC=zig` mode,
`relink-libc.sh`, `dynlist-cc.sh`, `rtlib-shim`) was retired in 2026-11; the
exported dynsym matched it, and `scripts/musl/alpine-parity.sh` (run by `build-all.sh`) fails
the build if the Alpine libc drifts from the golden list (`alpine-parity.<arch>.golden`;
FUNC sizes are not compared, Alpine builds `-Os`). Layout:

- `/lib/ld-musl-<arch>.so.1`: Alpine's regular file, the only libc inode (the kernel never
  resolves a symlink for `PT_INTERP`);
- `/lib/libc.musl-<arch>.so.1` and `/usr/lib/libc.so`: symlinks to it (the latter is owned by
  no package, so `apk add musl-dev` may take it over);
- `/lib/ld-musl-<arch>.so.1.recovery`: a byte copy of Alpine's loader (own inode, unowned by
  any package), the recovery copy. If the loader is damaged, from the static shell:
  `/usr/sbin/busybox.static cp /lib/ld-musl-<arch>.so.1.recovery /lib/ld-musl-<arch>.so.1`.

`apk del musl` is refused; `musl` is pinned in
`/etc/apk/world`, so any bump (edge, musl 1.2.6) needs the full test set. The build sysroot,
the Firefox and git ports still apply the rewrite below (a parallel lane replaces them). Alpine's loader is
stripped; install `musl-dbg` to symbolize it.

Alpine's prebuilt packages link `libc.musl-<arch>.so.1`, which is now a symlink to Alpine's own
loader, so the repackaged PipeWire, GStreamer, portal frontend and GPU Mesa stage ship their ELFs
unmodified: no soname rewrite and no `libleandros_ssp.so.1` `DT_NEEDED` injection. The loader
exports `__stack_chk_guard` (aarch64 GCC reads the canary from that global), and each of those
build scripts fails if the Alpine `ld-musl` it builds against does not export it. The Firefox
and git ports still rewrite to `libc.so` and add the shim until they move to vanilla apk packages.
`libleandros_ssp.so.1` stays in the image because older in-tree binaries may `DT_NEEDED` it.

## Threading (pthreads)

Full pthread support — `pthread_create`/`join`, mutexes, condition variables, thread-local
storage and cleanup handlers — is verified on both architectures.

Following the split real Linux libcs use, only a handful of generic primitives live in the
kernel, and everything POSIX-shaped is built on top of them in userspace:

- **Kernel primitives** — `clone` (the `CLONE_VM` branch spawns a true OS thread sharing the
  caller's address space, distinct from the `CLONE_VM`-clear `fork` path) and `futex`
  (`FUTEX_WAIT`/`FUTEX_WAIT_BITSET`, `FUTEX_WAKE`, and the PI family for musl/Firefox) are the
  thread-related kernel syscalls. See [../kernel/threads-and-futex.md](../kernel/threads-and-futex.md).
- **Userspace** — in musl, its own mutex, condvar, TSD and cleanup-handler implementation. This is the same layering glibc and musl use on Linux: the kernel has no mutex,
  condvar or TSD syscalls of its own.
- `pthreadtest` exercises contended mutexes, condvar wait/signal and TSD destructor ordering in QEMU on both targets.

The libc-level contracts the kernel had to honour exactly are the instructive part: musl
needs `CLONE_PARENT_SETTID` for every thread's tid (`8897e02`; without it a keybinding spawn
deadlocked cosmic-comp — `userland/spawnwedge`), `CLONE_CHILD_CLEARTID` wakes for `join`,
`FUTEX_WAIT_BITSET`'s timespec is an **absolute** deadline (Rust std's `park_timeout` uses
it), and private futexes are keyed per process, not by address alone.

### Thread exit and fork (October 2026 fixes)

- **Exited threads are freed.** the old relibc never unmapped an exited thread's 1 MiB pthread stack or its TCB/TLS mapping, so every thread left two VMAs and about 1 MiB of address space behind (`pthreadtest churn 5000`: VMAs 208 to 10208 and VmSize 113 MB to 5.29 GB; after the fix 8 VMAs and 9544 kB, flat). The kernel was innocent. The review follow-up blocks all signals and disables cancellation before a thread exits (as musl's `pthread_exit` does), makes `pthread_detach` of an already-exited thread push it on the zombie list instead of blocking, and has a fork child forget the parent's zombie list.
- **`fork()` holds the libc thread-map lock (historical, relibc).** `pthread::create` inserts into `OS_TID_TO_PTHREAD` (a futex mutex) after the clone; a fork from another thread at that moment gave the child a locked mutex and its first `pthread_create` hung forever. `fork` now takes the lock around the fork, and `epolltest churn 20 tfork` is in `regress`.
- **Affinity.** `sched_getcpu`, `sched_setaffinity`/`getaffinity` and `waitid` exist in the libc, and the scheduler honours the mask (placement, wake CPU choice, EEVDF pick, load balance).
- **Static musl `memset`** on aarch64: static binaries link Alpine's `libc.a`, whose `mem*` are musl's assembly versions (the retired zig toolchain's compiler_rt `memset` was a byte loop).

## Limitations & open work

- No in-tree crate is a shared library today; `scripts/build-userland.sh` emits static
  binaries only. A Rust `libpam.so.0` replacing the C lock-screen shim would be a new build
  shape ([../desktop/limitations.md](../desktop/limitations.md#login-and-privilege)).

## Code & tests

| Where | What |
|---|---|
| [userland/libc/src/](../../userland/libc/src/) | leandros-libc |
| [targets/](../../targets/) | the kernel target specs and `x86_64-unknown-leandros` (hard-float leandros-libc for DOOM) |
| [scripts/build-userland.sh](../../scripts/build-userland.sh) | which programs link musl vs leandros-libc |
| [scripts/build-all.sh](../../scripts/build-all.sh) | the musl builds of sibling projects |
| [elf/src/lib.rs](../../elf/src/lib.rs), [kernel/src/syscall.rs](../../kernel/src/syscall.rs) | loader bias, `PT_INTERP`, auxv |
| [userland/pthreadtest](../../userland/pthreadtest/), [userland/spawnwedge](../../userland/spawnwedge/), [userland/killmt](../../userland/killmt/) | thread-layer regressions |

## See also

- [README.md](README.md), [programs.md](programs.md), [ports.md](ports.md)
- [../kernel/processes.md](../kernel/processes.md) — the ELF loader and `execve`
- [../kernel/threads-and-futex.md](../kernel/threads-and-futex.md)
- [../getting-started/prerequisites.md](../getting-started/prerequisites.md) — the musl cross toolchain
