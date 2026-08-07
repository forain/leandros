# TGID-vs-raw-TID lookup audit (FINAL)

Host-only, read-only pass. Repo NOT modified — this is the audit doc for the
tree wave to apply. Scope: kernel/src, servers/vfs, servers/net, servers/tty,
drivers/, sched/.

## Convention under audit

`vfs::handle()` (servers/vfs/src/lib.rs:1453-1462) and `net_server::handle()`
(servers/net/src/lib.rs:673-679) both canonicalize `caller_pid` to
`sched::tgid_of(caller_pid)` at the single IPC entry point, because their
fd/socket tables are keyed by thread-group id (a process's threads share one
fd table). Any syscall is reachable from any thread, so a raw
`current_pid()` reaching a TGID-keyed table directly — bypassing that one
choke point — silently misses on non-leader threads. Three prior fixes
(install_dmabuf_vmo 146089c, eventfd/timerfd refcounts cb8ba58,
vfs_get_node_kind 5c62bdf) are this exact class.

## Tables inventoried

| Table | File | Keyed by | Canonicalization point |
|---|---|---|---|
| `FD_TABLES` (`ProcFdTable`) | servers/vfs/src/lib.rs:1117 | `.pid` (holds TGID) | `vfs::handle()` entry, line 1462 |
| `TMP_FILES` / `TMP_VMOS` | servers/vfs/src/lib.rs:326,378 | data-slot index, not pid | N/A — not pid-keyed |
| `EVENTFD_COUNTERS/SEQ/REFS`, `TIMERFD_POOL/REFS` | servers/vfs/src/lib.rs:984-1037 | fd-slot index (reached only via `VnodeKind::{EventFd,TimerFd}{slot}` resolved through FD_TABLES) | inherits FD_TABLES's canonicalization; SAFE |
| `STDIO_FLAGS` | kernel/src/syscall.rs:4492 | `.pid` (TGID, set by `set_stdio_flags`'s own `tgid_of`) | only at the setter; **not** at one reader — see CONFIRMED #2 |
| `EPOLL_INSTANCES` / `EPOLL_FDS` | kernel/src/syscall.rs:6045,6105 | `.owner_pid` (stored as TGID by `sys_epoll_create1`) | every reader compares `tgid_of(owner_pid) != current_tgid()` — SAFE, if slightly indirect |
| `TIMER_TABLES` (`ProcTimerTable`) | servers/tty/src/lib.rs:333 (approx, near `MAX_PROCS`) | `.pid`, intended as TGID per its own doc comment | **nowhere** — see CONFIRMED #1 |
| `TTY_TABLES` (`ProcTtyTable`) | servers/tty/src/lib.rs | `.pid`, intended as TGID | only `handle_ioctl`; `handle_open/read/write/close/isatty` do not — but this whole message family (TTY_OPEN et al.) is dead code, never dispatched from syscall.rs |
| `CONSOLE_TERMIOS` | servers/tty/src/lib.rs:194 | `.pid`, intended as TGID | only `handle_ioctl` (which is reachable, and does canonicalize) |
| `EXE_PATHS` | sched/src/lib.rs:861 | `.tgid` explicitly | every call site already passes a `tgid_of(...)`-derived value — SAFE |
| `FUTEX_TABLE` | sched/src/futex.rs:35 | `.pid` = raw TID, **by design** (each thread waits/wakes individually) | N/A — correctly per-thread, not a bug |
| socket tables | servers/net/src/lib.rs | internal, all reached through `net_server::handle()` | single choke point, always canonicalized — SAFE |
| `DUMB_BUFFERS` | drivers/src/drm_device_interface.rs:337 | GEM handle (global, no pid/owner field at all) | N/A — not pid-keyed, out of this bug's scope |
| `signal_actions` (per-tgid) vs `signal_mask` (per-thread) | sched/src/signal.rs | `sys_sigaction` explicitly resolves `tgid` then `find_pid_mut(tgid)`; `sys_sigprocmask` correctly stays on raw `pid` | both correct, opposite classes, and the file gets the distinction right |
| `Task.umask` / `Task.heap_end` | sched/src/lib.rs:903,913 + clone.rs | per-`Task` (per-thread) field, **copied** (not shared) at clone time | related-but-different class — see note below, not filed as CONFIRMED |

## Counts

- CONFIRMED-BUG (raw TID reaches a TGID-keyed table, reachable today): **3**
- CONFIRMED-BUG-IF-ACTIVATED (same defect, but currently dead code): **1** (5 handler fns share it)
- SUSPECT / needs a design call before fixing (fixing naively introduces a new bug): **2** (closely related, same root cause)
- SAFE (canonicalized correctly, or correctly per-thread by design): **9** tables/paths, listed above
- Out-of-class, noted but not filed as this bug: **1** (umask/heap_end architecture)
- Inverse-class (TGID applied where per-thread was needed) found: **0** — checked `sched/src/signal.rs` (sigaction vs sigprocmask split) specifically, both correct.

## CONFIRMED-BUG list (file:line)

1. **`servers/tty/src/lib.rs` — entire POSIX-timer family, `TIMER_TABLES` never canonicalized.**
   - Table: `TIMER_TABLES` (`ProcTimerTable`, doc'd as "a small per-process timer table").
   - Accessors with no `tgid_of` anywhere in the chain: `get_or_create_timer_table` (~L351), `check_timers` (~L445), `ensure_real_timer` (~L470), `close_all`'s timer branch (~L489), `handle_timer_create` (~L795), `set_timer_ticks`/`get_timer_ticks` (~L831,859) and everything built on them: `handle_timer_settime` (~L886), `handle_timer_gettime` (~L918), `handle_timer_getoverrun` (~L929), `handle_timer_delete` (~L941), `set_real_itimer`/`get_real_itimer` (~L875,882).
   - Call chain proving raw pid reaches it: `kernel/src/syscall.rs` — `sys_timer_create` L6859 `let pid = current_pid()`, `sys_timer_settime` L6883, `sys_timer_gettime` L6892, `sys_timer_getoverrun`/`sys_timer_delete` (same shape), `sys_setitimer` L6843 `let pid = current_pid()` → `tty_server::set_real_itimer(pid, ..)` L6857, `sys_getitimer` L6872 → `get_real_itimer(pid)` L6873, `sys_alarm` L6895 → `set_real_itimer(pid, 0, ..)` L6900. None of these canonicalize. `tty_server::handle()` (L408, the module's IPC entry point structurally parallel to `vfs::handle()`) also does **not** canonicalize `caller_pid` — the only `tgid_of` in the whole file is inside `handle_ioctl` (L623), fixing itself locally.
   - Reachable from any thread (`timer_create`/`setitimer`/`alarm`/`timer_settime` etc. are ordinary syscalls). Worst case: `check_timers(current_pid())` (kernel/src/syscall.rs:794, called on **every** syscall return) only ever checks the table under whichever raw tid happens to be running — if the thread that armed the timer isn't the one making syscalls afterward, the timer's deadline is checked by nobody and the signal never fires.
   - Fix: add `let caller_pid = sched::tgid_of(caller_pid);` at the top of `servers/tty/src/lib.rs::handle()` (before its `match msg.tag`, ~L409), and `let pid = sched::tgid_of(pid);` at the top of `check_timers`, `ensure_real_timer`, `set_real_itimer`, `get_real_itimer` (the four `pub fn`s called directly, not via `handle()`). `handle_ioctl`'s existing local fix becomes redundant (harmless no-op) once `handle()` itself canonicalizes.

2. **`kernel/src/syscall.rs:~4571` — `sys_fcntl`'s stdio `F_GETFL` branch reads `STDIO_FLAGS` with a raw pid.**
   - `fn sys_fcntl` sets `let pid = current_pid();` at its top (~L4530). In the `fd <= 2` arm, `F_SETFL` correctly calls `set_stdio_flags(pid, arg as u32)` (L4573ish) which canonicalizes internally (`let pid = sched::tgid_of(pid);`, L4512-4513) before writing `STDIO_FLAGS`. But the sibling `F_GETFL` arm, a few lines above (~L4571), reads `STDIO_FLAGS.lock().iter().find(|s| s.in_use && s.pid == pid)` with the **raw**, uncanonicalized `pid` — the one asymmetric reader that bypasses the pattern `stdio_nonblocking()` (L4498-4499) already uses correctly for the same table.
   - Reachable: any multithreaded process that sets `O_NONBLOCK` on stdio from one thread and queries it via `fcntl(fd, F_GETFL)` from a different (non-leader) thread gets back `flags=0` (looks blocking) even though the process-wide state says nonblocking.
   - Fix: `let pid = sched::tgid_of(pid);` immediately before the `F_GETFL` lookup at ~L4571 (or hoist one canonicalized `pid` to the top of the `fd <= 2` arm so both branches share it).

3. **`kernel/src/syscall.rs:2773` — `sys_execve` → `vfs::steal_mounted_file(pid, fd)` with a raw pid.**
   - `sys_execve` sets `let pid = current_pid();` at its top (L2749) and never rebinds it before L2773's `match vfs::steal_mounted_file(pid, fd) { ... }`. `steal_mounted_file` (servers/vfs/src/lib.rs:697) calls `find_tbl(pid, &mut *tbls)` directly with no internal `tgid_of` — unlike its neighbors `fd_nonblock`/`fd_redirected`/`vfs_get_node_kind` in the same file, which each do `let pid = sched::tgid_of(pid);` themselves.
   - Proof this is the exact known-fixed-elsewhere pattern, missed at this one call site: **later in the very same function**, `sys_execve` does get it right — L3153 `let fd_owner = sched::tgid_of(pid);` before the cloexec sweep, with a comment explicitly naming this bug class ("execve issued from a non-leader thread... named a pid no table matches and closed nothing at all"). The `steal_mounted_file` call 380 lines earlier in the same function was never updated to match.
   - Reachable: `execve()` from a non-leader thread of a multithreaded process, when the demand-paged fast path is taken (`open_exec_header` succeeds — i.e. the binary is filesystem-backed, not ramfs/initrd). Effect: not a crash — `steal_mounted_file` returns `None`, so `sys_execve` silently falls through to `sys_close(fd)` and then the eager whole-file-read fallback, defeating the demand-paging optimization the surrounding comment describes for exactly this scenario.
   - Fix: `match vfs::steal_mounted_file(sched::tgid_of(pid), fd) { ... }` at L2773 — same shape as the PRIME fix already applied at L5694/L5724.

## CONFIRMED-BUG-IF-ACTIVATED (currently unreachable — dead code)

4. **`servers/tty/src/lib.rs` — `handle_open`/`handle_read`/`handle_write`/`handle_close`/`handle_isatty` read/write `TTY_TABLES` with a raw pid**, omitting the `tgid_of` that `handle_ioctl` (L623) applies to the same table. Confirmed unreachable today: grepped `kernel/src/syscall.rs` and `servers/vfs/src/lib.rs` for `TTY_OPEN`/`TTY_READ`/`TTY_WRITE`/`TTY_CLOSE`/`TTY_ISATTY` — zero hits outside `servers/tty/src/lib.rs` itself, and the file's own header comment says so explicitly ("This TTY-fd path is currently dormant: nothing routes TTY_OPEN"). Not an active bug, but if this path is ever wired up it inherits the identical defect as finding #1. Fix shape whenever activated: `let pid = sched::tgid_of(pid);` at the top of each of the five handlers.

## SUSPECT — same root cause, fixing naively creates a new bug

5. **`servers/tty/src/lib.rs::close_all(pid)`** (called from `kernel/src/syscall.rs::vfs_close_all_for(pid)` L5562) **and `kernel/src/syscall.rs::stdio_flags_close_all(pid)`** (called at L5563) both take a raw pid and have no `tgid_of` of their own. Today this is an accidental cancellation, not a fix: `vfs_close_all_for` is invoked either with the exiting thread's own raw `current_pid()` (from `vfs_close_all_current`) or with a sibling's raw pid (the `EXIT_GROUP` forced-kill loop) — since neither `close_all` nor `stdio_flags_close_all` canonicalizes, a non-leader thread's exit is silently a no-op against these TGID-keyed tables (raw tid never matches the stored TGID), which happens to be the *correct* outcome (a lone worker thread exiting must not tear down process-wide console/timer state its siblings still need) but only by coincidence. **Do not fix this with a bare `tgid_of` insertion** — that would make every thread's ordinary exit correctly *find* the table and then wipe it out from under still-running siblings, a new inverse-class bug. The correct fix mirrors what `vfs_close_all_for` already does for net/epoll cleanup: move both calls inside the existing `if sched::tgid_of(pid) == pid { ... }` block (kernel/src/syscall.rs ~L5556-5560), which already gates that cleanup to thread-group-leader exits only, rather than adding canonicalization to `close_all`/`stdio_flags_close_all` in isolation.

## Noted, not filed as this bug class

- **`sched/src/lib.rs::umask()` (L903) / `heap_end()` (L913)** read/write the *calling thread's own* `Task` record via raw `current_pid()`, but `umask` and the heap-break are POSIX process-wide state. `sched/src/clone.rs` (L190-274, L412-485) **copies** `umask`/`heap_start`/`heap_end` into each new thread's own `Task` at clone time rather than sharing them, so a later `umask()`/`brk()` call by one thread updates only that thread's copy — invisible to siblings. This is architecturally the same *shape* of bug (process-wide state, thread-keyed access) but the one-line `tgid_of` fix doesn't fully solve it by itself: redirecting reads/writes to `find_pid_mut(sched::tgid_of(pid))` would make the thread-group leader's field the single source of truth going forward, but every non-leader thread's own copy (set once at clone) would become permanently stale/unused, which is fine functionally but is a bigger behavioral change than the other findings here and deserves its own decision rather than a mechanical one-liner in this pass. Flagging for awareness, not proposing a specific patch.
- **`sched/src/futex.rs`** — keyed by raw pid **by design** (each thread must wait/wake individually per POSIX futex semantics); confirmed SAFE, not a variant of this bug.
- **`sched/src/signal.rs`** — checked specifically for the inverse class (TGID applied where per-thread data was intended). `sys_sigaction` correctly resolves through the tgid leader (signal dispositions are process-wide); `sys_sigprocmask` correctly stays on the raw per-thread pid (signal mask is per-thread). No inverse-class bug found here.
- **`drivers/src/drm_device_interface.rs::DUMB_BUFFERS`** and **`drivers/src/drm/{auth,device,core}.rs`** — no pid/owner field exists in any of these at all (GEM handles are a single global namespace); out of scope for a raw-TID-vs-TGID bug, though it is a separate (unaudited) question whether DRM handles should be per-client-isolated.
- **`servers/net/src/lib.rs`** — the only public entry point is `handle()`, which canonicalizes; there is no bypass accessor analogous to vfs's `tmpfile_owner_of`/`steal_mounted_file`, so the whole module is SAFE by construction (single choke point).
- **`kernel/src/syscall.rs::EPOLL_INSTANCES`/`EPOLL_FDS`** — every accessor (`epoll_fcntl`, `sys_epoll_close`, `sys_epoll_ctl`, `sys_epoll_wait`) compares `sched::tgid_of(ep[slot].owner_pid) != sched::current_tgid()`, i.e. canonicalizes on both sides of the comparison at the read site itself, so it's SAFE regardless of whether `owner_pid` at rest is raw or canonical. `epoll_close_all(pid)` (L6125) is the one exception — it does a bare `inst.owner_pid == pid` with no `tgid_of` — but its single call site (L5560) is already inside `vfs_close_all_for`'s `if sched::tgid_of(pid) == pid` gate, so `pid` there is guaranteed already-canonical by construction. SAFE, but fragile: it depends on that call-site guard rather than protecting itself, unlike every other accessor in this table.
