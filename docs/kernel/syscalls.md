# System calls

LeandrOS began with a ten-call native ABI built around IPC ports. It has since grown a **Linux syscall personality**: the dispatcher answers the real Linux syscall numbers of each architecture, so relibc's Linux platform layer and stock `*-unknown-linux-musl` binaries — Mesa, D-Bus, COSMIC, Firefox — run without source patches. This page covers the register conventions, the original native table and where it went, the per-architecture numbering, the private calls that remain, the dispatcher's structure, the single path resolver, and the errno conventions.

## Overview

| Item | Value |
|---|---|
| Dispatcher | `syscall_dispatch` → `dispatch` → `dispatch_inner` in [kernel/src/syscall.rs](../../kernel/src/syscall.rs) (~11.8k lines; older docs say ~7.5k) |
| Linux numbers named | ~219 on AArch64, ~208 on x86-64 (`mod nr`, one per architecture) |
| Private calls | 510–523 and 590, identical on both architectures |
| Unknown number | `ENOSYS`, logged unconditionally: `[SYSCALL] ENOSYS nr=… pid=…` |
| Interrupt state | IRQs masked for the whole call ([scheduler.md](scheduler.md#preemption-and-the-kernels-interrupt-model)) |

## How it works

### Register conventions

Both follow Linux:

| Arch | Entry | Number | Arguments | Return |
|---|---|---|---|---|
| AArch64 | `svc #0` → `exc_el0_sync` | `x8` | `x0`–`x5` | `x0` |
| x86-64 | `syscall` (STAR/LSTAR/FMASK MSRs) | `rax` | `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9` | `rax` |

The trap stubs save a `UserFrame` on the kernel stack and pass its address as `frame_ptr`, which signal delivery, `rt_sigreturn`, `clone` and `execve` use. On x86-64 the entry switches to a per-CPU kernel stack with `swapgs` ([arch-x86_64.md](arch-x86_64.md#syscall-entry-and-per-cpu-stacks)); on AArch64 the exception vector already runs on `SP_EL1` ([arch-aarch64.md](arch-aarch64.md#exception-vectors-and-the-el1-fault-path)). Return values are `-errno` in the Linux style.

### The original native ABI

The first ABI was ten calls, numbered from 0:

| Number | Name | Args | Returns |
|---|---|---|---|
| 0 | `send` | port, msg_ptr | 0 / errno |
| 1 | `recv` | port, msg_ptr | 0 / errno |
| 2 | `call` | port, msg_ptr | 0 / errno |
| 3 | `map_mem` | virt, size, flags | 0 / errno |
| 4 | `unmap_mem` | virt, size | 0 |
| 5 | `yield` | — | 0 |
| 6 | `exit` | code | — |
| 7 | `spawn` | entry_va, stack_va, priority | pid / errno |
| 8 | `clock_gettime` | dest_ptr | 0 / errno |
| 9 | `wait` | pid, status_ptr | 0 / errno |

Those numbers collided with Linux's (0 is `read` on x86-64), so when the personality arrived the native calls moved. Today's mapping: the IPC primitives are private calls 511–513; `map_mem`/`unmap_mem` became `mmap`/`munmap`/`mprotect`/`brk`; `yield`, `exit`, `clock_gettime` and `wait` are `sched_yield`, `exit`/`exit_group`, `clock_gettime` and `wait4`/`waitid` at their Linux numbers; `spawn` survives as private call 510.

### Private calls

| Number | Name | Purpose |
|---|---|---|
| 510 | `SYS_SPAWN` | Start a task at an entry point and stack in the caller's address space |
| 511 / 512 / 513 | `SYS_IPC_SEND` / `RECV` / `CALL` | Port IPC ([ipc.md](ipc.md)) |
| 514 | `SYS_PORT_CREATE` | Create a port owned by the caller (servers create theirs kernel-side; this exists so `racetest` can exercise the raw blocking path) |
| 515–520, 523 | `SYS_BLKDEV_COUNT/INFO/NAME`, `SYS_PCIDEV_COUNT/INFO`, `SYS_USBDEV_COUNT/INFO` | Device enumeration behind `lsblk`, `lspci`, `lsusb` (Linux does this through sysfs and ioctls) |
| 521 / 522 | `SYS_MOUNTS_COUNT/INFO` | The live mount table for `mount`/`lsblk` |
| 590 | `SYS_DBG_SERIAL_WRITE` | Gated debug: write a short user string straight to the serial transmitter, framed, so a checkpoint survives a compositor owning the console |

Everything else uses Linux numbers. Programs written for LeandrOS reach the private calls through `leandros-libc`; ported software never needs them.

### The Linux personality and per-architecture numbers

The two architectures have genuinely different numbers for the same call — `mincore` is 232 on AArch64 and 27 on x86-64, `clock_gettime` 113 and 228, `mmap` 222 and 9 — and AArch64 lacks the legacy calls entirely (no `open`, `stat`, `fork`, `poll`; only the `*at` and `p*` forms). The tables are two `cfg`-gated `mod nr` blocks, and the dispatcher matches on the constant names, so the match arms are shared and only the numbers differ. Getting a number backwards is a class of bug this project has paid for more than once; the defence is that every suite runs on both architectures.

Coverage, by area (each links to the page that owns the semantics):

- memory: `mmap`, `munmap`, `mprotect`, `brk`, `mremap`, `mincore`, `madvise`, `membarrier`, `get_mempolicy`/`set_mempolicy`/`mbind` for one node ([memory.md](memory.md));
- processes and threads: `clone` (incl. `CLONE_PIDFD`), `fork`/`vfork` (x86-64), `execve`, `exit`, `exit_group`, `wait4`, `waitid` (incl. `P_PIDFD`), `pidfd_open`/`pidfd_send_signal`/`pidfd_getfd`, `set_tid_address`, `rseq`, `prctl`, credentials, `chroot`, `pivot_root`, priorities and rlimits ([processes.md](processes.md), [threads-and-futex.md](threads-and-futex.md));
- signals: `rt_sigaction`, `rt_sigprocmask`, `rt_sigreturn`, `sigaltstack`, `kill`/`tkill`/`tgkill`, `rt_sigqueueinfo`, `signalfd` ([signals.md](signals.md));
- time: `clock_gettime`, `clock_nanosleep`, `nanosleep`, POSIX timers, `timerfd`, `restart_syscall` ([time.md](time.md));
- I/O and files: the full VFS surface, `openat`/`faccessat2`/`renameat2`/`utimensat`, xattrs, `getdents64`, pipes, `eventfd`, `inotify`, `memfd_create`, `close_range`, `sendfile`, `splice`/`tee`/`vmsplice` ([servers/vfs.md](../servers/vfs.md));
- event loops: `poll`/`ppoll`, `select`/`pselect6`, `epoll_*` ([event-loop.md](event-loop.md));
- sockets: AF_UNIX, AF_INET (TCP/UDP/ICMP), `SCM_RIGHTS` ([servers/networking.md](../servers/networking.md));
- misc: `uname`, `getrandom`, `reboot`, `getcpu`, `getrusage`, `capget`/`capset`.

Deliberate non-implementations answer the way Linux would for the caller: `clone3` returns `ENOSYS` so callers take their portable fallback (`clone`); root-only operations the kernel does not implement (`acct`, `swapon`, module loading, `kexec`, `iopl`/`ioperm`) return `EPERM` to an unprivileged caller, as Linux's capability check would, and `ENOSYS` to root.

### Dispatcher structure

`syscall_dispatch` (called by the trap stubs) wraps `dispatch`, which wraps `dispatch_inner`:

1. **`dispatch_inner`** first canonicalises socket descriptors (`sock_alias_args`: socket fds live in their own number range from `SOCK_FD_BASE` = 0x200 and are routed to the net server; `not_a_socket` makes socket calls on non-sockets answer `ENOTSOCK`, not `EBADF`, because libpulse and others branch on the difference), then matches the number.
2. **`dispatch`** records the syscall per CPU and per pid for the watchdog and Ctrl-T (`note_syscall_enter/exit`), runs the gated tracers (`SYSCALL_TRACE`, `…_EINVAL`, `…_EACCES`, `…_EBADF`, `SC_STATS`), **checks the kernel-stack canary** (an overflow is named at the syscall that caused it and the task is killed — [invariants.md](invariants.md#kernel-stacks-are-128-kib-unguarded-and-policed)), fires expired POSIX timers, and services a VT switch requested from IRQ context.
3. **`syscall_dispatch`** converts the internal restart codes: `ERESTARTSYS` (−512), `ERESTARTNOHAND` (−514) and `ERESTART_RESTARTBLOCK` become `EINTR` in the frame and are recorded so the signal pass can rewind and re-execute the call (or `restart_syscall`, which resumes the stored absolute deadline) exactly when Linux would ([signals.md](signals.md)).

### One path resolver

Every path-taking syscall funnels through one resolver ([kernel/src/syscall.rs](../../kernel/src/syscall.rs)):

- **`resolve_user_path(ptr)`** copies the user string (prefaulted read-only), rejects the empty path with `ENOENT` (Linux's answer for `open("")`; `AT_EMPTY_PATH` is handled separately), joins a relative path onto the task's cwd, normalises `.`/`..`, and returns a `KPath` — an absolute, NUL-terminated path in **kernel** memory.
- **`resolve_at_path(dirfd, ptr)`** implements the `*at()` family: `AT_FDCWD` and absolute paths go through the cwd path; a relative path with a real dirfd is joined onto the absolute path that descriptor was opened by (the same `VFS_FD_PATH` query that answers `readlink("/proc/self/fd/N")`), falling back to cwd-relative behaviour when the descriptor names no path rather than failing.
- **chroot confinement** lives in the same place: an absolute path is anchored at the jail root, `..` can never climb above it, and a dirfd's host-absolute path is translated back into the jail's view before joining (otherwise the root prefix would be applied twice).
- `symlink(2)`'s *target* is copied verbatim (`read_user_cstr_kpath`), never resolved, so `ln -s ../x l` stays relative to the link's directory.

The servers receive the `KPath` pointer in place of the user pointer. That works because kernel memory is mapped in every address space and the caller stays blocked in `call_port` for the whole round trip, so the buffer outlives every server that reads it. Symlink following and per-component permission checks happen in the VFS and f2fs resolution that consumes the path ([servers/vfs.md](../servers/vfs.md)). The path limit is `KPATH_MAX` = 256 bytes, kept in lockstep with the server-side readers.

### errno conventions

- **An errno names its cause.** `ENOMEM` in the memory, address-space and exec paths goes through `enomem_site` ([memory.md](memory.md#enomem-must-name-its-cause)); fixed tables report themselves full in one line.
- **Match Linux exactly**, including which error wins when several apply (search permission before `EEXIST` before write permission for creation, `EINTR` for interrupted timed waits regardless of `SA_RESTART`, `ENOTSOCK` vs `EBADF`). See [overview/design-principles.md](../overview/design-principles.md).
- **`ENOSYS` is never silent.** It is the most common way a ported binary dies with no output, so every unknown number is logged.
- **A stub that lies is worse than `ENOSYS`.** `mincore` once returned success for any address ("pretend all pages resident"); Mesa uses `mincore` to test whether a pointer is dereferenceable, took the integer 3 for a `wl_surface*`, and crashed in libwayland at address `0x1B`. It now returns `ENOMEM` when any page in the range is unmapped.

## Design decisions

### Real Linux numbers, not a custom scheme

*Choice:* implement Linux's syscall numbers and structure layouts per architecture. *Why:* it lets relibc's `platform::linux` work unmodified and lets a stock musl cross-compile run with no port work beyond building it — which is how Mesa, D-Bus, PipeWire, COSMIC and Firefox run here without source patches ([overview/design-principles.md](../overview/design-principles.md)). *Alternative rejected:* a LeandrOS-specific ABI with a translation layer in libc. That would need a patched libc for every language runtime (Rust's std, Go, Mesa's own raw syscalls) and would never be bug-compatible with what those programs were tested against. *Cost:* two numbering tables to keep straight, and Linux's semantics to match edge case by edge case.

### The dispatcher is one big match

The match arms read like Linux's `syscall_64.tbl` grouped by subsystem, and each arm is a direct call. The alternative — a table of function pointers — would not survive the per-architecture differences in argument order (`clone`'s TLS and child-tid arguments are swapped between x86-64 and AArch64, for instance) without wrappers anyway.

### One resolver for every path syscall

Before the resolver existed only `open`, `chdir`, `execve` and `pivot_root` resolved anything; every other path syscall forwarded the raw user string, so the VFS saw `a.txt`, matched nothing and answered `ENOENT` — `cd /tmp; ls a.txt` failed while `ls /tmp/a.txt` worked. Ignoring `dirfd` broke every TOCTOU-safe traversal (GNU fts, `uucore::safe_traversal`): `rm /tmp/a/f` from another cwd did `unlinkat(dirfd, "f")` and got `ENOENT` on a file that existed. `renameat` honouring its dirfds, not just its paths, was the difference between a clean COSMIC session log and a storm of `ENOENT`s from atomic state-file writes. Having one resolver means chroot confinement and `AT_*` handling are implemented once.

## Limitations and open work

- `KPATH_MAX` is 256 bytes, far below Linux's 4096.
- No vDSO: `clock_gettime` and `gettimeofday` are real syscalls. On AArch64 a one-page `rt_sigreturn` trampoline is mapped at `0x7fff_ff00_0000` instead (musl installs handlers without `SA_RESTORER`).
- `getcpu` reports CPU 0; `sched_setaffinity` is accepted and ignored ([scheduler.md](scheduler.md#affinity)).
- Several pointer arguments are still validated only shallowly (ioctl `arg` for block/DRM/tty, `getdents64`/`getcwd` buffers checked for one byte, `mount`, the `memfd_create` name), and some server paths still dereference user memory under their own locks (lane uptrfault's list); a bad pointer there kills the caller instead of returning `EFAULT`.
- epoll: nested-epoll `ELOOP` and `EPOLLEXCLUSIVE` `EINVAL` checks are missing; `CLOSE_RANGE_UNSHARE` in a multithreaded caller lets siblings see the closes ([TODO.md](../../TODO.md)).

## Code and tests

| What | Where |
|---|---|
| Dispatcher, numbering, private calls, resolver, `mmap`, `execve` | [kernel/src/syscall.rs](../../kernel/src/syscall.rs) |
| Entry stubs | [arch/x86_64/src/syscall.rs](../../arch/x86_64/src/syscall.rs), [arch/aarch64/src/exception_asm.s](../../arch/aarch64/src/exception_asm.s) |
| Userspace wrappers for private calls | [userland/libc](../../userland/libc) |

Every regression suite is, in effect, a syscall conformance test; `uptrtest` drives 26 syscalls against lazy, file-backed, CoW and stack buffers plus deliberate `EFAULT` cases; `vfstest` covers the resolver (`*at` with real dirfds, chroot confinement, symlinks across mounts); `privtest` and `nettest` are C sources compiled unchanged for a Linux host and for LeandrOS, so every expectation in them has been checked against Linux. See [development/testing.md](../development/testing.md).

## See also

- [ipc.md](ipc.md) — the native IPC calls
- [processes.md](processes.md) — `clone`, `execve`, `wait4`
- [servers/README.md](../servers/README.md) — where most of the Linux surface is implemented
- [arch-x86_64.md](arch-x86_64.md), [arch-aarch64.md](arch-aarch64.md) — the entry paths
