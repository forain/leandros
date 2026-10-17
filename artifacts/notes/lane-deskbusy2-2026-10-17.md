# lane/deskbusy2 — 2026-10-17

## Question

Laptop (i5-8350U, i915, COSMIC as leandro), Firefox playing YouTube on the
session bus: desktop processes that idle at 1–2 % burn CPU (panel ~60 %, busd
~40 %, session ~30 %, ~555 % total), and the lane/mediabus kernel (exact poll
keys) did not help on the laptop although it halved the same load in QEMU.
Is it real D-Bus/Wayland work or kernel overhead charged as utime?

## Answer: kernel time — spurious poll wakes, each a full re-probe, under a contended RUN_QUEUE

Measured with a new exact user/system split (see Instrumentation), YouTube
playing, 13.1 s window, kernel cd15e251 (same behaviour as main-73fbe8e1):

| process | total % | user | sys |
|---|---:|---:|---:|
| cosmic-panel | 62.0 | 0.4 | 61.6 |
| busd | 34.7 | 0.0 | 34.7 |
| cosmic-session | 30.0 | 0.0 | 30.0 |
| cosmic-comp | 68.0 | 9.2 | 58.9 |
| cosmic-settings(-daemon) | 14.9 | 0.0 | 14.9 |
| cosmic-workspaces | 14.6 | 0.0 | 14.6 |
| cosmic-files-applet | 10.5 | 0.0 | 10.5 |
| pipewire / -pulse | 10.5 / 9.7 | 0 / 0.5 | 10.5 / 9.2 |
| firefox (main) | 79.5 | 26.3 | 53.3 |

- The desktop's own code does essentially nothing (0.0–0.4 % user). It is the
  kernel servicing `epoll_pwait`: cosmic-panel made **8 epoll_pwait calls/s
  and spent 233 ms/s of kernel CPU in them (~29 ms per call)**; busd and
  cosmic-session made **zero** syscall returns in 13.9 s while burning 33 % and
  20 %. They never leave epoll_wait: woken → re-probe every interest (EPOLL,
  FD_TABLE, server round trips) → nothing ready → park → woken again.
- `sched.wakestat=1` outcome census: cosmic-workspaces 0 useful / 2464
  spurious wakes/s, files-applet 0 / 1638, panel 4 / 1368, each applet
  600–1100, sshd 9 / 796. ≈ 300 µs of kernel CPU per spurious wake
  (panel 233 ms / 725, sshd 247 ms / 898 on e501b297).
- Firefox SIGSTOPped (old kernel): total 527 % → 69 %, panel 64 → 7, busd
  45 → 9, session 30 → 4: the load is Firefox-driven, and it is wakes, not
  messages.

Why the keyed refinement (lane mediabus) did not help on the laptop: a typical
iced/tokio thread parks with only 3–7 bloom bits, but 2 of them came from
**unkeyed** interests (its epoll's own tag, a timerfd, inotify, signalfd, a
device fd). An unkeyed bit is never refined, so every keyed AF_UNIX/pipe/
eventfd wake landing on one of those bits still woke the thread:
**16.6 k unrefined wakes/s** (vs 38 k refined skips). On top came bit-only
and broadcast sources: GPU completion wakes (~14 tasks per interrupt), timerfd
expiries, signal posts (a broadcast to ~180 parked tasks each), the deferred
tick wake (contended IRQ try-wakes fall back to bit-only), the net daemon's
kick and per-socket inet wakes.

## Fix (kernel)

e501b297:
- **Foreign poll tags** (`PollTag::foreign`, `POLL_REPLY_FOREIGN` 0x4c):
  objects never targeted by a keyed wake — signalfd, pty, pidfd, inotify,
  DevVt, device-server fds, AF_UNIX listeners, an epoll instance's own tag —
  join the mask without poisoning refinement (`PollSet::add_foreign_bits`).
  Sound because keyed wakes exist only for pipes, eventfds, timerfds, AF_UNIX
  data edges, inet sockets and the two fixed keys below, whose probes always
  carry their keys; a foreign object is woken only by bit-only wakes, which
  ignore refinement. Kinds are whitelisted; anything new stays unrefined.
- **Timerfds carry exact keys**; the deadline tick's expiry wake is keyed
  (`WakeKeys`, ≤16 per pass, else bit-only).
- **GPU syncobj wake keyed + exclusive** (`POLL_KEY_SYNCOBJ`); waiters park
  with the bare bit (never refined), so they are always reached.
- **Park mirror** in `RunQueue` (`pk_port`, `pk_mask`, `pk_deadline`): wake
  scans and the deadline tick filter parked slots without dereferencing each
  boxed Task. Stale only towards "still parked" ⇒ only skips non-matches.

