# M7z2 — cosmic-greeter EMFILE (Issue 1) + cosmic-idle idle-notify inert (Issue 2)

## ISSUE 1 — greeter EMFILE crash-loop. ROOT CAUSE FOUND (code inspection, ironclad)

The fd table is per-process in `servers/vfs` (MAX_FDS=128, fds 0..127). Sockets live
in a disjoint space (`servers/net`, SOCK_FD_BASE=0x100, MAX_SOCKS=512); epoll fds
higher still. eventfd/timerfd/pipe/**inotify**/files are all vfs fds (0..127).

`sys_inotify_init1(_flags)` (kernel/src/syscall.rs) **discarded the flags** and sent
`VFS_INOTIFY_CREATE` with no args; `handle_inotify_create` (servers/vfs) hardcoded
`flags: 0`. So EVERY inotify fd was non-CLOEXEC regardless of IN_CLOEXEC.

The execve close-on-exec sweep (`handle_exec_cloexec`) only retires fds whose
`flags & O_CLOEXEC != 0`. inotify fds always had flags=0 → **never swept** → they
leak across every execve into every child.

cosmic-config opens an IN_CLOEXEC inotify instance per config-key watcher; a
config-heavy session (cosmic-session + launch_pad spawning ~20 services) accumulates
~one-per-watch. The whole ~125-deep table is fork+inherited into each child; the
sweep closes the real CLOEXEC fds but leaves the inotify ones. Late children
(cosmic-greeter) open their FIRST fd (calloop Ping eventfd / winit) into a full
table → EMFILE (os err 24) → panic → launch_pad infinite restart = crash-loop.

Only inotify drops the flag — audited all `FdEntry {` constructions: open/pipe/dup/
import_fd/eventfd/signalfd/timerfd all persist O_CLOEXEC correctly. inotify was the
lone offender (line 4351, `flags: 0`).

### FIX (working tree)
- kernel/src/syscall.rs `sys_inotify_init1`: forward `flags` in the VFS msg.
- servers/vfs `VFS_INOTIFY_CREATE` dispatch: pass `arg(msg,0) as u32`.
- servers/vfs `handle_inotify_create(pid, flags)`: store `flags & (O_CLOEXEC|O_NONBLOCK_FL)`.

### TEMP instrumentation (to be reverted before final)
- `handle_exec_cloexec` returns an fd-table census in reply.data[8..40].
- kernel prints `[XFDS] pid=.. total=.. swept=.. sweptIno=.. survIno=.. ...` per execve.
  (all values HEX). Confirms magnitude + that inotify fds are now swept.

## ISSUE 2 — cosmic-idle never gets Idled. Mechanism mapped, root cause TBD (needs on-target)

smithay idle_notify (rev efeb597): `GetIdleNotification` → `reinsert_timer` inserts a
plain **calloop Timer::from_duration(timeout)**; on fire it calls `.idled()`. Timer is
NOT inserted iff `is_inhibited && !ignore_inhibitor` (line 206). cosmic-idle uses the
non-input variant (ignore_inhibitor=false).

cosmic-comp `refresh_idle_inhibit` sets is_inhibited = any idle_inhibiting_surface with
a primary scanout output. idle_inhibiting_surfaces populated only by the zwp_idle_inhibit
protocol; an idle desktop has none → is_inhibited should be false → timer SHOULD insert.

calloop 0.14.4 timers are **wheel-based, no timerfd**: `Poll::poll` computes
`timeout = min(user_timeout, next_deadline-now)` and calls `poller.wait(events, timeout)`
(polling crate → epoll). Expired timers fire only when `wait` returns on **timeout-expiry
with no fd ready**. So idle firing depends on our epoll_wait honoring a pure timeout wake.

Two live hypotheses:
  (a) cosmic-comp is_inhibited wrongly true (upstream/config) — timer never inserted.
  (b) our epoll_wait timeout-wake path doesn't fire the timer with no fd ready.
