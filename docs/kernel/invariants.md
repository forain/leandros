# Kernel invariants

The hard rules the kernel depends on, each with the failure that taught it. Breaking most of these does not produce an error — it produces a silent whole-machine freeze, a wrong answer, or corruption that surfaces minutes later somewhere unrelated — so they are checked in review and, where possible, by a test or a build step. The cross-cutting project principles these rest on are in [overview/design-principles.md](../overview/design-principles.md).

## The rules at a glance

| # | Rule | Enforced by |
|---|---|---|
| 1 | [Never touch user memory under an IRQ-off spinlock](#never-touch-user-memory-under-an-irq-off-spinlock) | Review; `uptrtest`; fault-tolerant `uaccess` copies |
| 2 | [Publish `Blocked` before checking the condition](#publish-blocked-before-checking-the-condition) | The shared three-phase helpers; `smpwaketest`, `racetest` |
| 3 | [Never yield, park or sleep while holding a `spin::Mutex`](#never-yield-park-or-sleep-while-holding-a-spinmutex) | `preempt_disable`; `might_sleep()` for `KSpinLock`/`TrackedMutex` |
| 4 | [IRQ context and tick hooks are `try_lock`-only](#irq-context-and-tick-hooks-are-try_lock-only) | Review; the watchdog names the lock if violated |
| 5 | [No file I/O under a filesystem or address-space lock](#no-file-io-under-a-filesystem-or-address-space-lock) | Prefault before the lock; unlocked fault reads |
| 6 | [No `unsafe` globals beyond boot and per-CPU state](#no-unsafe-globals-beyond-boot-and-per-cpu-state) | Review |
| 7 | [Checked arithmetic at memory boundaries](#checked-arithmetic-at-memory-boundaries) | Review |
| 8 | [Time conversions never make a wait early](#time-conversions-never-make-a-wait-early) | `timertest`, `polltest` latency case |
| 9 | [Kernel stacks are 128 KiB, unguarded, and policed](#kernel-stacks-are-128-kib-unguarded-and-policed) | `check-stack-frames.py` (build), canary (every syscall return) |
| 10 | [The kernel uses no FP/SIMD](#the-kernel-uses-no-fpsimd) | Soft-float target specs |
| 11 | [Detach a task's page table before it can be freed](#detach-a-tasks-page-table-before-it-can-be-freed) | Scheduler switch-back path |
| 12 | [Debug output is gated and off by default](#debug-output-is-gated-and-off-by-default) | `const …: bool = false` flags |
| 13 | [An errno names its cause, and a full table says so](#an-errno-names-its-cause-and-a-full-table-says-so) | `enomem_site`, ungated "FULL" lines |
| 14 | [Fatal faults end the whole thread group](#fatal-faults-end-the-whole-thread-group) | Arch fault handlers; `killmt` |
| 15 | [Both architectures, release builds, real round trips](#both-architectures-release-builds-real-round-trips) | Project practice |

## Never touch user memory under an IRQ-off spinlock

**Rule.** No raw read or write of a user pointer while holding `RUN_QUEUE` or any other spinlock — and since syscalls run with IRQs masked, every kernel `spin::Mutex` is an IRQ-off spinlock. Read user arguments before taking the lock; write results after releasing it; or use a mechanism that cannot fault: `AddressSpace::read_user_buf`/`write_user_buf` (HHDM copies of already-present pages), or `sched::uaccess::copy_{from,to}_user` after a prefault ([memory.md](memory.md#how-the-kernel-touches-user-memory)).

**Why.** A user page may not be present. The page fault re-enters the scheduler (`page_fault` → `lock_leader_address_space` → `RUN_QUEUE`), the CPU spins forever on a lock it already holds with interrupts off, every other CPU piles up behind it, and the machine freezes with **no panic and no output**. Found as an x86-64 Tokio bootstrap deadlock in `sigprocmask`/`sigaction` (fixed `82d0cc3`, 2026-07-21); AArch64 masked the same bug by luck — the page happened to be resident. *An architecture asymmetry in symptoms does not imply architecture-specific code.* The signature to recognise: all vCPUs pegged, `IF=0`, serial dead; live QEMU-monitor RIP sampling showing stable PCs and the same spin target is what cracked it.

**Where it recurred.** `futex_wait` read the futex word under its IRQ-off table lock (rewritten in 2026-09 to register → read → recheck). Unresolvable user faults under server locks killed the task (AArch64) or halted the CPU (x86-64) with the lock held: a `recv()` into a `PROT_NONE` page left `UNIX_CONNS` locked forever, and a pipe read into a read-only page leaked `PIPE_RINGS` (lane uptrfault, 2026-09-27). The fix there was twofold — prefault buffers before taking the lock, and make the unix-socket rings and pipe rings use the fault-tolerant copy so a bad pointer becomes `EFAULT` with the lock released. **Known remaining sites**, safe only while their pointers are good: VFS `RamFile`/`DevZero`/`FIONREAD`/`getdents` under `FD_TABLES`, PTYs under `PTYS`, termios under `CONSOLE_TERMIOS`, PipeWire under `STATE`. `epoll_wait` holds `EPOLL_INSTANCES` across readiness probes into the VFS and net servers, which is safe only because those probes touch no faultable user memory.

**How to apply.** When adding a syscall or server path, grep it for raw user-pointer dereferences between a `.lock()` and its guard's drop.

## Publish `Blocked` before checking the condition

**Rule.** Every blocking wait is prepare (publish `Blocked` on a wait channel) → re-check the condition → commit (yield) or cancel ([scheduler.md](scheduler.md#blocking-the-three-phase-protocol)). A waker sets the condition, then wakes.

**Why.** The check-then-block order loses the wake whenever the waker runs between the check and the park: the message sits queued while the receiver sleeps forever. `sys_recv`, `call_port`, poll/epoll, `wait4`, futexes and the kernel's sleeping locks all use the same helpers. The kfacil wait queue found a variant in 2026-10: a waker could unlink a waiter's node between the waiter linking it and publishing `Blocked`; the waiter now re-checks, under the queue lock, that it is still linked after publishing. Futex waits additionally claim their own wake with a per-waiter `woken` flag, and a wake is never "eaten" by clearing a slot that was not `Blocked`.

## Never yield, park or sleep while holding a `spin::Mutex`

**Rule.** No `yield_now`, no blocking primitive, and no bare `irq_window()` (which is a scheduling point) inside a `spin::Mutex` critical section. A long wait that must hold a lock brackets its windows with `preempt_disable()` … `preempt_enable_no_resched()`, which still takes ticks but defers the switch.

**Why.** A yield parks the mutex; every other acquirer spins with IRQs masked and can never be descheduled — a hard hang on a single-vCPU guest. `virtio_gpu::submit` waiting on the host under `VIRTIO_GPU` is the case that produced the `PREEMPT_DISABLE` mechanism; the alternative "yield under the mutex" design was refuted (28 blind lock sites plus the panic path). The sleeping primitives in `sched::ksync` refuse to park in atomic context and spin with a one-line `[KSYNC] sleep in atomic context` report instead — but only for `KSpinLock`/`TrackedMutex` holders; a plain `spin::Mutex` is invisible to that check.

## IRQ context and tick hooks are `try_lock`-only

**Rule.** Interrupt handlers and the BSP tick hooks may only `try_lock` (a bounded `try_lock_spin` counts), must not allocate except from the atomic pool, must never touch `VIRTIO_GPU` or the framebuffer console, and log only through the raw UART.

**Why.** A tick hook can run on a CPU that holds any lock — including inside a `preempt_disable` section that holds `VIRTIO_GPU` — and a non-reentrant spinlock taken twice on one CPU deadlocks it. The watchdog's own report path uses `try_lock` on the exe-path table for the same reason. A latent violation of this class remains: a kernel thread printing through the framebuffer console while another CPU initialises virtio-gpu can make that CPU's own diagnostics drain the console into a flush that needs the lock it holds ([scheduler.md](scheduler.md#limitations-and-open-work)).

## No file I/O under a filesystem or address-space lock

**Rule.** A file-backed page fault performs a filesystem read; it must never be taken while a filesystem lock is held. Syscalls prefault every user buffer they forward to a filesystem server, and the page-fault path reads file pages with the address space's `busy` flag **released** ([memory.md](memory.md#demand-paging-and-the-fault-path)).

**Why.** `pread64`/`pwrite64` without a prefault took a lazy private-file page fault inside f2fs under `F2FS_MOUNTS`; the fault re-entered the filesystem and deadlocked deterministically on both architectures. A filesystem read under `busy` stalled every sibling thread's faults and mm syscalls — one hold lasted 1.5 s during session start. A `read()` into a read-only buffer used to take a kernel permission fault with the filesystem lock held, kill the task in place, and hang the machine; it now answers `EFAULT` up front.

**Extended to device I/O (lanes nostall, loginhang).** The same rule now covers block I/O: the f2fs request path runs with the lock held and the block cache in no-I/O mode, reads a missing block with the lock dropped and re-runs the request ([filesystems.md](../servers/filesystems.md#performance-on-slow-media)). And because `F2FS_MOUNTS` and the USB disk lock are *sleeping* locks, a task that holds or is queued for one must not be reaped in place by `exit_group`'s group kill (that left the lock owned by a dead pid and froze every later login): `ksync` counts sleeping locks per task, the kill defers such a member until it returns to user space, the leader is reaped last, and no `SIGSTOP` is delivered while one is held.

## No `unsafe` globals beyond boot and per-CPU state

**Rule.** Shared mutable state is a lock (`spin::Mutex`, `TrackedMutex`, `KSpinLock`) or an atomic. Bare `static mut` is allowed only where nothing can race on it.

**Reality (counted 2026-10-08).** The older wording — "the only bare `static mut` blocks are in the boot parsers and the arch assembly stubs" — is the intent but not the literal state; there are about 40 `static mut` items, and every one falls into a category where the rule's reasoning still holds:

- **written before any AP starts**: `BOOT_INFO`, `BOOT_CMDLINE`, the boot-parser region arrays, `early_pgtables`, MAIR/SCTLR snapshots, MMIO base addresses;
- **per-CPU arrays indexed only by the owning CPU** with IRQs masked: `SCHEDULER_CTX`, `CURRENT_CTX`, the x86 `TSS`/`GDT`/double-fault stacks, `PER_CPU` syscall data, console staging buffers;
- **buffers with their own synchronisation**: the persistent-log ring (lock-free indices), DMA-visible driver statics, the CSPRNG scratch buffer.

A new `static mut` must fit one of those categories and say which in a comment; anything else is a lock or an atomic.

## Checked arithmetic at memory boundaries

**Rule.** Address and size arithmetic on values that come from userspace or firmware uses `checked_add` (or saturating forms) and rejects overflow: VMA end addresses, `mmap`/`mprotect`/`munmap` ranges, `validate_user_buf`'s `ptr + len`, slab order calculation, DTB and ACPI table offsets, ELF segment bounds.

**Why.** A wrapping `ptr + len` turns an out-of-range buffer into a "valid" one at the bottom of the address space; a wrapping DTB offset reads arbitrary memory during boot. These are silent memory-safety holes, not crashes.

## Time conversions never make a wait early

**Rule.** Convert a timeout to an absolute deadline in nanoseconds on the same clock it will be compared against, and round **up** wherever units coarsen. The tick may make a wake late (by at most one 10 ms period); nothing may make it early. Absolute deadlines (`TIMER_ABSTIME`, `FUTEX_WAIT_BITSET`, `timerfd` `TFD_TIMER_ABSTIME`) are honoured as absolute.

**Why.** `nanosleep` truncated every sub-tick sleep to zero. It passed every correctness test in the tree and was invisible for months, until Mesa's 160 µs ring back-offs became no-ops and its watchdog fired 200× early and `abort()`ed. Rounding up to one tick still fired at the next tick edge (`sleep(200 ms)` measured 196.9 ms); comparing nanosecond deadlines on the monotonic clock fixed it. The same class: `FUTEX_WAIT_BITSET`'s timespec read as a relative interval (every Rust `std` timed wait lasted uptime plus timeout), and a periodic `timerfd` below 10 ms truncated to 0 and became one-shot. Details: [time.md](time.md).

## Kernel stacks are 128 KiB, unguarded, and policed

**Rule.** Every task has a 128 KiB kernel stack (`sched::KERNEL_STACK_ORDER` = 5, defined once). No single frame may exceed **48 KiB**. Large structures are built on the heap in place (`Box::new_uninit()`), never as stack temporaries.

**Why.** The stack is buddy memory reached through the HHDM, with no guard page; frames below it belong to other allocations, so an overflow corrupts them without faulting. And the stack is deep by construction: every server and filesystem runs on it, in the caller's context. `f2fs::mount` built a 155,880-byte `MountState` as a stack temporary on what was then a 64 KiB stack; it corrupted ~90 KB of neighbouring memory on every mount, killed one host and not another (survivability, not susceptibility), and was first bisected to "image size" because the constant that moved `.data` also sized `MountState`. `Box::new(MountState { .. })` still materialises the struct on the stack first, which is why the rule says `new_uninit`.

**Enforced by:** `scripts/check-stack-frames.py`, which parses the `.stack_sizes` section that `-Zemit-stack-sizes` leaves in the linked ELF and **fails the build** above the budget (it named `sys_umount2` at 72,200 B as a second, unnoticed overflow); and a canary (`"STACK_OK"`) in the lowest word of every kernel stack, checked on every syscall return with one mask and one compare — the stack is naturally aligned, so its base is the stack pointer with the low bits cleared. An overflow prints `*** KERNEL STACK OVERFLOW *** pid=… syscall=…` and kills the task so the damage stops there.

**Method note:** when a bug bisects to a constant, check what else that constant sizes; and read the "unaffected" machine's log for the same symptom before believing it unaffected.

## The kernel uses no FP/SIMD

**Rule.** Both kernel targets are soft-float ([targets/](../../targets)); only the context-switch assembly touches vector registers, and it saves/restores the user's full FP/SIMD state on every switch.

**Why.** The EL0 trap frame saves no vector state. An AArch64 kernel built with NEON clobbered user `q0` on 1 in 16 file-backed page faults, which made a `HashMap` lose an entry and zbus reject a valid address ([arch-aarch64.md](arch-aarch64.md#a-soft-float-kernel)). Re-enabling SIMD in the kernel would require saving vector state on every trap, not just every switch.

## Detach a task's page table before it can be freed

**Rule.** After every switch back to the scheduler, the CPU reloads the kernel page table (x86-64: the boot CR3; AArch64: TTBR0 cleared) before the task can be reaped. An exiting thread's address space is released only when no CPU can have it loaded and its `busy` flag is clear; `execve` waits for `busy` before dropping the displaced address space.

**Why.** A CPU that still holds freed page tables in CR3/TTBR0 faults on its next TLB miss — or worse, walks tables that now belong to someone else. The worst instances were group-teardown bugs: `execve` that did not first terminate sibling threads, and fault handlers that killed one thread with `exit(1)`, each freed shared page tables under siblings still running on other CPUs.

## Debug output is gated and off by default

**Rule.** Hot-path tracing lives behind `const …: bool = false` flags (`SYSCALL_TRACE*`, `DBG_ENOMEM`, `DBG_MMAP`, `PORT_STATS`, `HOLD_PROFILE`, `TLBSTAT_PRINT`, `EL0_BACKTRACE`, `SC_STATS`, the `[MMAP]`/`[CPIO]`/unix-socket exchange traces, and the render and audio path traces — 26 such flags in the core crates alone), flipped in a local build when needed and committed `false`. Diagnostics that fire only on a genuine anomaly stay on (`[WDOG]`, `[TIMER] … re-armed`, `[BUDDY] BAD FREE`, the "table FULL" lines, unconditional `ENOSYS` logging), and IRQ-context diagnostics write the raw UART.

**Why.** An unconditional serial write on a hot path does not merely slow the system down; it changes the timing enough to hide the race being investigated, and the console costs ~0.19 s per line. Unlocked kernel lines interleaving with a login prompt once looked like a lost-input bug in 1 boot in 10; kernel lines now print whole through a per-CPU outbox.

## An errno names its cause, and a full table says so

**Rule.** A failure path that returns a generic errno for a specific limit must be traceable to that limit: every `ENOMEM` in the memory, address-space and exec paths goes through `enomem_site("…")`, and every fixed-size kernel table reports itself full in one ungated line, once.

**Why.** `ENOMEM` meant about a dozen unrelated things, two of them about RAM. Two investigations hunted a memory leak with over a gigabyte free; the cause was a 64-entry IPC port table, and the visible symptom was `stat()` of an existing directory failing with "Out of memory" ([ipc.md](ipc.md#live_buckets-the-ceiling-that-looked-like-ram)). Today: `[IPC] port table FULL … this is ipc::port::LIVE_BUCKETS, not RAM`, `[VFS] ENOMEM: no reply port for this task`, `[SCHED] task table FULL … this is runqueue::MAX_TASKS, not RAM`, and the mapping failure reason from `mm::vmm::last_map_fail()` ([memory.md](memory.md#enomem-must-name-its-cause)).

## Fatal faults end the whole thread group

**Rule.** A fatal user fault or a fatal signal terminates the thread group through `exit_group_signal`, never a single-thread `exit`, and the wait status reports the signal (`WIFSIGNALED`), not `128 + signo` as an exit code. Exactly one owner runs group teardown, regardless of which CPU the signal lands on.

**Why.** A single-thread exit of a faulting thread reaped the leader and freed page tables under running sibling Tokio threads, producing an external-abort storm that four investigations mis-attributed to an LLVM JIT that was not even in the build. Details: [processes.md](processes.md#fatal-signals-and-faults-end-the-whole-thread-group).

## Both architectures, release builds, real round trips

**Rule.** Every kernel change is built in release mode and booted on **both** x86-64 and AArch64, and its regression suites run on both. Changes to hand-written entry/exception assembly are verified with an interactive round trip that depends on a return value, not just a successful boot; a suspicious result is bisected with `git stash` and a single-architecture rebuild rather than reasoned about from the assembly. The Limine base revision is never lowered below 6 ([boot.md](boot.md#the-limine-protocol-and-the-revision-6-invariant)).

**Why.** Debug builds diverge early (symbols and timing). Architecture asymmetries hide bugs in shared code: the `sigprocmask` deadlock, the `PAR_EL1` corruption (visible only under HVF), `CR0.WP` on x86 APs, and the FP/SIMD clobber each appeared on one architecture only. And the accelerator is not the architecture: TCG implements exclusives on non-cacheable memory and round-trips `PAR_EL1`, so a TCG pass proves nothing about HVF or real hardware. Project practice for this is in [development/conventions.md](../development/conventions.md) and [development/methodology.md](../development/methodology.md).

## See also

- [overview/design-principles.md](../overview/design-principles.md) — the project-wide principles
- [memory.md](memory.md), [scheduler.md](scheduler.md), [ipc.md](ipc.md) — where each rule is implemented
- [development/debugging.md](../development/debugging.md) — recognising a violation from `[WDOG]` lines and Ctrl-T
- [development/testing.md](../development/testing.md) — the suites named above
