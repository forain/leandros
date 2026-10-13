# Threads and futexes

The kernel provides exactly two thread primitives — `clone` with `CLONE_VM` (a real OS thread sharing the caller's address space) and `futex` — and everything POSIX-shaped (mutexes, condition variables, thread-specific data, cleanup handlers) is built on them in userspace, the same split glibc and musl use on Linux. This page covers the kernel half: thread creation, the futex implementation and its SMP lost-wake protocol, futex keying, timeouts and signal interaction, and the `CLONE_PARENT_SETTID` bug that wedged the Zink desktop.

## Overview

Full pthread support — `pthread_create`/`join`, mutexes, condition variables, thread-local storage and cleanup handlers — works on both architectures, under relibc (the in-tree test programs) and under musl (every ported program: Mesa, D-Bus, COSMIC, brush).

| Layer | What it provides |
|---|---|
| Kernel | `clone(CLONE_VM \| CLONE_THREAD \| CLONE_SETTLS \| CLONE_PARENT_SETTID \| CLONE_CHILD_CLEARTID …)`, `futex`, `set_tid_address`, `exit` (thread) / `exit_group`, `gettid`, `tgkill`/`tkill` |
| relibc ([../userland/libc.md](../userland/libc.md)) | `RlctMutex`/`RlctCondvar` on `futex`, a `#[thread_local]` TSD map with destructor-on-exit, `pthread_cleanup_push`/`pop`, the raw thread-spawn path `rlct_clone` on both arches |
| musl | its own pthreads on the same two syscalls, unmodified |

The kernel has no mutex, condvar or TSD syscalls of its own. A contended mutex, condvar wait/signal and TSD destructor ordering are exercised in QEMU on both targets.

### What a thread shares and what it owns

| Shared by the thread group (lives on the leader or is keyed by TGID) | Per thread |
|---|---|
| Address space (`Arc<AddressSpace>`, one reference per thread) | Kernel stack, saved registers, FP/SIMD state |
| fd tables, sockets, epoll instances | TLS base (`tpidr_el0` / `fs_base`), user GS base |
| Signal dispositions, process-directed pending set | Signal mask, thread-directed pending set, alternate signal stack |
| pid (the TGID), ppid, pgid, sid, exit status | tid, futex wait slot, restart block, `clear_child_tid`, rseq registration |
| POSIX timers, exe path | nice value (as on Linux) |

cwd, root and umask are currently per-thread copies taken at `clone` time, a known deviation (see [processes.md](processes.md#limitations-and-open-work)).

## How it works

### Creating a thread

`sys_clone_or_fork` (in [kernel/src/syscall.rs](../../kernel/src/syscall.rs)) routes any `CLONE_VM` clone to `clone_thread` in [sched/src/clone.rs](../../sched/src/clone.rs). The child gets its own kernel stack with a copy of the parent's `UserFrame` (return value 0, SP replaced by `child_stack` if given), an aliased `Arc<AddressSpace>`, and:

- **TLS**: the caller's *current* TLS base, overridden by `CLONE_SETTLS` when given. Linux `clone()` always inherits the live TLS base; `CLONE_SETTLS` only replaces it. Forcing 0 when the flag was absent broke vfork-style spawns: musl's `posix_spawn` fast path calls `clone(CLONE_VM|CLONE_VFORK|SIGCHLD)` with no TLS argument and expects the child to keep using the parent's TLS block (for `errno`) until it `execve`s, so the child null-dereferenced on its first thread-local access. The base is read from the live register (`current_tls_base`), not `Task::tls_base`, because that shadow field is never populated on AArch64.
- **tgid**: the parent's with `CLONE_THREAD`; its own pid without (a vfork-style new process).
- **Signal state**: the creator's signal mask (POSIX `pthread_create`); dispositions stay on the leader, so a `CLONE_THREAD` sibling's own table is never consulted.
- **tid words**: `CLONE_PARENT_SETTID` and `CLONE_CHILD_SETTID` are written before the child is enqueued; `CLONE_CHILD_CLEARTID` is recorded as `clear_child_tid`.

On thread exit the kernel writes 0 to `clear_child_tid` and issues a `FUTEX_WAKE(1)` on it — the `pthread_join` handshake.

### The futex table and the three-phase wait

[sched/src/futex.rs](../../sched/src/futex.rs) keeps one global `FUTEX_TABLE` of `FutexWaiter { pid, uaddr, tgid, private, woken }`, sized to `MAX_TASKS`. A task waits on at most one futex, so the table can never be full; it used to be 256 entries and answered `EAGAIN` when full, which userspace reads as "value changed" and retries — a silent spin. Waiters are chained per address in 256 hash buckets, in arrival order (doubly linked, so freeing and re-keying a slot are O(1)), so `FUTEX_WAKE` and requeue walk one bucket rather than the whole table, which held 4096 slots at 32 B each, and `FUTEX_WAKE(n=1)` releases the longest waiter first (lane procmax, 2026-10-12).

The classic lost-wake race is: CPU A reads `*uaddr == val` and decides to sleep; CPU B changes the value and calls `FUTEX_WAKE` before A is findable; A sleeps until its timeout or forever. Linux closes it by comparing the user word *under* the hash-bucket lock. This kernel cannot: a read of user memory can take a demand-paging fault, and the fault handler re-enters the scheduler — the standing rule ([invariants.md](invariants.md)) is that user memory is never touched under a kernel spinlock. So the wait registers first and looks second:

1. **Prepare** — take `FUTEX_TABLE`, claim a slot for this task, drop the lock. From this instant every `FUTEX_WAKE` on `uaddr` can see the waiter.
2. **Re-check** — read `*uaddr` with no kernel lock held (a fault is serviceable) and compare with the expected value.
3. **Commit** — take `FUTEX_TABLE` again. If the slot was claimed in the window (`woken`), a wake was aimed at this waiter: deregister and return 0. If the value moved, deregister and return `EAGAIN`. Otherwise mark the task `Blocked` under `RUN_QUEUE`, still inside the `FUTEX_TABLE` hold, and yield.

`FUTEX_WAKE` therefore *claims* slots (`woken = true`) instead of freeing them; the waiter frees its own slot on every exit path, and `remove_waiter` covers a thread force-killed while parked. A claimed slot is invisible to further wakes, so one `FUTEX_WAKE(n=1)` never consumes two waiters. The `woken` flag also decides the return value: 0 if and only if a wake claimed the waiter; `ETIMEDOUT` only when none did *and* the deadline has passed. Lock order is always `FUTEX_TABLE → RUN_QUEUE`.

### Keying

A `FUTEX_PRIVATE_FLAG` futex is keyed by `(tgid, uaddr)` (`key_matches`): the threads of a group share one address space, and the same address in another process is a different futex. Shared (non-private) futexes match by address alone; a physical-address key is deferred.

### Operations

| Op | Value | Behaviour |
|---|---|---|
| `FUTEX_WAIT` | 0 | Relative timeout (nanoseconds on the monotonic clock) |
| `FUTEX_WAKE` | 1 | Wake up to `val` waiters on the key |
| `FUTEX_REQUEUE`, `FUTEX_CMP_REQUEUE` | 3, 4 | Wake `val`, move up to the limit to `uaddr2`; CMP compares `val3` first (`EAGAIN`) |
| `FUTEX_LOCK_PI`, `FUTEX_UNLOCK_PI`, `FUTEX_TRYLOCK_PI`, `FUTEX_LOCK_PI2` | 6, 7, 8, 13 | Linux lock-word protocol without priority inheritance (below) |
| `FUTEX_WAIT_BITSET` | 9 | **Absolute** deadline on `CLOCK_MONOTONIC`, or `CLOCK_REALTIME` with `FUTEX_CLOCK_REALTIME`; the bitset itself is not filtered (every libc here passes `FUTEX_BITSET_MATCH_ANY`) |
| anything else (`WAKE_OP`, `WAKE_BITSET`, requeue-PI, …) | | `ENOSYS` |

### Timeouts

`FUTEX_WAIT`'s timeout is a relative interval, converted exactly as `nanosleep` converts it (a 3 ms wait is 3 ms, not "0 ticks, wake at the next edge"). `FUTEX_WAIT_BITSET`'s is an absolute instant; a `FUTEX_CLOCK_REALTIME` deadline is translated onto the monotonic clock through the wall-clock offset once, at entry. A `BITSET` deadline already in the past is answered without parking, value check first (`EAGAIN` if the word moved, else `ETIMEDOUT`, as Linux).

Timed and untimed waiters take the same register-and-block path, so a cross-thread `FUTEX_WAKE` reaches a timed waiter exactly as it reaches an untimed one (it used to be left to time out). A timed waiter additionally records its deadline in `Task::poll_deadline`, so the poll-deadline service ([time.md](time.md#the-tick-and-one-shot-deadlines)) releases it at the timeout even if no wake ever comes.

### Signals

`futex_wait_intr` decides "woken / timed out / interrupted" inside the same `FUTEX_TABLE` hold that frees the slot, so a wake that claimed the waiter always wins over a signal that arrived at the same time. Then, following Linux `futex_wait`:

| Case | Result |
|---|---|
| Untimed wait, deliverable signal | `-ERESTARTSYS`: restarted transparently under `SA_RESTART` or when no handler runs, else `EINTR` |
| Timed wait, a handler runs | `EINTR`, with or without `SA_RESTART` (restarting would let a periodic signal postpone the timeout forever) |
| Timed wait, no handler runs (stop/continue, a sibling took the signal) | Restart through the restart block on the **original absolute deadline** (`futex_wait_timed`, Linux `futex_wait_restart`) |
| Ignored signal (`SIG_IGN`, or `SIG_DFL` for a default-ignore signal like `SIGCHLD`) | Re-register, re-check, re-park — Linux's `goto retry`; it used to return a spurious 0 |

`FUTEX_WAIT_BITSET` answers the same way (Linux does not tell the two apart). It used to return a spurious 0 at a signal — harmless for Rust std and relibc, which re-check, but an `SA_RESTART`-less caller could never see the `EINTR`.

### PI futexes

musl uses `FUTEX_LOCK_PI` for every `PTHREAD_PRIO_INHERIT` mutex, and `pthread_mutexattr_setprotocol(PTHREAD_PRIO_INHERIT)` *probes* the call on a zero word: any failure makes the attribute call fail. libpulse's `pa_mutex_new` asserts on that failure and aborts, which is how Firefox's audio thread died the moment cubeb opened its PulseAudio backend (`161af5c`). The kernel implements Linux's lock word (owner tid in bits 0..29, `FUTEX_OWNER_DIED` bit 30, `FUTEX_WAITERS` bit 31): `LOCK_PI` sets `FUTEX_WAITERS` *before* any timeout verdict and parks expecting exactly that word, so an unlock between the two becomes `EAGAIN` and a retry, never a lost wake; `UNLOCK_PI` releases to 0 and wakes one waiter. `EDEADLK` if the caller already owns it, `ESRCH` if the owner tid names no task. What is **not** implemented is the priority inheritance itself (there is no RT class to boost into), fair ownership handoff, or robust lists (`FUTEX_OWNER_DIED` is preserved, never set).

### Restartable sequences (rseq)

`rseq(area, len, flags, sig)` registers a per-thread `struct rseq` (`sched/src/rseq.rs`, after Linux `kernel/rseq.c`): `EINVAL` for a length under 32 or an area not 32-byte aligned, `EBUSY` for a second registration of the same area, `EINVAL` for a different one, `EPERM` for a signature mismatch; `RSEQ_FLAG_UNREGISTER` needs the same area, length and signature and resets `cpu_id` to `RSEQ_CPU_ID_UNINITIALIZED`. `AT_RSEQ_FEATURE_SIZE` (28) and `AT_RSEQ_ALIGN` (32) are in every auxv.

The kernel keeps the two ABI promises from `check_and_deliver_signals`, which every return to user space runs:

- **CPU fields.** `cpu_id_start`/`cpu_id` name the CPU the thread runs on; `node_id` is 0 and `mm_cid` the CPU number (unique among running threads, below the CPU count).
- **Abort.** If `rseq_cs` is set and the interrupted PC lies in `[start_ip, start_ip + post_commit_offset)`, the thread resumes at `abort_ip`, after the 4-byte word before it has been checked against the registered signature. Any malformed state — unreadable area or descriptor, out-of-range fields, `abort_ip` inside the section, a signature mismatch — is `SIGSEGV`. `rseq_cs` is cleared either way (the lazy clear the ABI relies on).

**Cost.** The scheduler sets a per-CPU flag when it dispatches a thread that has an area (a dispatch is exactly "it was off this CPU: preempted, blocked or moved"), and the return path does one relaxed load of it; threads without a registration — every musl thread — pay that and nothing else. Signal delivery runs the same fixup just before the handler frame is built, so the frame saves `abort_ip` and the handler returns there. The delivery loop can stop a thread and resume it on another CPU, so the flag is checked once more after it.

**Lifetime.** `fork` inherits the calling thread's registration (the copied memory has the area at the same address); a `CLONE_VM` child — thread or vfork — starts without one; `execve` clears it.

## Design decisions

### CLONE_PARENT_SETTID and the musl thread-list lock

**Choice.** `CLONE_PARENT_SETTID` is honoured: the child's tid is stored in the shared address space before the child is runnable (`d8d0fe8`).

**Why.** musl's `pthread_create` never assigns `clone()`'s return value to `new->tid`; it relies on the kernel's `CLONE_PARENT_SETTID` store. `__tl_lock` keys the thread-list lock on `__pthread_self()->tid`. With the flag ignored every non-main thread had tid 0, and such a thread treats a *free* lock (value 0) as one it already holds recursively — it bumps the process-wide `tl_lock_count` without acquiring. When that thread exits (`pthread_exit` takes the lock and leaves it for the kernel's `CLEARTID` to release) the count stays high, the main thread's next lock/unlock pair decrements the count instead of storing 0, and the lock is held by the main thread forever. The first `fork()` from another thread then parks in `__tl_lock` holding `__malloc_lock`, and the process wedges.

That is exactly cosmic-comp's first keybinding spawn — so it was simultaneously the root cause of system keybindings never firing and one of three kernel-side fixes the Zink desktop needed before it produced real frames (see [../graphics/zink-and-virgl.md](../graphics/zink-and-virgl.md)). None of the three was in the GPU driver.

**Guard.** `spawnwedge` reproduces the failing program's shape: a main thread allocating, workers hammering malloc/free, and short-lived spawner threads that `fork`+`exec` (through `Command::spawn` with a `pre_exec` hook, which forces std onto plain `fork()` and musl's atfork sequence) and then exit. `thread_tid_visible` checks the `CLONE_PARENT_SETTID` store directly; a wedge is reported with raw syscalls only, so the report cannot itself deadlock.

### Register first, look second (no user reads under the table lock)

**Choice.** The three-phase protocol above, with a per-waiter `woken` claim flag.

**Why.** The previous implementation read the user word under the IRQ-off table lock (a demand-paging fault there re-enters the scheduler under its own lock and freezes every vCPU with no panic and no output), and had three further lost-wake holes found during the 2026-09-15 SMP hunt (`4420b3d`): `futex_wake` cleared a not-yet-Blocked waiter's slot unconditionally, so the wake was eaten and the waiter later returned a spurious `ETIMEDOUT`; `ETIMEDOUT` was decided by the clock rather than by whether a wake arrived; and `blocked_on` was never cleared when parking on a futex, so *any* `wake_poll` yanked futex waiters. `smpwaketest` (futex and eventfd ping-pong ×2/×4, under spin and yield load, WFI wake, pipe-`EPOLLET` under a poller herd) reports `lost=0` on both arches since.

Two harness traps cost a cycle each and are worth knowing before writing a futex test: a pure `FUTEX_WAKE` against a constant word is dropped *by design* (use value-change: `fetch_add` then wake, with the waiter passing the value it snapshotted), and the snapshot must be taken *before* releasing the peer, or paired 2 s stalls look like lost wakes.

### Private futexes are keyed by thread group

**Choice.** `(tgid, uaddr)` for private futexes (`a19267d`).

**Why.** Keyed by virtual address alone, a `FUTEX_WAKE` in one process could claim a waiter parked at the same address in a different process — every process maps its libc and heap at the same addresses here (`MAIN_DYN_BASE`, `INTERP_BASE`), so collisions are not rare — and the wake meant for the local waiter was consumed elsewhere. COSMIC panel applets hung on it.

### FUTEX_WAIT_BITSET is an absolute deadline

**Choice.** Read the `BITSET` timespec as the absolute `CLOCK_MONOTONIC` instant it is (`6a313a1`).

**Why.** Rust std's `futex_wait` — under every `Condvar::wait_timeout`, `thread::park_timeout`, `mpsc::recv_timeout` and `Mutex` spin fallback in every Rust program — uses the bitset form with `now + timeout`. Treated as relative, every std timed wait lasted *uptime plus the intended duration*: harmless in the first seconds after boot, a wedge a few minutes in. async-io's driver thread backs off with `park_timeout(≤10 ms)` while any thread is inside `async_io::block_on` (zbus's executor is, permanently), so three minutes after boot each back-off became a three-minute sleep — the session compositor started by greetd after login sat in `zbus::Connection::session()` with its `Hello` reply unread in the socket for minutes, while the same compositor started a minute after boot came up fine. Same bug class as `clock_nanosleep` ignoring `TIMER_ABSTIME` and `timerfd_settime` ignoring `TFD_TIMER_ABSTIME`: an absolute time read as a relative one.

### Timed waits return EINTR on a handled signal, as Linux does

**Choice.** Linux's exact restart semantics, measured on a Linux host rather than assumed.

**Why.** A sigtest stress case once showed 10–20% `EINTR` from an `SA_RESTART` `FUTEX_WAIT` under host load and was filed as a race; two fix attempts at the per-CPU restart bookkeeping changed nothing. The premise was wrong twice over. First, Linux never restarts a *timed* `FUTEX_WAIT` once a handler runs, `SA_RESTART` or not — measured on a Linux 7.x host with a 350 ms wait signalled at 50 ms, 20 runs per row:

| Signal at 50 ms | EINTR | ETIMEDOUT | mean elapsed |
|---|---|---|---|
| handler + `SA_RESTART` | 20 | 0 | 50 ms |
| handler, no `SA_RESTART` | 20 | 0 | 50 ms |
| `SIGCHLD` at `SIG_DFL` (ignored) | 0 | 20 | 350 ms |

Second, the `EINTR` the stress case saw came from a `SIGCHLD` handler *without* `SA_RESTART` left installed by an earlier test case; under load the forked child's exit landed on the correctly restarted wait. With `SIGCHLD` at `SIG_DFL` both arches give 0/100 `EINTR`. The investigation did find two real bugs next to the false one — a claimed wake could be turned into a restart and lost, and an ignored signal returned a spurious 0 — both fixed (`8002838`, `2f7d101`). The stress cases are now strict: a handled signal must give `EINTR` at the signal with the handler run once, an ignored one `ETIMEDOUT` at ≥ 350 ms.

## Limitations and open work

- **Shared futexes are keyed by virtual address**, not physical page: two processes sharing a `MAP_SHARED` futex word at *different* addresses will not wake each other, and non-private waiters at the same address in unrelated processes can be woken spuriously (harmless — waiters re-check).
- No robust-list support (`set_robust_list` is not implemented): a thread that dies holding a robust mutex is not recovered.
- PI futexes have no priority inheritance and no fair handoff.
- `FUTEX_WAKE_OP`, `FUTEX_WAKE_BITSET` and the requeue-PI operations return `ENOSYS`; no libc in the tree issues them today.
- A `CLOCK_REALTIME` `BITSET` deadline is translated to monotonic once, so a clock step while the waiter is stopped is not re-applied on the restart.
- rseq: `RSEQ_CS_FLAG_*` in a critical-section descriptor are ignored rather than rejected, and `mm_cid` is the CPU number rather than a compact per-mm id.

## Code and tests

| What | Where |
|---|---|
| Futex table, wait/wake/requeue, keying | [sched/src/futex.rs](../../sched/src/futex.rs) |
| `sys_futex`, `futex_wait_timed`, PI futexes, `sys_clone_or_fork`, `sys_set_tid_address` | [kernel/src/syscall.rs](../../kernel/src/syscall.rs) |
| `clone_thread` (TLS, tid words, vfork) | [sched/src/clone.rs](../../sched/src/clone.rs) |
| rseq registration, cpu_id updates, critical-section abort | [sched/src/rseq.rs](../../sched/src/rseq.rs) |
| Restart block, `CLEARTID` wake on exit | [sched/src/lib.rs](../../sched/src/lib.rs) |

Regression suites: `pthreadtest` (create/join, contended mutex, condvar, TSD destructors, cleanup handlers); `smpwaketest` (lost-wake probes under SMP load); `spawnwedge` (`CLONE_PARENT_SETTID` and fork/`posix_spawn` from threads); `sigtest` futex cases (`futex_wait_restart_stress`, `futex_timed_signal_stress`, `futex_wait_bitset_signal`, wake-race; `sigtest futex N` and `sigtest futexab N` run them in loops); `sigtest stoprem` (timed waits resume the original deadline after stop/continue); `wakepolltest` (`FUTEX_WAIT_BITSET` absolute and past-deadline); `timertest futex_wait_relative_3ms`; `syscalltest` rseq cases (registration rules, `cpu_id`, abort on signal and on preemption, `SIGSEGV` on a bad signature, fork/thread/exec lifetime).

## See also

- [processes.md](processes.md) — fork/clone dispatch, exit_group, de-thread
- [signals.md](signals.md#syscall-restart) — the restart machinery the futex paths use
- [time.md](time.md) — the clocks and deadline service behind futex timeouts
- [invariants.md](invariants.md) — never touch user memory under a scheduler lock
- [../userland/libc.md](../userland/libc.md) — relibc and musl pthreads