DECISIVE TEST: standalone musl calloop-0.14.4 timer program on-target (4s Timer, print on
fire). Fires → kernel timers OK → cause is (a). Doesn't fire → kernel (b).

## RUN 1 (fix + exec-census instrumentation, aarch64, fresh full-rebuild image)
- greeter now reaches DEEP init (iced_winit, cosmic_greeter::locker/common, font, xkb) —
  far PAST the old "first calloop Ping" EMFILE crash. So greeter no longer crash-loops.
- BUT [XFDS] exec census: **sweptIno=0 / survIno=0 everywhere**; max inherited table = 43/128
  (mostly swept CLOEXEC files + 3 surviving pipes). So inotify fds are NOT inherited — they
  are created POST-exec by each process for its own config watching. => my "inherited inotify
  leak" story is NOT what fills the table; the table fills within a single process's lifetime.
- NEW surfaced blocker: **cosmic-panel** now EMFILEs on the `notify` crate inotify watcher
  ("Error while watching cosmic theme: ... No file descriptors available", os code 24) →
  panics code 101 → launch_pad restarts it → crash-loop. Panel applet area dark (top≈62,62,60).
- So EMFILE is fd-COUNT pressure inside a process that creates many notify::Watcher instances
  (cosmic-config, one per watched config key; each Watcher on Linux = inotify + mio epoll +
  eventfd waker). 128 slots too tight; Linux RLIMIT_NOFILE=1024 accommodates upstream.

## Open causality question (RUN 2 census will settle)
- Is greeter's improvement attributable to the inotify-CLOEXEC fix, or to fresh-image/timing?
  sweptIno=0 argues the CLOEXEC fix is NOT load-bearing for greeter. Need the [EMFILE] live
  census (what kinds fill the 128 table for the crashing pid) + whether it is BOUNDED (~130,
  raise cap fixes) or an UNBOUNDED leak (raise cap only delays). RUN 2 adds fd_emfile_census.

## RUN 2 (census) — DECISIVE for Issue 1 mechanism
- cosmic-panel EMFILE now at calloop **Ping** creation: "Failed to create a Ping.: Os code 24".
  calloop Ping = `pipe_with(CLOEXEC|NONBLOCK)` (rustix pipe2) → our `handle_pipe`. handle_pipe
  returns **-24 ONLY from alloc_fd()** (the ring-pool path returns -23/ENFILE). So the
  per-process **128-slot fd table is genuinely FULL** for cosmic-panel. Confirmed root class:
  fd-COUNT pressure inside one process, NOT inheritance, NOT a global pool (for this hit).
- [EMFILE] vfs-side extern serial print did NOT emit (linkage quirk) → moved census kernel-side
  (VFS_FD_CENSUS msg → kernel prints). RUN 3 pending for composition.
- Nondeterminism: whichever component's fd-creating call tips the table over crashes. In these
  runs cosmic-panel loses the race (crash-loops code 101); greeter wins (survives deep init).
  => greeter's survival vs the prior investigation is most likely the race outcome, NOT proof the
  inotify-CLOEXEC fix is load-bearing. sweptIno=0/survIno=0 everywhere confirms inotify fds are
  NOT inherited. The inotify-CLOEXEC fix is a REAL latent correctness bug (IN_CLOEXEC dropped)
  but is NOT the primary lever for the EMFILE symptom. Keep it (correct + prevents deep-tree
  leaks) + add a regression test; the PRIMARY fix is raising MAX_FDS.

## Planned fix (Issue 1)
- Raise per-process MAX_FDS 128 → 256 (task-sanctioned). Kernel per-thread stack = 1MB
  (init.rs:377), so the [_;MAX_FDS] stack copies in fork_dup/exec_cloexec/close_all (~10KB at
  256) are safe. RUN 3 census sizes the need (panel peak) + checks global pools (MAX_TIMERFDS=64,
  MAX_EPOLL_INSTANCES=64) aren't a secondary wall for a full session.

