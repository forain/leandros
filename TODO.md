# LeandrOS Missing Functionality Implementation Plan

## Overview
This document outlines all missing functionality in the LeandrOS codebase that needs to be implemented. The system follows a phased development approach with clear markers for what functionality is still stubbed or incomplete.

## Priority 1: Critical System Components (Immediate Implementation)

### 1. Complete x86-64 Signal Handling (Phase 2) — DONE 2026-06-30
**Why Critical**: The kernel has signal infrastructure but only AArch64 is fully implemented. x86-64 signal delivery is incomplete.
- Implement full x86-64 signal frame restoration
- Complete signal delivery timing for x86-64
- Add proper signal stack handling
- Ensure both architectures are fully compatible

**Actual root cause found**: `check_and_deliver_signals()` had no caller anywhere in
either architecture's trap-return path — signal delivery was dead code on AArch64
too, not just x86-64 as originally described here. Fixed by wiring it into the EL0
return paths in `exception_asm.s` (syscall, fault, and IRQ, but not the EL1 IRQ
path, which must not deliver user signals) and into the x86-64 `syscall_entry`
return path in `arch/x86_64/src/syscall.rs`. Added a full x86-64 `rt_sigframe`
implementation (`mod x86_64` in `sched/src/signal.rs`) matching the SysV
`ucontext`/`mcontext` layout used by relibc's Linux-ABI signal trampoline
(`__restore_rt`).
A real bug was caught and fixed during QEMU testing: the inserted x86-64 call
clobbered `rax` (the live syscall return value) because the existing return path
restores `rax` from the register, not from the stack — fixed by stashing it in a
callee-saved register across the call.

**Security follow-up, fixed 2026-06-30**: AArch64's `rt_sigreturn` path restored
`spsr_el1` verbatim from the user-writable signal-stack frame, with no masking —
a malicious program could forge the `M[3:0]` mode field to request a return to
EL1 on the next `eret`, a privilege-escalation primitive. Fixed in
`sched/src/signal.rs` (`aarch64::restore`) by masking the restored value to the
N/Z/C/V condition flags only (`SPSR_NZCV_MASK`), forcing everything else —
exception level, AArch64/32 state, DAIF — back to the same `spsr_el1 == 0`
baseline a freshly created thread starts with. Mirrors the x86-64 path, which
never restores `cs`/`ss` from user memory and masks `rflags` the same way.

