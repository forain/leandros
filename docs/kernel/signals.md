# Signals

LeandrOS implements POSIX signal delivery on both architectures with Linux's ABI: real `rt_sigframe`s with FP/SIMD state, `sigaltstack`, fault `siginfo`, `SA_RESTART` and the restart-block family, process- versus thread-directed delivery, and a real job-control `Stopped` state. Musl and relibc install handlers and return through `rt_sigreturn` without knowing they are not on Linux.

## Overview

| Piece | Where |
|---|---|
| Delivery engine, frames, `sigaction`/`sigprocmask`/`sigaltstack`, stops, restart record | [sched/src/signal.rs](../../sched/src/signal.rs) |
| `deliver_signal`, `deliver_signal_process`, `kill_pgrp`, `post_signal_*` | [sched/src/lib.rs](../../sched/src/lib.rs) |
| `SigInfo`, `SI_*`/`CLD_*`/`SEGV_*` codes, `SYNCHRONOUS_MASK` | [sched/src/task.rs](../../sched/src/task.rs) |
| `kill(2)` permission rules | [sched/src/cred.rs](../../sched/src/cred.rs) |
| Syscalls (`kill`, `tgkill`, `rt_sigaction`, `rt_sigreturn`, `rt_sigsuspend`, `rt_sigtimedwait`, `pause`, `restart_syscall`) | [kernel/src/syscall.rs](../../kernel/src/syscall.rs) |
| Return-to-user hooks | AArch64 [arch/aarch64/src/exception_asm.s](../../arch/aarch64/src/exception_asm.s); x86-64 [arch/x86_64/src/syscall.rs](../../arch/x86_64/src/syscall.rs) and [arch/x86_64/src/idt.rs](../../arch/x86_64/src/idt.rs) |

Signals 1–64 are tracked as bitmasks: a per-thread `signal_pending` and `signal_mask`, a process-level `shared_signal_pending` on the leader, and dispositions (`signal_actions[64]`) on the leader. Each pending signal carries exactly one `SigInfo` payload.

## How it works

### Generation and targeting