## RUN 3 (kernel-side census) — TRUE ROOT CAUSE (Issue 1)
`[EMFILE] pid=0x27 who=eventfd total=15 ino=3 ev=8 tm=1 pipe=3 sig=0 file=0 other=0`
The process held only **15 fds** yet eventfd() returned EMFILE → the per-process table is
NOT the limit. handle_eventfd returns -24 first from the **global EVENTFD_COUNTERS pool
(MAX_EVENTFDS=64)** being exhausted. => TRUE ROOT CAUSE: **global, session-wide fd-object
pools (eventfd/timerfd/epoll-instance), all sized 64, are too small for a full ~20-process
COSMIC session.** The `polling` backend (calloop 0.14) couples 1 epoll + 1 eventfd waker +
1 timerfd PER event loop; every component runs several (tokio + calloop + one per
notify::Watcher), so ~20 procs × ~6-8 ⇒ ~150 of each, far over 64. Whichever component's
event-loop creation loses the race when the pool hits 64 crash-loops (cosmic-panel here,
cosmic-greeter in the prior investigation — same class, nondeterministic victim).
My earlier "per-process 128 table full" read was WRONG (the pipe -24 came from a different
pid / calloop-0.13 eventfd-Ping, not a full table).

## FIX (Issue 1) — raise the global pools
- vfs MAX_EVENTFDS 64→256, MAX_TIMERFDS 64→256 (cheap arrays).
- kernel MAX_EPOLL_INSTANCES 64→256 (~4 MB zero-init .bss, NOBITS), MAX_EPOLL_FDS 128→512
  (EPOLL range [0x400,0x600) < TTY 0x1000, no collision).
- MAX_FDS stays 128 (per-process table never the wall — peak seen ~43 inherited, 15 live).
- inotify IN_CLOEXEC fix KEPT (real latent correctness bug, not load-bearing for EMFILE).

## RUN 4 (pool raise) — ISSUE 1 FIXED & VERIFIED (aarch64)
- top=(51,214,200) teal leandros-applet block + center=(190,142,130) Orion nebula, stable
  t=55..140 (pixel-identical) = the M7w known-good desktop. Before fix top was (62,62,60) dark
  (panel crash-looping). cosmic-panel: EGL + "GL Renderer softpipe" + applet "committed 220x32".
- 0 [EMFILE], 0 panel code-101, 0 "Failed to create a Ping", greeter alive (x20, no failures).

