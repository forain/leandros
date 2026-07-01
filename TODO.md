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

### 3. Complete Memory Management (Phase 6)
**Why Critical**: Memory management is core to system operation.
- Implement heap management with malloc/free
- Add memory pools and advanced allocation strategies
- Complete memory mapping with proper VMA management
- Implement memory protection and sharing

## Priority 2: Core System Integration (Phase 7)

### 4. Complete VFS Server Implementation (Phase 3/7)
**Why Critical**: VFS is the foundation for file operations and user-space compatibility.
- Implement full file system operations (open, read, write, etc.)
- Add device file support and directory operations
- Implement file permissions and ownership
- Add file locking and advisory locking support

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