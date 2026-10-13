# Processes

A LeandrOS process is a POSIX-shaped thread group, not a bare scheduler task: a TGID with threads under it, created by `fork`/`clone`, replaced by `execve`, torn down as a unit by `exit_group` or a fatal signal, and reaped by `wait4`/`waitid`. This page covers the process model, the ELF loader and `execve`, exit and reaping, and the per-process attributes the kernel keeps (credentials, root, priority). Threads and futexes have their own page, [threads-and-futex.md](threads-and-futex.md); signal delivery is in [signals.md](signals.md).

## Overview

| Concept | Where it lives | Notes |
|---|---|---|
| Task (one per thread) | `sched::task::Task` in [sched/src/task.rs](../../sched/src/task.rs) | pid, tgid, ppid, pgid, sid, credentials, signal mask, cwd/root, kernel stack, `Arc<AddressSpace>` |
| Thread group / process | `tgid` field; the leader is the task with `pid == tgid` | Signal dispositions, shared pending set, exit status, stop/continue bookkeeping live on the leader |
| Process creation | `fork_current` / `clone_thread` in [sched/src/clone.rs](../../sched/src/clone.rs), dispatched by `sys_clone_or_fork` in [kernel/src/syscall.rs](../../kernel/src/syscall.rs) | |
| Program loading | [elf/src/lib.rs](../../elf/src/lib.rs) (`load`, `load_lazy`) and `sys_execve` | ET_EXEC, ET_DYN, PT_INTERP, `#!` scripts |
| Exit / group teardown | `exit`, `exit_group`, `exit_group_signal`, `dethread_current_group` in [sched/src/lib.rs](../../sched/src/lib.rs) | |
| Reaping | `wait_scan` + the `EXIT_LOG` side table in [sched/src/lib.rs](../../sched/src/lib.rs) | |
| Permission rules | [sched/src/cred.rs](../../sched/src/cred.rs) | kill, setpgid, setreuid, setpriority, setrlimit |

Limits: `runqueue::MAX_TASKS` = 4096 tasks (threads), `MAX_PROCESSES` = 2048 thread groups (raised from 1024/512 by lane procmax, 2026-10-12, for an in-guest parallel `cargo build`). Per-process tables in the servers (VFS fd tables, socket tables, tty termios and timer tables, stdio flags, exe paths) and the zombie exit-record pool are sized to `MAX_PROCESSES`. `fork_admission` (in `sched/src/lib.rs`) checks the limits before a fork allocates anything and `admit_locked` checks them again under the hold that enqueues the child. `fork`/`clone` return `EAGAIN`, as on Linux, when the task table is full, when a new process would pass `MAX_PROCESSES` (live leaders plus unwaited zombie records, which count as Linux zombies do), or when a non-root caller passes its `RLIMIT_NPROC`. `RLIMIT_NPROC` defaults to `MAX_PROCESSES` and counts the tasks of the caller's real uid, threads included; root is exempt. A full task table also prints `[SCHED] task table FULL: N/4096 tasks … this is runqueue::MAX_TASKS, not RAM` once per boot. A kernel-stack allocation failure (128 KiB, order 5) still returns `ENOMEM`. `/proc/sys/kernel/threads-max` reports `MAX_TASKS`; `pid_max` reports Linux's 4194304, because pids are a monotonic u32 that is never reused.

Nothing scales with these constants on a hot path. Task walks go through the run queue's occupancy bitmap (`RunQueue::live_slots`), pid lookups through an authoritative pid index, `min_vruntime` through the runnable hint set, and `FUTEX_WAKE` through per-address hash buckets. So each costs the live (or runnable, or same-bucket) count. See [scheduler.md](scheduler.md).

## How it works

### Thread groups and the TGID key

A process is a TGID with threads under it. Everything that POSIX says is per-process is keyed by the TGID, never by the raw pid of whichever thread happened to make the call:

- VFS fd tables, socket tables, epoll instance ownership, POSIX timer tables, console termios, stdio flags, `/proc/<pid>/exe` paths (`EXE_PATHS`), and signal dispositions.
- The servers canonicalise at their single IPC entry point: `vfs::handle`, `net_server::handle` and `tty_server::handle` all start with `let caller_pid = sched::tgid_of(caller_pid)`.
- Per-thread state stays keyed by pid: the signal mask, the futex wait slot, the alternate signal stack, the TLS base, the restart block.