## RUN 5 (idle on healthy session) — ISSUE 2 RESOLVED by the Issue 1 fix
- Screen progressively DIMS uniformly: top 51,214,200 → 46,192,179 → 40,170,159 (×0.79 = a
  rising-alpha black overlay = cosmic-idle's fade). center dims in lockstep.
- DEFINITIVE: cosmic-idle logged `command 'loginctl lock-session' failed` — that runs ONLY from
  fade_done() → which runs ONLY after the Idled event fired and the 5 s fade completed. So the
  full chain fired: comp delivers **Idled** → cosmic-idle fade → output_power DPMS Off → lock.
  This NEVER happened on the degraded session. => Issue 2 was a CONSEQUENCE of Issue 1: cosmic-idle
  is a LATE-started process; under global fd-pool starvation its calloop/wayland loop couldn't
  function (arm timerfds/eventfds, drive frame callbacks) so it never acted on Idled. Healthy
  pools ⇒ the ext-idle-notify path works end-to-end. NOT is_inhibited, NOT a kernel timer bug.
- RUN 6 (m7z2_idleclean): clean two-phase confirm (continuous input=BRIGHT, no input=FADE).

## CORRECTION (Issue 2) — NOT resolved by Issue 1; a separate KERNEL timerfd-wake bug
RUN 6 (idleclean): input phase stayed BRIGHT (good), but NO fade after input stopped.
RUN 7 (m7z2_idlenoinput, ZERO injected input, 195 s): screen NEVER faded; no lock in serial.
RUN 5 (faded) had ONE input at t=64 → faded ~t=90. So the fade fires ONLY when some fd
event (input → wayland/cursor traffic) wakes cosmic-comp's loop, which then processes the
already-overdue idle timer. With NO fd activity the idle timerfd expiry does NOT wake the
blocked epoll_wait(-1). => TRUE Issue 2 root cause: **a blocked `epoll_wait(-1)` is not woken
when a `polling`-armed timerfd expires** (fd activity wakes it; timerfd expiry alone does not).
This is why cosmic-comp never delivers ext-idle-notify `Idled` on a truly idle desktop.
(My earlier "resolved by Issue 1" note was wrong — run 5 faded via input-driven wakes.)

Mechanism recap: calloop 0.14 → polling 3.11 arms a relative one-shot timerfd + registers it
EPOLLONESHOT + calls epoll_wait(timeout=None). Code path SHOULD work: sys_timerfd_settime
parses it_value correctly (offset 16); handle_timerfd_settime sets armed+deadline;
poll_deadline_tick(100 Hz) reads earliest_timerfd_deadline() and calls service_poll_deadlines
with timerfd_due → wake_due_poll_deadlines wakes ALL is_poll waiters (runqueue.rs:217);
epoll_ctl MOD re-arms EPOLLONESHOT (syscall.rs:6462). Yet empirically it doesn't wake.
RUN 8 diagnostic added: poll_deadline_tick logs [TDIAG now/tfd/nextpoll] once/sec to see
whether cosmic-comp's ~5 s idle timerfd is armed+visible during idle.

## RUN 8 [TDIAG] result — Issue 2 mechanism pinned to a scheduler-wake gap
poll_deadline_tick logged, once/sec: from t≈22 s an armed timerfd deadline `tfd=0x8DB`
appears and **stays stuck** while `now` advances 3+ s past it (an expired one-shot timerfd
never disarmed — earliest_timerfd_deadline() does a read-only scan, never consuming it). So
poll_deadline_tick computes `timerfd_due=true` EVERY tick and calls service_poll_deadlines,
yet cosmic-comp's idle never fires. => the timerfd-deadline wake is NOT reaching the idle
compositor's blocked wait. Asymmetry: fd-edge wakes go wake_poll→unblock_port (`.lock()`,
syscall ctx = reliable); timerfd-deadline wakes go poll_deadline_tick→service_poll_deadlines
(`RUN_QUEUE.try_lock()`, 100 Hz IRQ ctx = **droppable under contention**). The stuck-timerfd
storm itself creates the contention that keeps dropping the wake. That is why input (fd edge)
makes the overdue idle fire but pure idle never does. This is a delicate real-time
scheduler/locking fix (make IRQ-ctx poll-deadline wakes reliable, e.g. a pending-wake flag
serviced on the next RUN_QUEUE unlock; and/or stop earliest_timerfd_deadline from perpetually
re-reporting an already-expired one-shot). NOT shipped: unverified scheduler changes are too
risky under the both-arch-verify discipline; needs a per-pid epoll_wait trace to land safely.

## FINALIZATION
- Instrumentation fully reverted (XFDS/EMFILE census/TDIAG). Real diff = 4 pool constants +
  inotify IN_CLOEXEC honoring. mkfs-f2fs-populated.py change is PRE-EXISTING (idle/greeter
  staging from the prior investigation), not mine.
- Screenshots (PNG): m7z2-aarch64-poolfix-t140.png (Issue 1: panel+teal applet+nebula+cursor),
  m7z-aarch64-healthyidle-t125.png (Issue 2 fade: dimmed, cursor hidden).

## REGRESSIONS (both arches, fresh-image kernel changes)
aarch64 (FRESH image): vfstest 36/0, scmtest 25/0, wakepolltest 10/0, forktest 3/0,
  epolltest 8/0, polltest 6/0, sigtest 6/0, timertest 5/0, memtest 4/0; waittest 3/2 =
  the KNOWN wait_on_process_group flake (documented open issue; x86_64 waittest 5/0 confirms).