**`sigaltstack`, completed 2026-06-30**: was a stub on both architectures
(always reported `SS_DISABLE`, ignored the requested alt-stack, and reported
the wrong `SS_DISABLE` value — 4 instead of the real Linux/relibc value 2,
per `userland/relibc/src/header/signal/linux.rs`). Now real per-thread state:
`Task::altstack_{sp,size,flags}` (`sched/src/task.rs`), get/set through
`sched::{current_altstack, set_current_altstack}`, and a real
`sched::sys_sigaltstack()` implementing Linux's `do_sigaltstack()` semantics —
`SS_ONSTACK`/the `EPERM`-while-active check are derived from the live user SP
at syscall time via `on_altstack()`, not stored as separate state. Wired into
signal delivery on both architectures: `SA_ONSTACK` in `sigaction.sa_flags`
now redirects the signal frame onto the configured alt-stack
(`sigframe_base_sp()` in `sched/src/signal.rs`), falling back to the normal
stack if no alt-stack is configured/enabled or if already executing on it
(nested-signal case, matching Linux's `get_sigframe()`). New threads
(`fork`/`clone`) start with no alt-stack configured, matching Linux's
`copy_process()`, which resets `sas_ss_*` unconditionally for every new task.
This closes out Priority 1 item 1 — both signal delivery and `sigaltstack`
are now real on both architectures.

### 2. Complete Thread Management (Phase 4) — DONE 2026-07-01
**Why Critical**: Threads are fundamental to multitasking and application execution.
- Implement thread-local storage
- Add mutex and condition variable support
- Implement thread attributes and cleanup handlers
- Add thread-specific data support

**Architecture correction**: mutex/condvar, TSD (with destructors), and
pthread_cleanup_push/pop do **not** belong in the kernel — they're
POSIX/pthread-level concerns that belong in the C library sitting on top of
a handful of generic kernel primitives (`clone`, `futex`, TLS setup), the
same way glibc/musl do it on real Linux. `userland/relibc` already had a
correct, complete userspace implementation of all three: `RlctMutex`/
`RlctCondvar` (`src/sync/pthread_mutex.rs`, `src/sync/cond.rs`) built on the
kernel's `futex` syscall, a `#[thread_local]` TSD map with destructors
(`src/header/pthread/tls.rs`) wired into `pthread_exit`, and
`pthread_cleanup_push`/`pop` (`src/header/pthread/mod.rs`). A prior session
had started adding kernel-side `sys_pthread_key_create/delete/getspecific/
setspecific` syscalls, a `sched::tsd` module, and free-function
`FutexMutex`/`FutexCondvar` wrappers in `sched::mutex` to duplicate this —
that work didn't compile (14 rustc errors) and nothing in userland ever
called it, so it was reverted rather than fixed.

**Root cause of the real gap**: `pthread_create()` couldn't succeed at all,
on either architecture, because `Sys::rlct_clone` — the function that
actually spawns the OS thread via `clone()` — was a hard
`Err(Errno(ENOSYS))` stub in `userland/relibc/src/platform/leandros/mod.rs`
(a separate, ~half-finished parallel reimplementation of the Pal trait for
a custom `target_os = "leandros"`, alongside relibc's own mature, complete
`platform::linux::mod.rs` used for real Linux builds). Since this kernel
deliberately implements the real Linux syscall ABI (numbers, calling
convention all match — confirmed: `platform::leandros`'s own syscall-number
table was already identical to real Linux's), the fix was to stop
maintaining two Pal implementations and reuse `platform::linux::mod.rs`
directly:
- `platform::mod.rs`'s `sys` module routing now points `target_os =
  "leandros"` at `linux/mod.rs` (previously it pointed at its own
  `leandros/mod.rs`, a ~50%-stubbed parallel copy).
- `platform::linux::mod.rs` needs a `syscall!(NAME, ...)` macro and a
  `nr::NAME` syscall-number table, normally supplied by the external `sc`
  crate — but `sc`'s own internal `#[cfg(target_os = "linux")]` platform
  dispatch doesn't recognize this custom target, and widening the actual
  target JSON's `"os"` field to literally `"linux"` was tried and reverted:
  it cascades `cfg(target_os = "linux")` into unrelated third-party
  dependencies too (e.g. broke the `libc` crate, pulled in transitively by
  something else in the tree, which assumes a real hosted Linux). Instead,
  `platform::leandros::mod.rs` was trimmed down to just: the raw syscall
  trampolines (already had correct, working, tested aarch64/x86_64 asm) and
  a complete real-Linux syscall-number table (vendored verbatim from the
  small, dependency-free `sc` crate, Apache-2.0/MIT — its own internal
  target dispatch is what's unreachable for us, not the actual per-arch
  code) plus the handful of LeandrOS-only syscalls (IPC ports, `spawn`) that
  have no Linux equivalent and so live here as extra `impl Sys` methods
  rather than in the shared `Pal` trait. `linux/mod.rs`'s aarch64
  `rlct_clone` (previously `todo!()`) was implemented for real: raw `clone`
  syscall (nr 220), parent/child branch on the returned value, child
  unwinds the (entry_point, arg, tcb, mutex) tuple `pthread::create()`
  prepared on the new stack via `ldp`/`ldr` and `br`s into `new_thread_shim`
  — mirrors the existing x86_64 version's structure exactly.
- Also needed: `#[macro_export]` on the local `syscall!` macro plus
  declaring `leandros` (unconditionally providing it) *before* `sys` in
  `platform::mod.rs` so it's in scope for `linux/mod.rs` the same way
  `#[macro_use] extern crate sc;` used to work; ~35 call sites across
  `linux/mod.rs`/`signal.rs`/`socket.rs`/`ld_so/{mod,tcb}.rs` needed an
  explicit `unsafe {}` added or removed around `syscall!(...)` (Rust 2024
  edition requires it explicitly even inside `unsafe fn` bodies — some
  vendored call sites had it, some didn't, depending on whether `sc`'s own
  macro used to add it).
- One more real kernel-side bug, found only once threads could actually
  run: relibc's real futex usage calls `FUTEX_WAIT_BITSET` (op 9, with
  `FUTEX_BITSET_MATCH_ANY`) rather than plain `FUTEX_WAIT` (op 0) — the
  kernel's `sys_futex` (`kernel/src/syscall.rs`) only recognized op 0/1 and
  returned ENOSYS for 9. Fixed by treating 9 the same as 0 (the bitset is
  always match-any in practice here, so they're behaviorally identical for
  our purposes).
- Unrelated latent bug fixed in passing: `OsTid.thread_id`
  (`userland/relibc/src/pthread/mod.rs`) was `#[cfg(target_os =
  "linux")]`-gated only, but referenced unconditionally — widened to
  `any(linux, leandros)`.

**Verified 2026-07-01**, built from scratch against a from-scratch sysroot
(see the run-leandros skill session notes / project memory for the
toolchain recipe) and run in QEMU on **both** x86_64 and aarch64: a test
program exercising `pthread_create`/`join`, a mutex contended by two
threads (2000/2000 correct increments, no lost updates), a condvar
wait/signal, TSD `pthread_key_create`/`get`/`setspecific` with its
destructor firing on thread exit, and `pthread_cleanup_push`/`pop` — all
five passed cleanly on both architectures.

**What's Left**:
- [ ] Add automated/repeatable tests for this (the verification program
      from this session lived in the QEMU scratch environment, not
      committed anywhere — consider adding a real pthread test target to
      the build if ongoing regression coverage is wanted).
- [ ] `platform::leandros::mod.rs`'s 54-stub predecessor covered some Pal
      methods `platform::linux::mod.rs` may still be missing or may
      implement differently for non-thread-related syscalls (file I/O
      edge cases, etc.) — not exercised by this pass; worth a broader
      smoke test if VFS-heavy pthread-adjacent bugs show up later.

### 3. Complete Memory Management (Phase 6) — DONE 2026-07-01
**Why Critical**: Memory management is core to system operation.
- Implement heap management with malloc/free
- Add memory pools and advanced allocation strategies
- Complete memory mapping with proper VMA management
- Implement memory protection and sharing

**Actual state found**: this entry's framing was stale, matching items 1 and
2 above — heap allocation (a real buddy physical-page allocator plus a
slab-style `#[global_allocator]`, `mm/src/buddy.rs`/`slab.rs`), VMA tracking,
and `mmap`/`munmap`/`mprotect`/`brk` were already real and working on both
architectures before this session. The genuine gaps, found by reading the
code rather than trusting the bullet list, were narrower:

- **Buddy allocator never coalesced on free** (`mm/src/buddy.rs`) — freed
  blocks just went on the head of their own order's free list forever, so
  long-running alloc/free churn permanently fragmented memory. Fixed with
  doubly-linked free lists (next/prev stored inline in the freed page) and a
  merge-with-buddy loop in `free()`.
- **`sys_mremap`'s grow path silently zeroed data** instead of preserving it
  — a live bug, not theoretical: `userland/relibc`'s dlmalloc calls `mremap`
  directly for `realloc()`. Fixed by copying the overlapping bytes into the
  new mapping before releasing the old one.
- **Fork never did real copy-on-write** — `mm/src/cow.rs::clone_as` deep-copied
  every faulted-in page up front; `VmaRegion.cow` existed but was always
  `false` and never read. Replaced with real page-granular CoW: a new
  `mm/src/pageref.rs` refcounts physical pages that have ever been shared,
  `clone_as` now shares already-faulted pages read-only between parent and
  child (converting still-contiguous eager regions to the same per-page
  tracking lazy VMAs use, since each side can now diverge independently),
  and both architectures' page-fault handlers were widened to also route
  protection (not just not-present) faults through the recovery path with
  a write/read bit, promoting a page in place or copying it depending on
  the current refcount.
- **`MAP_SHARED` was accepted but silently downgraded to `MAP_PRIVATE`**
  everywhere, including the anonymous path (`map_lazy` hardcoded it). Now
  real for anonymous mappings whose pages are already faulted in at fork
  time (the realistic mmap-then-fork-to-share pattern): `clone_as` skips the
  CoW read-only downgrade for these regions and maps both sides with full
  permissions, refcounting only for correct teardown.

**Bug exposed and fixed in passing**: `sys_wait`/`sys_waitid` wrote a child's
exit status into the caller's pointer via a raw `core::ptr::write` from
kernel context. That pointer is very often the caller's own stack, which
now sits on a CoW-shared, read-only page immediately after `fork()` until
the parent's own user-mode code triggers promotion — and this kernel's
page-fault handlers don't attempt recovery for faults taken in
supervisor/EL1 mode (x86_64 printed a fatal "KERNEL EXCEPTION" on the
parent's own stack address right after the child exited). Fixed by routing
both through `AddressSpace::write_user_buf` (virt→phys/HHDM), which never
touches the raw PTE permission bit, instead of dereferencing the pointer
directly. Broader syscalls that still write to user pointers this way
were not swept — this fix only covers the two call sites this phase's own
test suite exercises.

**What's Left**:
- [ ] A page in a region that's never been touched before a fork, in either
      a private-CoW or a `MAP_SHARED` region, diverges independently per
      side on first touch afterward rather than staying genuinely shared —
      this needs the same VMO/backing-object refactor `unmap_range`'s
      existing middle-split leak (`mm/src/vmm.rs`) is already deferred to;
      not attempted here to avoid scope creep into that.
- [ ] File-backed `MAP_SHARED` (as opposed to anonymous) still has no page
      cache to share through — same VMO dependency as above.
- [ ] The kernel-mode-direct-pointer-write hazard above is a general pattern
      (`sys_waitid` had two more instances, both fixed; there may be others
      across `kernel/src/syscall.rs` not exercised by this session's test
      coverage) — worth a broader audit if more of these surface.

**Verified 2026-07-01** in QEMU on x86_64 and aarch64, both UEFI/Limine and
direct boot, via a new committed test binary
(`userland/memtest`, see `userland/memtest/src/main.rs`): fork'd
parent/child no longer see each other's writes to a pre-fork-touched page,
`mremap`-grow preserves content, an alloc/free churn loop across varied
sizes still allows a large allocation afterward, and `MAP_SHARED` siblings
do see each other's writes across a fork.

## Priority 2: Core System Integration (Phase 7)

### 4. Complete VFS Server Implementation (Phase 3/7) — DONE 2026-07-01
**Why Critical**: VFS is the foundation for file operations and user-space compatibility.
- Implement full file system operations (open, read, write, etc.)
- Add device file support and directory operations
- Implement file permissions and ownership
- Add file locking and advisory locking support

**Actual state found**: like items 1-3, this entry's framing was stale.
`servers/vfs/src/lib.rs` already had a substantially complete VFS
(open/read/write/close/lseek/stat/pipe/dup/getdents64/mkdir/unlink across
tmpfs, RAMFS, devfs, and mounted filesystems) — "device file support" and
most "directory operations" were already real, not stubbed. The genuine
gaps, found by reading the code rather than the bullet list:
- No `rmdir` at all — no protocol tag, no handler; `unlink()` explicitly
  refused directories, with no path to remove one.
- `rename()` only worked within `/tmp`; unlike `open`/`stat`/`unlink`/
  `mkdir`, it was never proxied to a mounted filesystem.
- No advisory locking anywhere: `flock(2)` was a kernel-side stub that
  always returned success; `fcntl(F_SETLK/F_GETLK/F_SETLKW)` silently
  no-op'd in the VFS server's fcntl fallthrough.
- No file permissions/ownership model: every file's `st_uid`/`st_gid` was
  always 0; `chmod`/`chown`/`setuid`/`setgid` were kernel-side no-ops that
  discarded their arguments and always reported success; `open()`'s `mode`
  argument was read but never stored or checked.

**What was implemented**:
- **rmdir**: new `VFS_RMDIR` tag + `handle_rmdir` (tmpfs, empty-check via
  prefix scan, EBUSY on built-in dirs, EROFS elsewhere); kernel's
  `sys_unlinkat` now honors `AT_REMOVEDIR` and routes accordingly; F2FS
  gained a real `handle_rmdir` (with an empty-directory check), and its
  `handle_unlink` now rejects directories with EISDIR instead of silently
  unlinking them.
- **rename to mounted filesystems**: `handle_rename` now proxies to a
  mount's IPC port when either path resolves under one (EXDEV if old/new
  resolve to different mounts); F2FS gained a `VFS_RENAME` dispatch +
  `handle_rename` (rejects an existing destination name — no
  overwrite-on-rename yet).
- **Advisory locking**: a shared lock table in `servers/vfs/src/lib.rs`,
  keyed by vnode identity (tmpfs slot, or mount port + file_id), backs both
  a real `flock()` (whole-file shared/exclusive, `LOCK_NB`) and a real
  byte-range `fcntl(F_GETLK/F_SETLK/F_SETLKW)` — both process-owned and
  released on `close`/process exit. flock and fcntl share one lock domain
  (stricter than real Linux's independent classes, never weaker). The
  user's `struct flock` is read/written via
  `AddressSpace::read_user_buf`/`write_user_buf` rather than a raw pointer
  dereference — the same kernel-mode CoW-page-fault hazard class fixed in
  Phase 6. Kernel's `FLOCK` stub replaced with a real `sys_flock`.
- **Permissions/ownership**: `TmpFileEntry` now carries real
  `mode`/`uid`/`gid`, set from the creating task's euid/egid and umask at
  `open(O_CREAT)`/`mkdir()` time (both previously discarded the `mode`
  argument entirely). `handle_open` enforces owner/group/other permission
  bits against the caller's euid/egid (root bypasses, matching Unix
  semantics). New `VFS_CHMOD`/`VFS_FCHMOD`/`VFS_CHOWN`/`VFS_FCHOWN` handlers
  (tmpfs only — everywhere else now honestly returns EROFS/EPERM instead of
  the previous silent-success lie) wired from the kernel's
  `FCHMODAT`/`FCHMOD`/`FCHOWNAT`/`FCHOWN` (previously hardcoded `=> 0`).
  `SETUID`/`SETGID` now really mutate the calling task's credentials
  (`sched::set_current_uid/gid`, real setuid(2) drop-privilege semantics —
  a root process can permanently drop to another uid but can't regain
  root), and `GETUID`/`GETEUID`/`GETGID`/`GETEGID` report the real values
  instead of a hardcoded 0. `stat()` reports real `st_uid`/`st_gid` for
  tmpfs entries.

**Real bug found and fixed in passing**: the kernel's `RENAMEAT`/
`RENAMEAT2` dispatch called `sys_renameat(a1, a2)` — `a2` is `renameat`'s
*new-directory-fd* argument (typically `AT_FDCWD`), not the new-path
pointer (`a3`). Every `rename()`/`renameat()` call in the system (including
via relibc, e.g. the shell's `mv`) was reading a bogus "path pointer" built
from `AT_FDCWD`'s bit pattern and failing with EFAULT. Caught by writing a
real end-to-end rename test rather than trusting the code read. Fixed to
`sys_renameat(a1, a3)`.

**New test coverage**: `userland/vfstest` (mirrors the Phase 6 `memtest`
pattern) — `rmdir` (empty/non-empty/ENOTEMPTY), `rename` (move + content
check), `flock_conflict` (cross-process EAGAIN denial + release/reacquire
via `fork()`), `fcntl_byte_range_conflict` (cross-process byte-range denial
+ `F_GETLK` conflict reporting), `permission_enforced` (setuid privilege
drop, EACCES on a 0600 file, and confirming root can't be regained).
`leandros-libc` gained the corresponding syscall wrappers (`rmdir`,
`rename`, `chmod`/`fchmod`, `chown`/`fchown`, `flock`, `fcntl_lock`,
`setuid`/`setgid`) it didn't have before.

**What's Left**:
- [ ] Permissions/ownership and locking only cover tmpfs; F2FS-mounted
      files have no chmod/chown/lock support (F2FS inodes don't carry
      uid/gid on disk yet) — same on-disk-format dependency other F2FS gaps
      share.
- [ ] `rename()` across two different mounts (or between `/tmp` and a
      mount) returns EXDEV rather than a copy+unlink fallback — matches
      Linux's own syscall behavior, but userspace (`mv`/coreutils) would
      need to implement that fallback itself, same as on real Linux.
- [ ] Discovered, not fixed (out of scope for this pass): `servers/init`'s
      `run_posix_tests()`/`init_main()` — a substantial POSIX smoke-test
      suite (tmpfs, pipes, dup2, getdents64, /proc, shell pipe/redirect) —
      is dead code. `kernel/src/init.rs`'s own doc comment claims it "hands
      off to `init_server::init_main()`", but nothing in the kernel actually
      calls it; the real boot path spawns the separate `userland/init` ELF,
      which `execve`s straight into `bin/shell`. Worth fixing later, but
      wiring dead code back up wasn't part of this task's scope.

**Verified 2026-07-01** in QEMU on x86_64 and aarch64, both UEFI/Limine and
direct boot (4 configurations): `vfstest` (5/5) and `memtest` (4/4,
regression check) both pass cleanly on every configuration; the interactive
shell's `ls`/`mkdir` still work normally afterward.

### 5. Complete F2FS Implementation (Phase 7)
**Why Critical**: File system support is essential for data persistence.
- Implement double-indirect block pointers
- Add full F2FS file system operations
- Implement file system mounting and unmounting
- Add file system caching and buffering

## Priority 3: User Experience Features (Phase 8-9)

### 6. POSIX Timers (Phase 8)
**Why Critical**: Timers are essential for applications and system scheduling.
- Complete timer creation and expiration
- Implement timer signal delivery
- Add timer synchronization primitives
- Implement timer accounting and statistics

### 7. Poll/Select/Epoll (Phase 9)
**Why Critical**: Event notification is fundamental for I/O multiplexing.
- Complete poll implementation
- Implement select with proper event handling
- Complete epoll with efficient event notification
- Add performance optimizations for large fd sets

## Priority 4: Additional System Components

### 8. Network Stack Implementation (net server)
**Why Important**: Network connectivity is essential for modern systems.
- Implement complete network protocol support (TCP, UDP)
- Add socket operations (bind, connect, listen, accept)
- Implement network device drivers
- Add network configuration support

### 9. Audio Server Implementation
**Why Important**: Audio support provides user experience.
- Implement complete audio server with device support
- Add audio format and mixing support
- Implement audio streaming capabilities
- Add audio configuration and control

### 10. Input Device Support
**Why Important**: Input devices are essential for user interaction.
- Implement complete input device drivers
- Add input event handling and processing
- Implement input device abstraction and configuration
- Add support for various input device types

### 11. Graphics Subsystem
**Why Important**: Graphics support is essential for visual applications.
- Implement complete graphics drivers
- Add graphics API support and GPU acceleration
- Implement graphics memory management
- Add display management capabilities

## Implementation Strategy

### Phase-by-Phase Approach:
1. **Phase 2 (Signal Handling)** - Start with x86-64 signal delivery
2. **Phase 4 (Thread Management)** - Implement thread-local storage and synchronization
3. **Phase 6 (Memory Management)** - Add heap management and memory pools
4. **Phase 7 (VFS Server)** - Complete file system operations
5. **Phase 8 (POSIX Timers)** - Implement timer functionality
6. **Phase 9 (Poll/Select/Epoll)** - Complete event notification systems
7. **Phase 3 (VFS Integration)** - Complete VFS server implementation
8. **Network Stack** - Implement net server
9. **Audio Server** - Implement audio server
10. **Input Device Support** - Implement input drivers
11. **Graphics Subsystem** - Implement graphics drivers

### Key Implementation Considerations:
1. **Architecture Consistency** - Ensure both AArch64 and x86-64 are fully supported
2. **Performance** - Optimize critical paths for system calls
3. **Security** - Implement proper access controls and memory protection
4. **Compatibility** - Maintain Linux ABI compatibility where possible
5. **Testing** - Implement comprehensive unit and integration tests

## Todo List in Order of Priority

1. Complete x86-64 Signal Handling (Phase 2)
2. Complete Thread Management (Phase 4)
3. Complete Memory Management (Phase 6)
4. Complete VFS Server Implementation (Phase 3/7)
5. Complete F2FS Implementation (Phase 7)
6. POSIX Timers (Phase 8)
7. Poll/Select/Epoll (Phase 9)
8. Network Stack Implementation (net server)
9. Audio Server Implementation
10. Input Device Support
11. Graphics Subsystem