Resolving `tgid_of` used to take `RUN_QUEUE` and scan every task slot. It is now two lock-free structures: a per-CPU `CURRENT_TGID` published at dispatch (the common case, the caller's own tgid), and an open-addressed pid→tgid side table (`PID_TGID`, 4096 slots) written only from `RunQueue::enqueue`/`remove`. Removal writes a tombstone, not a zero: zeroing mid-chain would strand later entries and the symptom would be a silently *wrong* tgid via the `unwrap_or(pid)` fallback, not a crash. See [Design decisions](#tgid-lookup-is-lock-free-not-cached-per-call-site).

### fork and clone

`sys_clone_or_fork` splits on `CLONE_VM`:

- **`CLONE_VM` clear (`fork`)**: `fork_current` copies the parent's saved `UserFrame` to the child's kernel stack, sets the child's return value to 0, and clones the address space copy-on-write (see [memory.md](memory.md) for lazy CoW). The child inherits the parent's TLS base, signal mask, dispositions (`SIG_IGN` survives `fork`, per POSIX), credentials, cwd, root, umask, nice value and rlimits.
- **`CLONE_VM` set**: `clone_thread` shares the caller's `Arc<AddressSpace>`. With `CLONE_THREAD` the child joins the caller's group (pthread); without it the child is a *vfork-style* new process (musl's and Rust std's `posix_spawn` fast path, `clone(CLONE_VM|CLONE_VFORK|SIGCHLD)`) that borrows the address space until it `execve`s or exits.

Before the child is enqueued (the `before_enqueue` hook), the per-process kernel state it needs is duplicated so another CPU cannot dispatch it into a syscall against a table that does not exist yet: `VFS_FORK_DUP`, `NET_FORK_DUP` and `epoll_fork_dup`. Both fork paths first check `vfs::fd_table_slot_free()` and fail with `EAGAIN` up front; if enqueue then fails, `undo_unborn_child` releases the tables the child already took (this leaked four tables per 20-terminal run before it was added). The parent is identified by `tgid_of(current_pid())`, because a `fork` issued from a tokio worker thread otherwise names a pid no fd table matches and the child inherits an empty table.

`CLONE_VFORK` parks the parent on `VFORK_WAIT_CHANNEL` (three-phase, so a release that lands between the check and the park is not lost) until `vfork_complete` runs at the child's successful `execve` or exit. Signals do not interrupt this wait, as on Linux. Before 2026-09-16 the parent yield-spun, burning one CPU for every spawn's exec window.

`CLONE_PARENT_SETTID`, `CLONE_CHILD_SETTID`, `CLONE_CHILD_CLEARTID` and `CLONE_SETTLS` are honoured; the tid words are written through the VMA tables before the child becomes runnable (no user-memory access under `RUN_QUEUE`, see [invariants.md](invariants.md)), after `sys_clone_or_fork` has prefaulted them with no lock held. The `CLONE_PARENT_SETTID` story is in [threads-and-futex.md](threads-and-futex.md#clone_parent_settid-and-the-musl-thread-list-lock).

Parentage: a child's `ppid` is the *thread* that forked it. Everything that cares about the parent *process* goes through the tgid: `getppid` reports the leader's parent as a tgid, `wait_scan` matches children forked from any thread of the caller, and the `EXIT_LOG` record stores `parent_tgid` resolved at exit time (the forking thread may itself die first).

### The ELF loader

[elf/src/lib.rs](../../elf/src/lib.rs) maps ELF64 images; it applies no relocations. Two entry points:

- `load(bytes, as, bias)` — eager: maps every `PT_LOAD` and copies segment data. Used for initrd/ramfs binaries and as a fallback.
- `load_lazy(header, as, file_cap, bias)` — demand-paged: needs only the ELF header and program-header table; each `PT_LOAD` becomes a file-backed lazy VMA whose pages the fault handler reads on first touch, zero-filling past `p_filesz` (BSS). This replaced reading and copying the whole image at exec time — 397 MB for MAME.

Both return an `ElfInfo` with the biased entry point, `AT_PHDR`/`AT_PHENT`/`AT_PHNUM` values, `e_type`, and the file range of any `PT_INTERP` string.

Address-space layout chosen by `sys_execve`:

| Region | Address | Why |
|---|---|---|
| ET_EXEC image | its own `p_vaddr` (bias 0) | byte-identical to the original static userland |
| ET_DYN main image (PIE, static-PIE, dynamic) | `MAIN_DYN_BASE` = `0x20_0000` (2 MiB) | the same address ET_EXEC images always loaded at, so image + `brk` heap layout is unchanged |
| Dynamic interpreter (`/lib/ld-musl-<arch>.so.1`) | `INTERP_BASE` = `0x3000_0000` (768 MiB) | above any realistic `brk` heap, 256 MiB below the `mmap` bump at 1 GiB where ld.so's library mappings land |
| `mmap` bump (no hint) | from `0x4000_0000` | |
| User stack | top `0x7fff_ffff_f000`, 8 MiB | matches Linux's default `ulimit -s` |
| Signal-return trampoline | `0x7fff_ff00_0000` | see [signals.md](signals.md#signal-frames) |

With a `PT_INTERP`, the kernel maps the interpreter at `INTERP_BASE` and enters at *its* entry point; ld.so self-relocates using `AT_BASE`, processes the main program from the auxv and jumps to `AT_ENTRY`. Static and static-PIE binaries have no interpreter: the kernel enters their own entry and `AT_BASE` stays 0 (musl's `rcrt1` self-relocates from `AT_PHDR`). The heap start is captured from the main image *before* the interpreter load, which would otherwise move it after `INTERP_BASE`. The interpreter itself is demand-paged through the deduplicated, inode-keyed private-mmap registry (one entry for every dynamic process, no open-file slot held); the eager whole-file read (~1 MiB of libc per exec) is only the fallback.

A symbolisation tip that follows from the layout: a PIE main image's PC minus `0x200000` is its file offset, e.g. `eu-addr2line -e cosmic-comp $((pc - 0x200000))`.

### execve

`sys_execve` runs in this order:

1. **Resolve the path** through the shared resolver (so `./prog` and `prog` name the same file as every other path syscall; chroot confinement applies).
2. **`#!` scripts**, as Linux's `binfmt_script`: `argv[0]` is dropped and `[interpreter, optional-arg, script-path]` goes in front of `argv[1..]`; nesting repeats the rewrite up to `SHEBANG_MAX_DEPTH` (`ELOOP` past it). Execute permission is checked at every level (`exec_permitted`); a missing file still reports `ENOENT`, not `EACCES`.
3. **Find the image**: demand-paged path first (`open_exec_header` → `vfs::steal_mounted_file(tgid_of(pid), fd)` → `EXEC_FILES` registration); if `EXEC_FILES` (64 slots, each pinning one of f2fs's 256 open files) is full, the image is backed by inode through `MMAP_FILES` instead; then the eager VFS read; then `get_file_data_by_path`; then the initrd.
4. **Build the new address space** privately — image and interpreter are loaded *before* argv/envp are collected, because the collection holds the global `EXEC_ARGV`/`EXEC_ENVP` buffers and the loaders' file I/O used to make every other `execve` in the system wait.
5. **Collect argv/envp** (`MAX_EXEC_ARGS`, `E2BIG` past it) with fault-checked reads, and **reserve the user stack** as a lazy 8 MiB VMA; only the pages holding argv/envp/auxv plus a 4-page prefault below the initial SP are populated. Nothing is mapped below it, so running off the bottom faults outside any VMA and raises `SIGSEGV`. Until 2026-09-26 all 8 MiB were allocated eagerly at every exec.
6. **Write the initial stack**: argc, argv, envp, then the auxv — `AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, `AT_RANDOM`, `AT_PAGESZ`, `AT_UID`/`AT_EUID`/`AT_GID`/`AT_EGID`, the private server-port tags (`AT_LEANDROS_VFS_PORT` = 256, `…_NET_PORT` = 257, the audio port; see [ipc.md](ipc.md) for auxv service discovery), `AT_ENTRY`, `AT_BASE`, `AT_SECURE`, `AT_HWCAP`, `AT_EXECFN`, `AT_NULL`. The tags are emitted uniformly for ET_EXEC too; musl and relibc ignore the extras.
7. **Point of no return**: `dethread_current_group()` terminates every other thread (see below), then the close-on-exec sweeps (`VFS_EXEC_CLOEXEC`, `NET_EXEC_CLOEXEC`, `epoll_exec_cloexec`, socket-alias pruning), `vfork_complete`, `set_exe_path`, `reset_handlers_on_exec` (caught handlers revert to `SIG_DFL`; `SIG_IGN`, the mask and pending signals survive), `clear_child_tid` zeroed, and finally `replace_address_space`, which never returns.

The de-thread must precede the close-on-exec sweep, as Linux's `de_thread` precedes `do_close_on_exec`. With the order reversed, closing a `SOCK_CLOEXEC` signal socketpair woke a tokio worker parked in `epoll_wait` on it, whose read of the already-closed end failed `EBADF` before the kill reached it — brush's "Bad read on self-pipe: EBADF" panic whenever it ran `exec <cmd>` (greetd's session wrapper).

Because `replace_address_space` never returns, nothing still alive in `sys_execve`'s frame is ever dropped. The eager ELF buffer (hundreds of MB for large binaries) and argv/envp buffers are dropped explicitly before the swap, and the new `Box<AddressSpace>` is unboxed in an inner scope — keeping the `Box` leaked one 56-byte slab object per exec.

### Non-leader execve: taking over the leader's pid

When a non-leader thread calls `execve`, `dethread_current_group` → `take_over_leader` does what Linux `de_thread` does: it reaps every other sibling first, then exchanges pids with the leader under one `RUN_QUEUE` hold. The caller becomes `tgid` and inherits the leader-only state — signal dispositions, the shared pending set and its payload slots, the leader's thread-pending signals, ppid/pgid/sid, credentials, stop/continue bookkeeping, `reply_port`. Children whose `ppid` named the old tid are re-parented, `CURRENT_PID[cpu]` is republished, and the old leader's `Task` retires under the caller's old tid as a plain thread exit. An on-CPU leader is parked with `stop_pending`, never marked Zombie (its own CPU would reap a `pid == tgid` Zombie as a *process* exit). If the leader is already gone (the main thread called `pthread_exit`), the caller is promoted in place and the side table and `CURRENT_TGID` are republished.

### Exit and group teardown

- `exit(code)` ends the *calling thread*. It is the right primitive for a thread returning from its start routine.
- `exit_group(code)` ends the *process*: `claim_group_exit` makes exactly one thread the owner (the leader's `group_exit_owner`), `reparent_children` hands live children to init, `run_group_kill` loops `kill_next_group_member` until every sibling has actually stopped (opening an IRQ window while it waits, because the other CPU may be waiting on *this* one for a TLB-shootdown acknowledgement), and `run_exit_teardown` releases each reaped sibling's fds, sockets, epoll instances, VT and evdev grabs. If the owner is not the leader, it also runs the leader's teardown and sends the parent's `SIGCHLD`, which only the leader's own `exit` would otherwise have sent.
- `exit_group_signal(signo)` is the only path for a default-action kill: it stamps `term_signal` on every thread **before** the group kill (a sibling logged to `EXIT_LOG` first would otherwise lose it), then exits with code `128 + signo` for the serial exit log while the wait status carries the real signal.
- A thread that finds the group already dying (`AlreadyDying`) just marks itself Zombie and leaves the CPU: no fd teardown, no `clear_child_tid`, no `SIGCHLD`.

On the way out, `release_exiting_address_space` drops the last thread's address space *before* the task is marked Zombie (after the `clear_child_tid` write): it detaches TTBR0/CR3 under `RUN_QUEUE`, sets `page_table = 0`, then drops the space once `busy` clears. This matches Linux, where `exit_mm` runs before `exit_notify`, and it closed a measurement window in which `sysinfo` read straight after `wait4` still counted the dead child's 8–15 MiB.

### Reaping: wait4, waitid and the exit log

`wait4`/`waitid` select by pid, any child, or process group (`WaitSel`), and by what changed (`WaitWhat`: exited, stopped, continued — `WUNTRACED`/`WCONTINUED`, `WEXITED`/`WSTOPPED`). They block on the poll wait-channel tagged `poll_class::WAIT` + the caller's tgid, rather than spinning; `log_exit` requests that wake on every exit.

The scheduler reaps a zombie's run-queue slot as soon as it can (only the owning CPU's loop may physically recycle it), so a waiter can arrive after the task is gone. `log_exit` therefore records every exit in `EXIT_LOG` as an `ExitRecord { pid, status, parent_tgid, pgid, is_process, consumed }`. `wait_scan` reports a zombie still in its slot in place (marking it reported, so the later log record is born consumed) and otherwise falls back to the log. This is what closes both the reaper-vs-waiter race and the `SIGCHLD` → `wait4(WNOHANG)` race: a child is waitable the moment it is a zombie.

`EXIT_LOG` has two parts. Unconsumed process records go to a pool of `MAX_PROCESSES` slots that is never overwritten; a record leaves it only when a waiter consumes it. Thread exits and records that are already consumed go to a 256-entry ring, which only `get_exit_code` reads. Until 2026-10-12 everything shared that one ring, and a burst of more than 256 exits before the parent's `wait4` overwrote unconsumed records. A parallel build does this easily, because each rustc exits with a dozen threads. The parent's `wait4` then returned `ECHILD` for a child that had exited normally, and cargo reported "could not execute process". Unwaited records count toward the process limit, so a parent that never waits gets `EAGAIN` from `fork` before the pool can fill. A parent whose `SIGCHLD` is `SIG_IGN` or has `SA_NOCLDWAIT` gets its children reaped at exit, as on Linux, so its records are born consumed. When a parent dies, `reparent_children` hands its unwaited records to init, together with its live children. `procmax`'s `burst` case covers this path: 400 children that each start two threads all exit before any `wait4`. A zombie whose slot is already recycled is still a zombie to userspace while its record sits in the pool: `pidfd_open`/`pidfd_send_signal` and pidfd readiness (`process_exit_state`) look its pid up there, and `setpgid` into a group whose only member is such a zombie finds the group through the record's `pgid` (`exit_logged_pgid`). Both lookups walk the pool's occupancy bitmap (`ExitLog::any_pending`), so they cost the number of unwaited zombies.

Wait status encoding (`ExitStatus::wait_status`): a normal exit is `(code & 0xff) << 8`; a signal death puts `signo` in the low seven bits. `WCOREDUMP`'s 0x80 bit is deliberately never set — there is no core-dump path, and reporting a dump that was not written sends shells ("(core dumped)") and CI harnesses looking for a file that does not exist. `waitid`'s `si_code`/`si_status` are derived from the same `ExitStatus`, never by hand; they used to disagree (`si_code` was hardcoded to `CLD_EXITED`).

### pidfds

`pidfd_open(pid, flags)` installs a VFS descriptor (`VnodeKind::PidFd { pid }`) naming a process by tgid. Pids are never reused (`alloc_pid` only counts up), so the number is a stable identity and the descriptor needs no pool or teardown. `EINVAL` for a non-leader thread or unknown flags (only `PIDFD_NONBLOCK`), `ESRCH` once the process has been reaped; pidfds are always close-on-exec.

- **Readiness.** `POLLIN` once no thread of the group is still running (`sched::process_exit_state`, Linux `thread_group_exited`), `POLLIN|POLLHUP` once a waiter has consumed the exit. The exit is a targeted wake on `poll_class::PIDFD` + tgid: immediate when the leader exits, deferred to the tick for any other thread (the leader can `pthread_exit` first), backed by `log_exit`. The epoll edge seq is simply "exited", so an `EPOLLET` registration fires once.
- **Why it has to be exact.** tokio — brush's runtime — opens a pidfd for every child it spawns and, when that works, reaps from it: it waits for `EPOLLIN`, then calls `waitpid(pid, WNOHANG)`. Readiness before the child is waitable would spin that loop; readiness after would hang the pipeline. Both conditions are the same "leader is a zombie and no thread is live" test `wait_scan` uses.
- `waitid(P_PIDFD, fd, …)` waits for the named child (`ECHILD` if it is not ours) and answers `EAGAIN` on an `O_NONBLOCK` pidfd instead of blocking.
- `pidfd_send_signal(fd, sig, info, 0)` is `kill`/`rt_sigqueueinfo` through the pidfd: a zombie accepts and drops it, a reaped process is `ESRCH`.
- `clone(CLONE_PIDFD)` installs the child's pidfd in the parent after the fork (the child's copy of the table never holds it, as on Linux) and writes it through `parent_tid`; with `CLONE_PARENT_SETTID` or `CLONE_THREAD` it is `EINVAL`. `clone3` stays `ENOSYS`.
- `pidfd_getfd(fd, targetfd, 0)` duplicates another process's descriptor through the SCM_RIGHTS transfer path (`net_server::dup_fd_across`), close-on-exec; the ptrace-attach check is reduced to "same real uid, or root".

### Process groups, sessions and orphans

Tasks carry `pgid` and `sid`; `setpgid`/`setsid`/`getsid` follow Linux's rules (own process or own child, same session, no session leaders), enforced in [sched/src/cred.rs](../../sched/src/cred.rs). `kill(0, sig)`, `kill(-pgid, sig)` and `killpg` deliver once per process in the group (`kill_pgrp`). When a process exits, `reparent_children` re-parents its children to init and collects candidate groups that may have just become orphaned; an orphaned group with a stopped member gets `SIGHUP` + `SIGCONT`, the POSIX orphaned-process-group rule. Terminal-side job control (foreground group, `^Z`, `SIGTTIN`/`SIGTTOU`, `tcsetpgrp`) is in [../servers/tty-and-vt.md](../servers/tty-and-vt.md); the signal side (the `Stopped` state) is in [signals.md](signals.md#job-control-stops).

### Credentials, root and priority

- **Credentials**: real/effective/saved uid and gid plus supplementary groups per task, with `setresuid`/`setresgid` and the Linux permission rules in [sched/src/cred.rs](../../sched/src/cred.rs). There are no capability sets: euid 0 holds every capability. Accounts, `/etc/shadow`, `/bin/login`, the PID-1 getty loop and permission enforcement are described in [../servers/security.md](../servers/security.md).
- **`chroot(2)`** (root only) is real confinement: symlink resolution and fd-to-path resolution are both confined to the jail rather than escaping through the mount table. If the caller's cwd is outside the new root, it is re-anchored at the root.
- **Priorities**: `setpriority`/`getpriority` back `nice(1)`. `setpriority` clamps to -20..19 and follows Linux `set_one_prio` (EPERM unless the caller owns the target or is root; EACCES for lowering the nice value past `RLIMIT_NICE`, 0 by default). `getpriority` returns `20 - nice`, the Linux ABI (musl undoes the bias). The nice value sets the task's EEVDF weight; see [scheduler.md](scheduler.md).
- **`/proc/<pid>/exe`** is synthesised by the VFS server (there is no separate proc server) from the tgid-keyed `EXE_PATHS` table for any pid, not just the `self` alias. It resolves inside `readlink(2)` only — nothing `open`s or `execve`s that path, which is why greetd's session worker must call `std::env::current_exe()` before it forks rather than handing the magic path to `execv`. See [../servers/vfs.md](../servers/vfs.md).

## Design decisions

### Fatal signals and faults end the whole thread group

**Choice.** Every default-action kill and every fatal user fault goes through `exit_group_signal`, never `exit`.

**Why.** `exit` reaps only the calling thread, and `deliver_signal_process` deliberately prefers a *blocked* thread as the delivery target (so the signal also wakes it) — in a tokio program that is almost always a worker parked in `epoll_wait`, not the leader. A process routinely survived its own `kill -9`: measured on aarch64, `kill -9` of brush terminated the target 1 time in 8 before the fix (`8b1f76a`) and 8/8 after. x86-64 scored 6/6 both ways; the defect is arch-independent, only the race window differs. Fatal user faults had the same shape and were fixed the same way (`ed75cc7`).

### One owner runs the group kill, and the address space is reference-counted per thread

**Choice.** `claim_group_exit` elects a single owner; every `CLONE_THREAD` sibling holds its own reference to the shared `AddressSpace`, which is freed only when the *last* thread's `Task` drops — and the scheduler drops it only after that thread is off its CPU and the CPU is back on the kernel page table.

**Why.** Two threads in `exit_group` at once (two fatal signals, or a sibling that faulted because its page tables were on the way out) each marked the other Zombie and spun with IRQs masked for the other to leave its CPU — the `[WDOG]` wedge behind `kill -9` of a multithreaded process. And when only the leader owned the address space, a non-leader running the group kill could reap the leader while it and other siblings were still executing on those tables on other CPUs: a freed page-table root under a live CPU (`6b206f0`). `killmt` exercises eleven modes of this, including siblings spinning on other CPUs at the moment of the kill.

**Refuted on the way.** Freeing an off-CPU Blocked sibling's kernel stack in place was suspected of corruption; it is safe, because `on_cpu == None` is set by the task's own CPU only after `cpu_switch_to` returned onto the scheduler stack, and the kill loop checks it and removes the slot in the same `RUN_QUEUE` hold (`parked_mix` 200/200, flat memory). What the census did find was that `AddressSpace::drop` never freed the intermediate page tables — about 68 KiB per small process, hundreds of pages per compositor — now fixed with `arch_free_user_page_tables` on both arches (`bf51e25`).

### execve takes over the leader's pid instead of promoting the caller

**Choice.** Linux `de_thread` semantics: the exec'ing thread becomes the leader *under the leader's pid*.

**Why.** The previous design promoted the caller to a new single-thread group under its own pid, and went wrong three ways at once (proven with `killmt exec_worker`, which reported an exit status of 0 to the parent while the process was still exec'ing): the kill loop reaped the old leader as a *process* (whole fd table torn down, the parent handed a spurious exit), the side table and this CPU's cached tgid still said "old leader" until the next dispatch, and once they caught up the new image found an empty fd table under its new key. Everything per-process is keyed by the tgid, and keeping the tgid is what keeps all of it valid (`bf51e25`).

**Refuted.** A suspected ~121 KiB-per-exec leak on this path ("order-5 blocks 601→374") was fragmentation plus a measurement window, not a leak: a plain exec and a takeover exec both leak nothing by buddy census. The window (an address space dropped later by the reaping CPU) was then closed in the kernel by `release_exiting_address_space`.

### Exit-log side table rather than delaying the reaper

**Choice.** The scheduler reaps zombies eagerly and records exit codes in `EXIT_LOG`; `wait` falls back to it.

**Why.** Only the owning CPU's scheduler loop may physically free a task slot (its kernel stack may still be in use until it switches away), so the reaper cannot simply wait for the parent. Recording the exit and letting `wait_scan` consult both the live zombie and the log removes the race between the reaper and the waiter without coupling the two.

**Limitation (fixed 2026-10-12).** The log used to be one 256-entry ring shared with thread exits, so an unwaited child's status could be overwritten. Unconsumed process records now have their own pool, walked through an occupancy bitmap, so a `wait4` costs the number of pending zombies rather than the pool size. Thread exits keep the ring.

### TGID lookup is lock-free, not cached per call site

**Choice.** `tgid_of` reads `CURRENT_TGID[cpu]` for the caller and the `PID_TGID` side table for anyone else; epoll instances additionally cache `owner_tgid` at create.

**Why (measured).** On a live COSMIC session on x86_64/KVM, `tgid_of` — `RUN_QUEUE.lock()` plus a linear scan — was 10.6% of running guest CPU, called from the epoll ownership check on every `epoll_wait` (`f7c4577`, `eb9422e`). After the side table it left the profile entirely. Caching the tgid in the epoll instance is also *more* correct than resolving it: `tgid_of` falls back to the pid itself when a task is gone, so once the thread that called `epoll_create1` exited, its still-open epoll fd used to fail `EBADF` for every surviving sibling.

### Raw TID reaching a TGID-keyed table is a recurring bug class

At least seven instances were found and fixed (`install_dmabuf_vmo`, eventfd/timerfd refcounts, `vfs_get_node_kind`, `tmpfile_owner_of` in `77f170d`, the POSIX timer table, `sys_fcntl`'s stdio `F_GETFL`, `sys_execve`'s `steal_mounted_file`). The rule that ended it is structural: canonicalise once at each server's IPC choke point, and make every `pub fn` that bypasses `handle()` canonicalise itself. The inverse mistake is real too: teardown helpers called on *every* thread exit must stay gated to leader exits (`if sched::tgid_of(pid) == pid`), or a worker thread's exit would wipe process-wide console/timer state out from under its siblings.

## Limitations and open work

- **`AT_RANDOM` is derived from the tick counter**, not from the kernel CSPRNG that `getrandom` uses, so it is predictable; it seeds libc stack-protector canaries and pointer guards. **`AT_SECURE` is always 0** and **`AT_UID`/`AT_EUID`/`AT_GID`/`AT_EGID` are always 0**, whatever the process's real credentials (libcs read the syscalls instead, so nothing visible breaks today). There is no set-user-ID exec.
- **cwd, root and umask are per-thread**, copied at `clone` time rather than shared; a `chdir`/`umask` in one thread is invisible to its siblings (Linux shares them under `CLONE_FS`). Same shape as the TGID bug class, but a bigger behavioural change than a one-line fix.
- A signal-killed group still becomes waitable when its leader is reaped, before a sibling's CPU drops the last address-space reference; `killmt` tolerates the window, the kernel still has it.
- `with_task_address_space` (evdev/drm) reads a page-table root without taking a reference, a latent race against any address-space drop.
- A `SIGKILL` landing during the takeover window of a non-leader `execve` can still reap the parked leader as a process exit.
- No core dumps. `clone3` is `ENOSYS` (callers fall back to `clone`), so `CLONE_PIDFD` is only reachable through `clone`.
- `times(2)` reports zero CPU times (the per-task `cpu_ns` behind `CLOCK_*_CPUTIME_ID` is not wired into it).

## Code and tests

| What | Where |
|---|---|
| Task, states, `SigInfo`, `ExitStatus` constants | [sched/src/task.rs](../../sched/src/task.rs) |
| fork, clone, vfork | [sched/src/clone.rs](../../sched/src/clone.rs) |
| exit, exit_group, de-thread, wait, `EXIT_LOG`, `PID_TGID`, `EXE_PATHS` | [sched/src/lib.rs](../../sched/src/lib.rs) |
| Credentials and permission rules | [sched/src/cred.rs](../../sched/src/cred.rs) |
| ELF loader | [elf/src/lib.rs](../../elf/src/lib.rs) |
| `sys_execve`, `sys_clone_or_fork`, `sys_wait4`, `sys_waitid`, `sys_pidfd_*`, `sys_chroot`, `sys_setpriority` | [kernel/src/syscall.rs](../../kernel/src/syscall.rs) |
| Original design record for ET_DYN/PT_INTERP/auxv | [../design/k3-elf-design.md](../design/k3-elf-design.md) |

Regression suites (see [../development/testing.md](../development/testing.md)): `forktest`, `waittest`, `sigchldtest`, `racetest` (lifecycle, `wait4`/`waitid`, `SIGCHLD`, process-group waits); `killmt` (eleven `kill -9` and exec modes including `exec_worker`, `exec_plain`, `fork_exit`, `parked_mix`, `leader_kills`, with a median-of-chunks free-memory leak bound of 8 MiB that tolerates compositor churn); `exectest` (`#!` scripts, exec permission); `syscalltest` (the pidfd family); `memtest` (`lost_kib_per_death`); `jobtest` (job control on a pty); `vfstest` (`chroot_confines_symlink_resolution`); the dynamic-linking ladder `hello-dyn`, `hello-dyn-rs` and `dlopen-host` packed by `scripts/mkfs-f2fs-populated.py`.

## See also

- [threads-and-futex.md](threads-and-futex.md) — `CLONE_VM` threads, futexes, the pthreads split
- [signals.md](signals.md) — delivery, frames, job-control stops
- [memory.md](memory.md) — address spaces, CoW fork, demand paging
- [scheduler.md](scheduler.md) — run queue, EEVDF weights, the `Stopped` state in the scheduler
- [ipc.md](ipc.md) — auxv service discovery
- [../servers/security.md](../servers/security.md) — accounts, login, permission enforcement
- [../servers/tty-and-vt.md](../servers/tty-and-vt.md) — terminal job control