- **Process-directed** (`kill(pid)`, `kill(-pgid)`, `killpg`, `kill(-1)`, timers, `SIGCHLD`): `deliver_signal_process` picks a thread of the group that has the signal unmasked, *preferring a blocked one* so the signal also wakes it. If every thread masks it, the bit parks on the leader's `shared_signal_pending`, and whichever thread later unmasks it claims it on its next return to user space. A thread id passed to `kill` names its process, as on Linux.
- **Thread-directed** (`tgkill`, `tkill`, `raise()`, `pthread_kill`, `SIGEV_THREAD_ID` timers, synchronous faults): `deliver_signal` to that thread only.
- **Permission**: `kill(2)` follows Linux `kill_ok_by_cred` (the sender's real or effective uid matches the target's real or saved uid, or the sender's euid is 0 — there are no capability sets), plus the exemptions for the caller's own thread group and `SIGCONT` within the caller's session. The check and the target lookup run under one `RUN_QUEUE` hold. The kernel's own sources — tty job control, `SIGCHLD`, POSIX timers, VT release/acquire — call `deliver_signal*` directly and are not subject to it, exactly as in-kernel `send_sig` is not on Linux.
- **Undeniable signals**: `SIGKILL` and `SIGSTOP` (`UNBLOCKABLE`) can never be blocked, caught or ignored. `sigprocmask` (and ppoll/pselect6 masks) silently strip them, `sigaction` rejects them with `EINVAL`, and process delivery does not gate them on any mask.
- Every delivery also issues a broadcast poll wake, so a thread parked in `poll`/`epoll_wait`/`wait4`/`nanosleep` re-checks for a deliverable signal (see [event-loop.md](event-loop.md)).

### Delivery path

`check_and_deliver_signals(frame_ptr)` runs on every return to user space from a syscall, IRQ or fault:

- **AArch64**: from `exception_asm.s` on the EL0 paths only. The EL1 IRQ path deliberately skips it, since that path returns to interrupted kernel code.
- **x86-64**: on the `syscall_entry` return path, and on every user-committable fault vector, each of which now goes through a full `UserFrame` path.

For each pending, unmasked signal:

1. **Synchronous signals first.** `SIGILL`, `SIGTRAP`, `SIGBUS`, `SIGFPE`, `SIGSEGV` (`SYNCHRONOUS_MASK`) are dequeued ahead of anything else, as Linux's `dequeue_synchronous_signal` does — otherwise a handler for a queued `SIGCHLD` would return to the faulting instruction, re-fault, and only then see `SIGSEGV`.
2. **Disposition**:
   - `SIG_IGN`, or `SIG_DFL` for a default-ignore signal (`SIGCHLD`, `SIGCONT`, `SIGURG`, `SIGWINCH`): discard.
   - `SIG_DFL` stop (`SIGSTOP`, `SIGTSTP`, `SIGTTIN`, `SIGTTOU`): stop the group (below).
   - `SIG_DFL` terminate: `exit_group_signal(sig)` — the whole thread group, with `WIFSIGNALED` status (see [processes.md](processes.md#fatal-signals-and-faults-end-the-whole-thread-group)).
   - A handler: build a frame, apply `sa_mask` plus the signal itself unless `SA_NODEFER`, honour `SA_RESETHAND`, and redirect the return to the handler.
3. **Syscall restart** bookkeeping (below) decides whether the interrupted syscall is re-executed.

The blocked-set and `SA_RESETHAND` updates happen *inside the handler arm*, next to frame construction, as in Linux's `handle_signal()`. They used to happen before the disposition was examined: an ignored signal widened the mask, no frame was built, no `rt_sigreturn` ever restored it — so ignoring `SIGUSR2` once blocked it forever, and because `SIGCHLD` is default-ignore, every program's first child exit permanently blocked `SIGCHLD` before it had installed a handler (`3f4d609`).

**What counts as "interrupted".** Blocking syscalls use `has_deliverable_signal()` as their `EINTR` condition: a pending, unmasked signal that would actually run a handler or terminate. A bare `pending & !mask` test would make every child exit spuriously interrupt a parent that never installed a `SIGCHLD` handler. Callers that *park* on the answer use `has_deliverable_signal_locked` inside the same `RUN_QUEUE` hold that marks them Blocked, because `deliver_signal` only performs a Blocked → Ready wake and a signal landing between a separate check and the park would be stranded.

### Signal frames

Both architectures build a real Linux `rt_sigframe` on the user stack (or the alternate stack):

- **x86-64**: `pretcode` + SysV `ucontext`/`mcontext` matching relibc's `src/header/signal/linux.rs` and its Linux-ABI `__restore_rt` trampoline; `SA_RESTORER` supplies the return address. A full 128-byte `siginfo_t`, then a 512-byte FXSAVE area at offset 968 that `uc_mcontext.fpregs` points at (frame size 1480 bytes). The FXSAVE area is carved out of a larger buffer at a runtime-aligned offset, because `fxsave64`/`fxrstor64` fault on an unaligned address and the kernel stack's alignment cannot be relied on. MXCSR is masked with the CPU's `MXCSR_MASK` before `fxrstor64`.
- **AArch64**: the `ucontext` is followed by an `fpsimd_context` record (`FPSIMD_MAGIC` 0x46508001, 528 bytes: q0–q31, fpsr, fpcr). The handler's return address is the kernel-provided sigreturn trampoline mapped at `SIGRET_TRAMPOLINE_VA` (`0x7fff_ff00_0000`) in every exec'd address space. Linux aarch64 has no `SA_RESTORER` convention — musl installs handlers without a restorer and relies on the kernel's vDSO trampoline. relibc always passes one, which is why the gap stayed hidden until the first musl binary (brush) took a signal and returned through `LR = 0`.

`rt_sigreturn` restores the general registers, the FP/SIMD record and `uc_sigmask`. Saving FP/SIMD state in the frame is not optional: compiled handler code uses the vector registers freely, and before `145bbf7` any handler that did so handed its values back to the interrupted code. `sigtest handler_preserves_vector_regs` fills every vector register, sends itself a signal from inside one asm block, lets the handler overwrite them, and checks them after `sigreturn`.

### Hardening rt_sigreturn

The saved processor state in a frame is user-writable memory, so `rt_sigreturn` never restores it verbatim: AArch64 keeps only the condition flags (`spsr_el1 & 0xF000_0000`, NZCV), and x86-64 keeps only `SAFE_RFLAGS_MASK` bits plus the fixed ones. A forged frame therefore cannot request a privilege escalation — for example an EL0 → EL1 mode-bit forgery on AArch64 — through `sigreturn`.

### sigaltstack

Real per-thread alternate-stack state (`Task::altstack_sp/size/flags`). `SA_ONSTACK` redirects delivery onto the configured stack. `SS_ONSTACK`, and `EPERM` for changing the stack while on it, are derived from the live user stack pointer at syscall time rather than tracked separately, matching Linux's `get_sigframe()`/`do_sigaltstack()`. `MINSIGSTKSZ` is 2048.

### Fault siginfo

Every user-committable fault vector delivers to a registered handler with a real `_sigfault` payload instead of only killing the task (`dbc96e5`): `si_addr` and `SEGV_MAPERR`/`SEGV_ACCERR`, `BUS_ADRALN`/`BUS_ADRERR`, `ILL_ILLOPC`, `FPE_INTDIV`/`FPE_FLTINV`, `TRAP_BRKPT`/`TRAP_TRACE`. `fault_signal` mirrors Linux's `force_sig_fault`: if the signal is `SIG_DFL`, `SIG_IGN`, or blocked, the process is killed rather than the signal deferred, because the faulting instruction would only fault again. The blocked check doubles as the re-entrancy guard — a handler runs with its own signal masked (unless `SA_NODEFER`), so a handler that faults again is killed instead of recursing. A fault's payload overwrites any earlier pending instance of the same signal, since a stale `kill(2)` payload would misdirect the handler inspecting `si_addr`. `siglongjmp` out of a fault handler works.

### siginfo for everything else

Every producer must say where a signal came from — `deliver_signal` takes a `SigInfo` with no default, because a zeroed payload is indistinguishable from a genuine `SI_USER` `kill()` and misreports every other origin (`9b27469`). `SI_USER` carries the sender's tgid and uid; `SI_TKILL` for `tkill`/`tgkill`; `SI_QUEUE` for `rt_sigqueueinfo`; `SI_TIMER` carries `si_timerid`, `si_overrun` and `si_value` (`sigev_value`) for POSIX timers; `SIGCHLD` carries `CLD_EXITED`/`CLD_KILLED`/`CLD_STOPPED`/`CLD_CONTINUED` with the child's pid, uid and status, derived from the same `ExitStatus` the wait status is (never `CLD_DUMPED`: there are no core dumps).

### Inheritance across fork, threads and exec

- `fork()` and `pthread_create()` both inherit the caller's signal mask.
- A new *process* also inherits the leader's dispositions (`SIG_IGN` survives `fork`, per POSIX); a new thread shares them.
- `execve` resets caught handlers to `SIG_DFL` (`reset_handlers_on_exec`), keeping `SIG_IGN`, `SIG_DFL`, the mask and pending signals. Omitting this let a stale handler survive exec: `/bin/sh` is a hardlink to brush, a fixed-base ET_EXEC, so `exec /bin/sh …` reloaded the same code at the same address, the new brush read its own stale handler as the "previous" one, and `signal_hook_registry` chained to itself — an unbounded, stack-exhausting recursion that blocked the COSMIC session launcher (`afbb3d4`).

### Syscall restart

Internally a syscall can return Linux's restart codes; userspace never sees them:

| Code | Meaning | Used by |
|---|---|---|
| `-ERESTARTSYS` (-512) | Restart if the handler has `SA_RESTART` or no handler runs, else `EINTR` | blocking pipe/socket `read`/`write`/`send`/`recv`/`readv` (`block_until_ready`), console read, `wait4`, `waitid`, untimed `FUTEX_WAIT` |
| `-ERESTARTNOHAND` (-514) | Restart only if no handler runs | absolute `clock_nanosleep` |
| `-ERESTART_RESTARTBLOCK` (-516) | `EINTR` if a handler runs; otherwise resume through `restart_syscall` with the saved state | relative `nanosleep`, timed `FUTEX_WAIT`/`FUTEX_WAIT_BITSET`, `poll`/`ppoll`, `select`/`pselect6` |

The dispatcher records `(nr, a0)` per CPU (`note_syscall_restart`, with a "no-handler-only" bit) and reports `EINTR` in the frame; the `check_and_deliver_signals` that immediately follows on the same CPU (IRQs masked in between) consumes it and, if the rules say restart, rewinds the frame onto the syscall instruction: x86-64 `rax = nr, rip -= 2`; AArch64 `x8 = nr, x0 = a0, elr -= 4`. For `-ERESTART_RESTARTBLOCK` the rewind targets `restart_syscall` (x86-64 219, AArch64 128), which resumes from the per-thread `Task::restart_block: [u64; 8]` — an absolute deadline plus, for ppoll/pselect6, the temporary signal mask and the timeout write-back pointer.

`epoll_wait`/`epoll_pwait2`, `rt_sigtimedwait` and `sigsuspend` return `EINTR` and are not restarted, matching Linux.

### sigsuspend, pause, sigtimedwait and temporary masks

`rt_sigsuspend` installs a temporary mask and parks the caller's own mask in `Task::saved_sigmask`; the signal that ends the wait is delivered under the temporary mask and the handler frame's `uc_sigmask` puts the old one back. `ppoll` and `pselect6` with a sigmask use the same mechanism (`masked_timed_wait`, Linux `set_user_sigmask` + `poll_select_finish`); any other result restores the caller's mask at once, so a signal only the temporary mask unblocked stays pending. Only a handler frame clears `saved_sigmask` — it used to be cleared whenever any signal was picked, so an ignored signal or a stop during a masked `ppoll`/`sigsuspend` left the temporary mask installed for good. `pause()` keeps the current mask (it used to install an empty one); `rt_sigsuspend(NULL)` is `EFAULT`; `rt_sigtimedwait` returns `EINTR` when some *other* handled signal arrives.

### Job-control stops

A real `Stopped` task state, not a terminate (`61f8bcd`):

- `SIGSTOP`/`SIGTSTP`/`SIGTTIN`/`SIGTTOU` at `SIG_DFL` stop **the whole thread group** (`do_signal_stop`): the receiving thread stops itself, off-CPU siblings are moved to `Stopped`, on-CPU siblings get `stop_pending` and a reschedule IPI. A stop discards any pending `SIGCONT`.
- `SIGCONT` resumes every stopped thread at send time (which is why its own default action is "ignore" by the time it is dequeued); `SIGKILL` also ends a stop and then kills (`c21be30`).
- The parent gets `SIGCHLD` with `CLD_STOPPED`/`CLD_CONTINUED` unless it set `SA_NOCLDSTOP`, and `waitpid(WUNTRACED)`/`waitpid(WCONTINUED)`/`waitid(WSTOPPED|WCONTINUED)` observe the transitions once each (leader bookkeeping: `stop_signal`, `stop_reported`, `cont_pending`).
- Background reads and writes on a terminal raise `SIGTTIN`/`SIGTTOU` from inside the syscall (Linux `n_tty` rules, including "ignored or blocked means `EIO` for a read and silent success otherwise"), and an orphaned process group with a stopped member gets `SIGHUP` + `SIGCONT`. The tty side — `^Z`/`VSUSP`, session-scoped `tcsetpgrp`, `TOSTOP` — is in [../servers/tty-and-vt.md](../servers/tty-and-vt.md).

Ctrl-T's task dump prints a stopped task as state `T` (see [../development/debugging.md](../development/debugging.md)).

## Design decisions

### One payload per signal, not a queue

**Choice.** `signal_pending` is a bitmask and each signal has one `SigInfo` slot.

**Why.** POSIX guarantees only a single pending instance of a standard signal, which is exactly what the bitmask models. Real-time queueing (`SIGRTMIN..`, `sigqueue` depth) is used by nothing this kernel runs, and a multi-entry queue would multiply the per-signal cost inside `Task` — which `Task::new_kernel` materialises as a literal on the 128 KiB kernel stack (`scripts/check-stack-frames.py` guards that). Coalescing is visible where it matters: a POSIX timer whose signal is still pending counts the coalesced expiry as an overrun.

### SIGKILL and SIGSTOP are enforced as undeniable

**Why.** `sigprocmask` used to apply the caller's set verbatim, and musl and Rust std block the *entire* signal set around `fork`/`posix_spawn` — so processes routinely ended up with `SIGKILL` masked on every thread. `deliver_signal_process` then found no eligible thread, parked the bit on the leader, and returned success: `kill -9` reported that it had worked while the target went on running (`8b1f76a`).

### A stopped thread loses channel wakes on purpose

**Choice.** A thread that was `Blocked` when stopped becomes `Stopped`; wakes arriving during the stop (`unblock_port*`, `futex_wake`, the deadline tick all require `Blocked`) are not recorded. `SIGCONT` makes it `Ready`, it returns from its yield as a spurious wake, and every blocking primitive re-checks its condition and re-parks.

**Alternative rejected.** Leaving it `Blocked` and having every wake path consult a stop flag spreads job control over a dozen wake sites. The re-check-after-yield contract already exists for every primitive, so the spurious wake is free.

### Prefer a blocked thread for process-directed signals

**Why.** It delivers and wakes in one step. The flip side — that the chosen thread is usually a worker, not the leader — is what made "terminate only the receiving thread" so wrong, and why every terminate path goes through `exit_group_signal`.

### Restart on the original deadline, not the remaining time

**Choice.** `ppoll`/`pselect6`/`select` and timed futex waits restarted after a stop/continue keep their original absolute deadline.

**Why, and the difference from Linux.** Linux re-executes these with the remaining time it wrote back when the signal arrived, so the stopped interval *extends* a Linux wait (a 500 ms wait stopped from 150 to 250 ms ends at about 600 ms on Linux, 500 ms here). Keeping the absolute deadline matches POSIX better and the tests assert it. `nanosleep` and the futex paths match Linux exactly (Linux also restarts those on the original deadline). Before the restart block existed, timed waits restarted with their full relative interval (futex: a 500 ms wait stopped at 150–250 ms returned at 753 ms) or failed with `EINTR` after a stop (`nanosleep`, `ppoll`, `pselect6`), which Linux has not done since 2.6.24 (`a3f0353`).

## Limitations and open work

- No real-time signal queueing (above).
- No core dumps; `WCOREDUMP` is never set.
- `SIGEV_THREAD` has no kernel meaning (libc builds it on `SIGEV_THREAD_ID`); it is accepted and never signals.
- `epoll_pwait`'s sigmask argument is ignored (see [event-loop.md](event-loop.md#limitations-and-open-work)).
- `signalfd` reads carry `ssi_signo`, `ssi_code`, `ssi_pid`, `ssi_uid`, `ssi_status`, but not `ssi_overrun` or `ssi_ptr` for timer signals.

## Code and tests

Regression suites (see [../development/testing.md](../development/testing.md)):

- `sigtest` — `sigaction()` struct field-order round trip, real delivery and return through the sigreturn trampoline, per-signal handler dispatch, `sigprocmask`/`sigpending` blocking and deferred delivery, `SIG_IGN`, and `raise()` (which resolves to `TKILL` against the caller's own tid, distinct from the `KILL`/`TGKILL` paths the other checks use); `handler_preserves_vector_regs`; the futex/restart cases; `timed_wait_stop_resumes_remainder`; `test_ppoll_pselect_sigmask`. Modes: `sigtest futex N`, `sigtest futexab N`, `sigtest stoprem`, `sigtest pollmask`.
- `sigtest2` — mask and disposition inheritance across `fork`/`pthread_create`, fault-handler `siginfo` delivery (including `siglongjmp` out of a handler), default-action kills while blocked or inside a handler, stop/continue/kill-while-stopped.
- `sigchldtest`, `waittest` — `SIGCHLD` payloads and wait-status encoding.
- `jobtest` — job control on a pty: `^Z`/`SIGTSTP`, `SIGTTIN`, `SIGTTOU`, `TOSTOP`, orphaned groups.
- `timertest` — `SIGALRM` delivery to `pause`/`sigsuspend`/`sigwaitinfo`, `SA_RESTART` on a blocked read, `si_value`, `SIGEV_THREAD_ID`.
- `killmt` — `kill -9` of multithreaded processes.

## See also

- [processes.md](processes.md) — exit_group, wait status, process groups
- [threads-and-futex.md](threads-and-futex.md#signals) — futex/signal interaction
- [time.md](time.md#posix-timers-and-itimers) — timer signals
- [event-loop.md](event-loop.md) — signalfd, and how signals wake parked pollers
- [arch-aarch64.md](arch-aarch64.md), [arch-x86_64.md](arch-x86_64.md) — exception and syscall entry paths
- [../servers/tty-and-vt.md](../servers/tty-and-vt.md) — terminal job control