3fdd0300:
- **Signal posts are tagged, not broadcast**: `signal_wake_tag(tgid)` =
  `poll_tag(SIGNALFD, tgid)`; sigsuspend/sigtimedwait park with it (they used
  mask 0 and relied on the broadcast). The thread a signal is queued on is
  still made Ready directly; group kills reap siblings directly.
- **Deferred wakes keep exact keys** (`request_poll_wake_key`, 32-slot
  lock-free table, bit fallback when full): pipe/eventfd fallbacks, syncobj,
  the net daemon's IRQ kick, the timerfd tick fallback.
- **Net daemon kick keyed** (`NETD_KEY`); **inet sockets keyed**
  (`inet_key`; the daemon's per-pass readiness wake is keyed, ≤16 per pass).

## Instrumentation (kept)

- `/proc/<pid>/stat` utime/stime and getrusage: **stime = on-CPU time inside
  syscalls** (`sched::sysacct`: per-CPU syscall clock carried across context
  switches in `Task::sc_saved_*`); utime = the rest (page faults, IRQs from
  user mode, and kernel threads' time count as utime).
- `/proc/sysstat`: `t pid tgid cpu_ns sys_ns` per thread, `s tgid nr calls ns
  zero` per (thread group, syscall) — calls, on-CPU kernel ns, returns of 0.
- `/proc/rqhold`: RUN_QUEUE holds per acquire site (`file:line holds ns_held
  max_ns ns_spun`), always on.
- `/proc/pcsample`: the tick PC sampler, now always on and readable. Kernel
  time is under-sampled (syscalls run with IRQs masked): use sysstat for the
  split.

## Laptop before/after (YouTube playing, % of one CPU, utime+stime, ~13 s)

| process | main-73fbe8e1 | cd15e251 (instr.) | e501b297 | 3fdd0300 |
|---|---:|---:|---:|---:|
| **total** | 527 | 555 | 510 | **375** |
| cosmic-panel | 64 | 62 | 40 | **27** |
| busd | 45 | 35 | 33 | 30 |
| cosmic-session | 30 | 30 | 20 | **10** |
| cosmic-settings | 14 | 15 | 10 | **2.9** |
| cosmic-workspaces | 12 | 15 | 5.4 | 4.9 |
| cosmic-files-applet | 11 | 10.5 | 5.4 | 2.6 |
| cosmic-osd | 12 | 6 | 6.3 | 5.2 |
| pipewire / -pulse | 8.7 / 8.4 | 10.5 / 9.7 | 8.2 / 9.8 | < 1.6 each |
| cosmic-comp | 76 | 68 | 56 | 68 |
| firefox (two biggest) | 64 + 39 | 80 + 54 | 97 + 78 | 81 + 34 |
| sshd (incl. the sampling session) | 11 | 14 | 25 | 30 |
| RUN_QUEUE acq/s | 303 k | 382 k | 358 k | **195 k** |
| RUN_QUEUE spin ms/s | 525 | 661 | 816 | 859 |
| EPOLL acq/s | 551 k | 592 k | 440 k | **242 k** |
| FD_TABLE acq/s | 759 k | 882 k | 676 k | **333 k** |
| unrefined keyed wakes/s | 16.9 k | 16.6 k | 7.5 k | **4.7 k** |

Firefox's own share varies with playback phase; compare the desktop rows.
Idle reference (no Firefox, cd15e251): total 40 %, panel 5.5, comp 4.8, busd
3.6 — also all sys.

## What is left, quantified (3fdd0300, /proc/rqhold, 13.3 s)

RUN_QUEUE: 335 ms/s held, **1126 ms/s spun** (several CPUs waiting at once).

| site | holds/s | hold ms/s | avg hold | spin ms/s |
|---|---:|---:|---:|---:|
| scheduler pick/dispatch (`lib.rs` run loop) | 40.7 k | 105 | 2.6 µs | **325** |
| `has_deliverable_signal` (`interrupted()`, signal.rs:109) | 45.7 k | 11 | 0.24 µs | **279** |
| syscall-return signal claim (signal.rs:261) | 52.5 k | 12 | 0.22 µs | **192** |
| syscall-return delivery loop (signal.rs:325) | 52.5 k | 6 | 0.12 µs | 22 |
| switch-back CPU charge | 22.5 k | 16 | 0.7 µs | 99 |
| deadline tick service | 15.3 k | 63 | 4.1 µs | 0 (try-lock) |
| keyed wake scans (net/vfs/drm sites) | ~4 k | ~45 | **10–12 µs** | ~40 |
| park prepare (plain + keyed) | 16.8 k | 6 | 0.24–0.46 µs | 71 |
| /proc/<pid>/stat CPU sum (sampler) | 183 | 42 | 228 µs | 2 |

- Every syscall return takes RUN_QUEUE twice (signal claim + delivery loop)
  and every `interrupted()` once more: ~150 k of the 195 k acquisitions/s and
  ~490 ms/s of the spin are signal bookkeeping on paths that find nothing.
- A Task dereference under the lock costs ~380 ns (the 600-thread stat walk:
  228 µs), which is why the park mirror mattered and why the keyed wake scan
  still costs ~11 µs (it dereferences every mask-matching task to check its
  fingerprints).
- Per spurious wake, ~40–50 µs is RUN_QUEUE wait (pick, charge, park,
  `interrupted()` ×2 at ~6–8 µs average spin each); the rest of the ~300 µs is
  the O(N) re-probe of every interest (cosmic-comp: 613 µs of kernel CPU per
  epoll_pwait with ~41 interests; sshd, broadcast-mask: 18.7 ms per call).

## Recommendation (next lanes, in order of payoff)

1. **Lock-free signal fast path** on syscall return and in `interrupted()`:
   a per-task "signal/stop/exit state changed" hint, an atomic set under
   RUN_QUEUE by every writer of signal_pending / shared_signal_pending /
   stop_pending / group_exit_owner / saved_sigmask, published per CPU at
   dispatch; read without the lock; cleared only under the lock when the
   locked check finds nothing. Removes ~150 k acquisitions/s and ~0.5 CPU of
   spin. Small and local, so do it first.
2. **epoll ready list instead of re-probing everything** (Linux's ep_poll
   callback model): a keyed wake already names the object, so mark the
   matching interests ready (key → interest index per epoll instance) and
   have `epoll_wait` probe only marked interests, plus a full probe only on
   unkeyed or broadcast wakes. This attacks the remaining ~300 µs per wake,
   which is the bulk of comp/busd/panel/sshd sys time. Per-object wait
   queues are the general form; the ready list gets most of the benefit
   without changing the scheduler.
3. **Per-CPU runqueues** (pick/dispatch/charge: ~0.43 CPU of spin plus the
   holds). This is the structural fix for the pick path, but it is a large
   change; 1 and 2 cut the acquisition rate first and may make it
   unnecessary at desktop scale.
4. Smaller items:
   - Mirror `kbits` plus a 64-bit fingerprint bloom per slot, so refined
     keyed wakes skip without a Task dereference (wake scan ~11 µs → ~1 µs).
   - Run the deadline-tick service on one CPU, not on every CPU's tick
     (15 k/s × 4 µs).
   - Give the inet listener a tag: sshd parks broadcast-mask and is woken by
     everything (~800/s).
   - Replace the net daemon's 100 ms broadcast safety net, ~190 tasks per
     broadcast (~1.2–1.8 k spurious/s while streaming).

## Refuted premises

- "A D-Bus/MPRIS/notification flood makes busd and the panel work": busd
  0.0 % user, panel 0.4 % user; busd made no syscall returns in 13.9 s.
- "utime is user-space work": it was all CPU time (stime was 0); 95–100 % of
  the desktop's time is in syscalls.
- "mediabus' exact keys should have fixed the laptop": its QEMU win did not
  transfer because laptop threads park with unkeyed epoll-self/timerfd/inotify
  bits that poisoned the refinement (2 of ~3 bits per thread).

## Tests (release)

- regress x86_64 PASS (all 26 suites) on e501b297 and 3fdd0300; aarch64
  PASS (all suites) on 3fdd0300. epolltest 15/15 on both arches, including
  the new cases:
  - `timerfd_set_not_woken_by_unix_traffic`: a waiter on 40 timerfds, an
    inotify fd and a quit eventfd; 600 unrelated AF_UNIX wake calls must
    wake it less than 1/8 of the time. Measured −1 / −3 extra wakes over the
    control.
  - `own_timerfd_among_many_wakes`: one armed timerfd among 40 still wakes
    its waiter, on time: 2/2 hits, ≤ 54 ms for a 50 ms timer.
- The laptop booted e501b297 and 3fdd0300 as trials; a COSMIC session and
  Firefox/YouTube (sound, GPU) ran on both. Neither is promoted.

## Laptop

Trial 0ef84c9e642e = `#1 deskbusy2-3fdd0300` (extra cmdline
`sched.wakestat=1`, for the census only — drop it for a promote). Good entry
unchanged (1f603f52052d, main-73fbe8e1).