x86_64 (existing/dirty image): scmtest 25/0, wakepolltest 10/0, forktest 3/0, epolltest 8/0,
  polltest 6/0, waittest 5/0, sigtest 6/0, timertest 5/0, memtest 4/0; vfstest 35/1 =
  xattr_list_f2fs (dirty-image artifact, orthogonal to fd-pool/inotify change; aarch64 fresh=36/0).
=> pool-relevant tests (wakepoll/epoll/timer/scm) GREEN both arches. No regressions.

## ISSUE 2 — DEEP per-pid wake-path DIAGNOSTIC + partial fix (coordinator green-light)
Added kernel diag (spdOK/spdFAIL for service_poll_deadlines try_lock; pollblk/woken from
wake_due_poll_deadlines; earliest-timerfd owner pid + block-state). Pure-idle (zero input) steady
state: `tfd=0x8BB slot=0 owner=0x12 STUCK` (deadline ~20-30 s in the past, never disarmed),
`pollblk=woken=0x2D` EVERY tick (45-task wake STORM 100×/s), `spdFAIL` climbing ~8-11% (try_lock
drops under the storm's contention), `ownerblk=0` (owner pid 0x12 = earliest proc = cosmic-comp is
Running/Ready, NOT blocked — STARVED). ROOT of the storm: an armed one-shot timerfd whose owner
doesn't promptly consume it stays `armed` with a past deadline forever; earliest_timerfd_deadline
(read-only) reports it every tick ⇒ timerfd_due fires every tick ⇒ mass-wake ⇒ CPU saturation ⇒
starves the process that must run to consume it. Self-reinforcing.

FIX A (KEPT, regression-safe): `vfs::fold_expired_timerfds(now)` retires each expiry ONCE in the
100 Hz tick (disarm one-shots / advance periodics, preserving the expiration count); drive
timerfd_due from "expired on THIS pass" not "stale deadline still ≤ now". IRQ-safe (try_lock,
no user mem). Kills the storm. Regressions aarch64 ALL GREEN (timertest 5/0, wakepoll 10/0,
epoll 8/0, poll 6/0; waittest flake). Session renders healthy with it.

Pure-idle fade STILL doesn't fire with Fix A → remaining cause is ABOVE the kernel. run-5 (one
input) DID fade via the SAME kernel timerfd path, so the kernel wake works; pure-idle fails
because the idle notification's calloop Timer is never inserted without a notify_activity(input)
event: smithay reinsert_timer skips when is_inhibited; cosmic-comp recomputes is_inhibited only in
Common::refresh()→refresh_idle_inhibit() (runs on repaint/activity); our STATIC leandros-applet
(no clock, no periodic frames) means a truly idle desktop never repaints → never refreshes →
is_inhibited stays stale → timer uninserted. Correct-on-Linux (live clock applet + cursor blink
drive periodic refreshes). Can't patch COSMIC. PROPOSED OUR-SIDE mitigation (unconfirmed): make
leandros-applet commit ~1 frame/s via frame callbacks → cosmic-comp repaints → refresh →
is_inhibited recomputed → idle arms → fade fires.

## FINAL STATUS
- [x] Issue 1: fd-object pool exhaustion — FIXED (4 pool consts) + VERIFIED + regressions both arches + PNG
- [x] inotify IN_CLOEXEC dropped-flag — FIXED (correctness)
- [x] Issue 2 STORM (expired one-shot timerfd → perpetual timerfd_due wake storm) — FIXED (fold), regression-safe both arches
- [~] Issue 2 pure-idle FADE — NOT fired; root cause above the kernel (cosmic-comp is_inhibited not
      refreshed in a static idle session); kernel timerfd wake proven working (input fires it);
      mitigation is OUR-side applet (proposed) or upstream — documented, not landed
- [ ] inotify-CLOEXEC userland regression test — recommended follow-up (needs userland+mkfs rebuild)
- Diff: kernel/src/syscall.rs, servers/vfs/src/lib.rs (pools + inotify + fold), sched/* reverted to clean.
