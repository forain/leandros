# lane/execfd — non-leader execve identity, group-kill stack ownership — 2026-09-18

Machine: linux desktop 172.16.158.150, worktree
`/run/media/forain/samsung970pro512/leandros-siblings/leandros-execfd`, x86_64/KVM.
**aarch64 NOT verified (wrapped up early on orchestrator request; the aarch64
kernel type-checks on the Mac).**

## Item 1 — execve from a non-leader thread (FIXED, verified x86_64/KVM)

Root cause (proven on baseline `7a9ed31` with the new `killmt exec_worker`
mode: report `""`, parent's waitpid status `0x0`): `dethread_current_group`
ran the ordinary group-kill loop, which reaped the *leader* as a process —
`log_exit(is_proc=true)` (the parent saw an exit-0 while the process was
exec'ing) and `run_exit_teardown(tgid)` (the whole fd table, sockets, epoll,
VT closed) — then set the caller's `tgid = pid`. Everything keyed by tgid
(VFS fd tables, net, epoll, exe path, POSIX timers, tty, `PID_TGID` side
table, `CURRENT_TGID` cache — the latter two stayed stale until the next
dispatch) then pointed at a dead pid. The new image had no fds and a new pid.

Fix (`sched/src/lib.rs`): Linux `de_thread`. `take_over_leader` reaps every
other sibling first, then exchanges pids with the leader under one RUN_QUEUE
hold: the caller becomes `tgid`, inherits the leader-only state (signal
dispositions, shared pending set + payload slots, leader's thread-pending
signals, ppid/pgid/sid, creds, stop/cont bookkeeping, `reply_port`), children
with `ppid == old tid` are re-parented, `CURRENT_PID[cpu]` is republished; the
old leader's Task retires under the caller's old tid as a plain thread exit
(`hook(old_pid)` frees the caller's old reply port, stack freed, AS Arc
dropped, `pid_tgid_remove(old_pid)`; the `tgid→tgid` side-table entry stays
valid). An on-CPU leader is parked with `stop_pending` (never Zombie: its own
CPU would reap a `pid == tgid` Zombie as a process exit). The dispatch loop's
switch-back identity check now re-reads `CURRENT_PID[id]` instead of the
dispatch-time copy. `clear_child_tid` is zeroed on every exec (Linux
`mm_release`). Fallback when the leader is already gone (pthread_exit from
main): promote in place AND republish `PID_TGID` + `CURRENT_TGID`.

Evidence: `killmt 200 exec_worker` **200/200** functional (pid preserved,
leader-opened + thread-opened fds readable, close-on-exec fd gone, a child
forked by the thread waitable after exec, `/proc/self/exe`, exit 42 reaches
the parent's waitpid). **But its memory census FAILS: −24084 KiB over 199
iterations ≈ 121 KiB/exec, order-5 (128 KiB) free blocks 601→374** — one
kernel-stack-sized block per iteration leaks on this path. Not root-caused
(see "where I stopped").

## Item 2 — `kill_next_group_member` freeing an off-CPU Blocked stack (REFUTED as corruption; a different leak found and fixed)

The in-place free is safe: `on_cpu == None` is set by the task's own CPU only
after `cpu_switch_to` returned onto the scheduler stack, and the kill loop
checks it and removes the slot in the same RUN_QUEUE hold, so nothing can
dispatch, wake (`futex_wake`, `unblock_port`, poll deadlines all go through
the run queue by pid), or inspect (`task_census`, `dump_group_stacks` hold
the lock) the task afterwards; no wait structure stores a pointer into a
kernel stack (`futex::remove_waiter` retires the pid-keyed slot). Evidence:
`parked_mix` (workers blocked in pipe read / nanosleep / futex, signal lands
on one of them, the other two reaped off-CPU in place) **200/200, mem −148
KiB**, `leader_kills` **200/200**, task count stable at 30, no `[PF]`,
`[WDOG]` or refused frees.

What the census DID find, on baseline, in every mode with a process death:
**`AddressSpace::drop` never freed the intermediate page tables** (PDPT/PD/PT,
L1–L3) — ~17 pages (68 KiB) per small process, hundreds of pages for a
compositor (almost certainly the "~140 MB per compositor death" open item).
Fixed with `arch_free_user_page_tables` on both arches (`mm/src/paging.rs`,
`mm/src/vmm.rs`, `arch/*/src/paging.rs`): walks the user tree, frees table
pages only (huge/block entries and leaf frames skipped; x86 upper-half PML4
entries are the shared kernel tables and are never followed). After the fix
`parked_mix` is flat; before it every mode dropped 6.4–6.7 MiB per 100.

## Where I stopped / next steps / unverified
- **aarch64 unverified** (not built or booted). Build + run
  `killmt 100` (all modes) on aarch64 before merging.
- **exec_worker leaks ~121 KiB per exec** (order-5 block). Bisect: run a
  single-threaded exec loop (e.g. `exectest`-style, or add a killmt mode with
  no sibling threads) to see if plain execve already leaks on this kernel;
  if not, suspect the takeover path (candidates: a Task Box not dropped, or a
  kernel stack of a sibling reaped through `exit_dying_thread` while
  `on_cpu` was Some). `python3 /tmp/execfd-ctrlt.py` on the desktop sends
  Ctrl-T and prints the buddy per-order census; order 5 is the stack size.
- `killmt`'s `MEM_LEAK_BOUND` (4 MiB) is noisy under the running greeter
  (tokio threads come and go: ±5–12 MiB swings seen); consider 8 MiB or
  measuring order-5 blocks instead of freeram.
- Full existing `killmt` mode battery not re-run on the fixed kernel (only
  exec_worker, parked_mix, leader_kills at 200 each).
- Pre-existing, untouched: `fork_current` records the forking *thread* as
  `ppid`; `setuid`/`chdir` are per-thread; a SIGKILL that lands during the
  takeover window can still reap the parked leader as a process exit.

Branch state: `lane/execfd` builds (x86_64 built + booted; aarch64 kernel
`cargo check` clean). **Not mergeable until aarch64 is verified and the
exec_worker leak is explained.**
