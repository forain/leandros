# lane pools — 2026-10-19/20 (openbugs L5.7–L5.10)

## L5.7 IPC port table — FIXED
`ipc/src/port.rs`: the 512-bucket open-addressed static table (every thread
that touches a mounted fs keeps one reply port until it exits) is now a
growable, generation-tagged slab (`MAX_LIVE_PORTS` 65536). The slot vector
and the queues are allocated with `PORT_TABLE` dropped. Reply ports
(`port::create_reply`, used by `vfs::call_port` and `sys_call`) get a 4-deep
queue. Per-thread reply-port reuse was already in place (one per thread,
lazily created, released by the task-exit hook).
procmax `fsthreads` (4000 threads, each doing a stat on the root f2fs, then
parking): before 3504/4000 stats failed with ENOMEM (~496 worked); after 0.

## L5.8 fixed pools
- AF_UNIX connections — FIXED: `UNIX_CONNS` is a heap Vec that grows to 8192
  (was a static 512). procmax `unixconns` (1200 socketpairs): before 509 then
  ENOMEM; after 1200.
- evdev clients — FIXED: 16 -> 64 slots, and a full pool refuses the open
  (`[EVDEV] client pool full` once) instead of LRU-evicting a LIVE queue.
  Pid-keyed queues (no VFS_CLOSE names them, the reason LRU existed) are now
  retired by the task-exit hook (`evdev_server::release_pid`, chained in
  `kernel/src/init.rs::task_exit_cleanup`).
- VFS LOCKS — ALREADY-FIXED (lane fdmax: grown on demand up to 1<<16,
  servers/vfs/src/lib.rs `MAX_LOCKS`).
- TMP_FILES 4096 — left alone (tmpfs pool, lane vfsabi's territory); it
  already answers ENOSPC with a once-per-boot console line.

## L5.9 spin::Mutex invisible to might_sleep — FIXED
spin 0.9.8 vendored (`vendor/spin`, LEANDROS-PATCH.md) with an
acquire/release hook; `sched/src/spinheld.rs` keeps a per-CPU count.
`sleep_forbidden_reason` reports "holding a spin::Mutex"; `preempt_check`
defers a tick preemption while it is non-zero; `yield_now` logs a voluntary
switch made with one held (`[KSYNC] task switched out holding N
spin::Mutex ...`, once per reason) and carries the count with the task. Boot
prints a self-check line (`[KSYNC] spin::Mutex held-count accounting on`).
Regress run (pools3): zero `[KSYNC]` violation lines; only the self-check line.
Not covered: `spin::RwLock`, `Once`/`Lazy`.

## L5.10
- Full-pipe writer yield-spin — ALREADY-FIXED: `sys_write` parks in
  `block_until_ready` on the pipe key (`vfs::fd_wake_tag`), and the reader
  wakes it on full -> writable (servers/vfs/src/lib.rs ~2653, ~7482). No
  yield loop remains on that path.
- writev O_APPEND per iovec — FIXED: a hashed sleeping append lock
  (`APPEND_LOCKS`, keyed by tmpfs inode / mount via
  `vfs::fd_append_lock_key`) is taken by every O_APPEND regular-file write
  and held by `sys_writev` across the whole vector. procmax `appendv` (two
  writers, 2000 3-iovec records each): before 3351 torn lines; after 0.

## Tests (aarch64, HVF)
regress: all green except host pollnarrowtest/nettag2test (1 stray wake), run
without LEANDROS_RUN_ID; both PASS on rerun with LEANDROS_RUN_ID=pools (the
secarm lane saw the same two fail). procmax PASS incl. the 3 new cases.
Final run pools3 (LEANDROS_RUN_ID=pools, rebased on main 685f6d02): overall PASS, no
KSYNC/IPC/EVDEV/unix-pool warnings in /tmp/leandros-pools-serial-full.log.
