# lane/epollready — 2026-10-17/18

Branch `lane/epollready` off local main d2f0eaee (Merge 'lane/wifiht'),
continued as `lane/epollready2` (from d74b7401). Not pushed.

Status 2026-10-18: DONE in QEMU — regress 26/26 + net-e2e 5/5 + epolltest
24/24 (both modes) on aarch64 AND x86_64; COSMIC + Firefox desktop A/B:
kernel CPU halved, epoll kernel time −61 %. Two regressions of the first
cut found by the A/B and fixed (listener filter drift, MOD coverage), plus
three no-wake producers. Laptop A/B not done (see Open).

Commits on lane/epollready2: a5648753 (evdev/pty/signalfd wakes), ef823a90
(never widen a listener filter), 9429a5ba (epolltest guard), 90ded6fd
(MOD re-probe when the filter lacks the entry), 110d5401 (VT switch wake
broadcast, out from under DRAW_LOCK), then this note.

## Problem

On the laptop during YouTube, desktop threads sit in epoll_pwait; every wake
re-probed EVERY registration (VFS_POLL / NET_POLL round trip each), twice per
park (the probe pass, then `epoll_any_ready` between prepare and commit), and
every call started with a full pass. ~300 µs kernel CPU per useless wake;
cosmic-comp 613 µs per epoll_pwait over ~41 fds (lane deskbusy2).

## Design (commit 8fe32140)

Like Linux's ready list, a wake re-probes only what it can concern.

- **Poll-wake listener** (`sched::PollListen` in `Task::listen`, RunQueue
  `listening` bitmap + `ls_mask` mirror). From a thread's first epoll_wait on
  an instance until it parks on something else, the wake scans
  (`unblock_port_tagged_inner`, `wake_due_poll_deadlines_keyed` for timerfd
  expiries; `widen_port_masks` only for a PARKED listener, see invariant 6)
  record every wake that passes its filter — the
  same mask / kbits / fingerprint rule as a parked poller — into a per-task
  `WakeRing` (8 exact keys; overflow and bit-only wakes OR into `tags`;
  `WAKE_DEADLINE` flag), whether the thread is parked, running, in user
  space, or blocked elsewhere. API in sched/src/lib.rs: `poll_listen_begin`
  (None = record-everything filter), `poll_listen_update` (install set, take
  ring), `poll_listen_take`, `poll_listen_park` (parks ONLY if the ring is
  empty, same RUN_QUEUE hold: Taken/Parked/Off), `poll_listen_end`,
  `poll_listen_enter`/`poll_listen_store` (take/put `Task::ep_cache` + ring).
  `block_on_port_until_keyed` (poll/select/legacy parks) ENDS the listener,
  because it overwrites `poll_fps`, which the listener filter shares.
- **Per-thread snapshot** `EpCache` (kernel/src/syscall.rs, stored as
  `Box<dyn Any>` in `Task::ep_cache`): per registration index its fd/ofd/mseq,
  armed, last probe tag (`PollTag`), `ready` (ready at last probe, or never
  probed). Valid while same slot, same instance `id`, same listener id.
- **`epoll_wait_ready`** (syscall.rs, selected in `epoll_wait_until` unless
  `epoll.ready=0` on the cmdline → `EPOLL_READY`): per pass
  DIFF instance vs snapshot (new/changed fd/ofd → unknown tag; `mseq` changed
  = ADD/MOD → candidate, old tag kept provisionally) → CANDIDATES = all if
  reason unknown; else unknown-tag, ALL-tag and nested entries (always),
  ready-list entries (first pass of a cached call), and `ep_hit` (key equal to
  one of the entry's keys, or the key's bit among the entry's UNKEYED bits;
  bit-only tags intersecting the entry's bits) → PROBE candidates with the
  old loop's exact fire/ET/ONESHOT/POLLNVAL rules → FILTER: if any tag
  changed (or the listener is still ALL), rebuild the PollSet and
  `poll_listen_update`; entries whose tag changed under a narrow filter are
  re-probed once more (`recheck`; after 2 rechecks probe under an ALL filter
  — `churn` guard) → return / park via `poll_listen_park`, EPOLL_CTL_GEN
  re-checked after Parked (as before). After a real sleep: ring empty ⇒
  reason unknown ⇒ full re-probe (counted in `EP_UNKNOWN`).
- Cache miss (first call, other instance, poll()/select() in between):
  `poll_listen_begin(None)` (record everything) BEFORE the full pass, so no
  second full pass is needed; then the real filter is installed.
- `EpollInterest::mseq`, `EpollInstance::id` from `EPOLL_SEQ`.
- `/proc/wakestat`: `# epoll calls N miss N probes N unknown N parks N`.

### Invariants (a lost wake is a hang)

1. While a listener is on, every poll wake matching its filter is in its ring
   (or its bits are). Readers take the ring under RUN_QUEUE; park only if
   empty in the same hold.
2. An entry is probed under a filter that covered it, or is re-probed after
   the new filter is installed (recheck), or was probed under ALL.
3. Unmatched keys are ignored — sound under the existing mediabus/deskbusy2
   rule that a keyed wake's key equals the key the object's probe reports, or
   the probe reports that bit without a key (then `ep_hit` catches it), or the
   object is foreign (never keyed-woken). Bit-only wakes select by bits (the
   tag invariant: producer bit ∈ probe bits for the same object).
4. Events written + committed (last_seq/ONESHOT) in a pass are always
   returned (the listener-lost path no longer `continue`s when n > 0).
5. Readiness that changes with NO wake at all used to be rescued by unrelated
   full re-probes; now only by the ready list / next unknown wake. Any such
   latent producer bug surfaces as a stall. Watch for it.

6. (lane epollready2) A listener's FILTER is never widened by epoll_ctl.
   `widen_poll_masks` widens park masks only (one sleep), and notes the added
   bits in a parked listener's ring. A new or re-armed registration reaches
   the filter through the per-pass diff: an ADD is unprobed (`known` false);
   a MOD of an entry the current filter does not hold (`EpEnt::filt` false,
   e.g. a ONESHOT that fired and was left out of a rebuild) is re-probed as
   new, so the filter is rebuilt with it and it is rechecked under it.
   EPOLL_CTL_GEN (bumped before the widen) covers a listener mid-pass.

## Results (lane/epollready2, code at 110d5401)

All on the Mac: aarch64 = HVF, x86_64 = TCG. Release builds.

| run | aarch64 | x86_64 |
|---|---|---|
| `--suite regress` | 26/26 PASS (reg-a3) | 26/26 PASS (reg-x3) |
| `--suite net-e2e` | 5/5 PASS (net-a3) | 5/5 PASS (net-x3) |
| `epolltest`, ready list | 24/24 (ept-a4) | 24/24 (ept-x3) |
| `epolltest`, `epoll.ready=0` | 24/24 (ept-aL3) | 24/24 (ept-xL3) |
| COSMIC + Firefox session (anim page) | renders, Firefox up, no stall (ws-new3) | not run (TCG) |

`epolltest bench` (kernel ns from /proc/sysstat; the host was loaded by a
sibling lane's builds for the later runs, so compare within a row):

| bench | aarch64 legacy | aarch64 ready | x86_64 legacy | x86_64 ready |
|---|---:|---:|---:|---:|
| roundtrip, kernel ns/call (41 fds, one ready) | 51 355 | **9 566** | 249 185 | **63 184** |
| spurious, kernel ns per unrelated push (broadcast waiter) | 239 211 | **18 304** | 1 023 464 | **195 998** |

(First quiet-host aarch64 run: 44 566 → 6 633 and 68 791 → 5 354.)

### Desktop A/B (aarch64 HVF, virgl, COSMIC + Firefox on a CSS animation)

`ffsession.py aarch64 ws-<m> --put <S>/anim.html:/tmp/anim.html --url
file:///tmp/anim.html --wait 30 --post "cat /proc/sysstat; sleep 12; cat
/proc/sysstat; cat /proc/wakestat"`, images with `sched.wakestat=1` (legacy:
plus `epoll.ready=0`). 12 s window, all processes:

| | legacy | ready list (final) |
|---|---:|---:|
| kernel (in-syscall) CPU, all tgids | 8 723 ms | **4 235 ms** |
| epoll_pwait kernel time | 2 911 ms (11 370 calls, 256 µs/call) | **1 129 ms** (11 245 calls, 100 µs/call) |
| cosmic-comp epoll | 1 660 ms / 8 440 calls (197 µs) | **950 ms / 8 396 (113 µs)** |
| cosmic-panel epoll | 561 ms / 102 calls | **82 ms / 97** |
| busd kernel CPU | 923 ms | **61 ms** |
| tasks woken by the AF_UNIX recv site (net:5129, since boot) | 154 736 | **70 952** |

The legacy run is from the build before the evdev/vt/widen fixes (its code
path is unaffected by them). Analysis scripts: `<S>/ab.py`, `join.py`,
`top.py` (S = scratch dir below).

### Found by the A/B: listener filters drifted to broadcast (fixed, ef823a90)

The first desktop run with 8fe32140 was WORSE than legacy: 9.3 s kernel CPU
vs 2.6 s; idle COSMIC clients (applets, app-library, workspaces, cosmic-bg,
wireplumber) 0.1-1 s each in a single long epoll_pwait; wakestat "woken"
10x legacy, `allmask` (broadcast-mask tasks woken) 671 k vs 25 k, 4.8 M
listener parks for 156 k calls. Cause: `widen_port_masks` ORed every
not-ready ADD's tag into every LISTENER whose filter held the instance's
self-tag bit, and the filter is only rebuilt when an answer changes. Self
tags are bloom bits shared across instances; relibc's poll() ADDs every fd
on every call; one ADD of a broadcast-tagged fd (TCP listener, untagged
server, ...) anywhere made colliding listeners `POLL_TAG_ALL` for good, i.e.
re-probing everything on every wake in the system. Fix: never widen a
listener's filter (see invariant 6). That exposed a second reliance on the
widening — a MOD re-arming a fired ONESHOT kept a "known" tag the filter did
not hold (epolltest oneshot_rearm_warm hung) — fixed by `EpEnt::filt`
(90ded6fd). After both: 0.43 M parks, `allmask`/`unrefined` back at legacy
levels, numbers above. Guard: epolltest `foreign_add_does_not_widen` (its
own epoll kernel time while 400 unrelated pushes flow, after 32 foreign
instances ADD a listening TCP socket): negative control on the unfixed
kernel 4.16 ms → FAIL; fixed 33 µs (HVF) / 377 µs (TCG) → PASS.

## No-wake producer audit (2026-10-18)

The ready list re-probes only what a recorded wake names, so a readiness
change with no wake (or a wake whose tag/key differs from the probe's) that
legacy full re-probes used to rescue would now stall. Audited every
pollable class (VFS_POLL, NET_POLL, tty/pty/vt, evdev, DRM, netctl, timerfd,
signalfd, pidfd, inotify, nested epoll) including the exit/close-all paths
(`vfs_close_all_for` → `release_vnode`; net `handle_close_all` broadcast on
peer hang-up) and the deferred-wake paths (`service_deferred_poll_wake`
re-arms on contention; evdev `WAKE_PENDING` flushed every tick; the net
daemon re-checks `kicked()`).

OK (producer tag = probe tag, or the probe reports broadcast / the bit
unkeyed): pipes (keyed `pipe_key`, incl. writer close and exit), eventfd
(keyed, try→deferred), timerfd (settime with a past deadline → bit; tick
fold → exact keys / bit / deferred), AF_UNIX connected ends (RD/WR keys, HUP
bit on shutdown/close, broadcast at exit), pre-accept connectors, AF_UNIX
listeners (UNIX bit, foreign), TCP/UDP/ICMP (daemon `readiness_moved` keyed
`inet_key`; listeners/udpany broadcast → re-probed every pass), DRM events
(DRM-open tag, deferred if contended), netctl, /dev/tty0 (DEVVT), pidfd
(PIDFD bit; non-leader exit deferred), inotify (never fires), regular files
(broadcast), console stdio (broadcast), VT keystrokes (evdev broadcast
flush). Every foreign-marked class is woken bit-only or broadcast (never
keyed), which `ep_unkeyed_bits` relies on.

Fixed (a5648753, 110d5401):
- evdev VT-switch resync (SYN_DROPPED → POLLIN on EVDEV tags) had only the
  DEVVT-tagged wake: `complete_switch`'s final wake is now a broadcast (one
  per switch; evdev_vt_activated runs under DRAW_LOCK so it must not wake).
- pty TCFLSH / TCSETSF free queue space (POLLOUT) with no wake: now take the
  existing PTY-tag wake.
- signalfd's tag named the fd-table OWNER's tgid while its answer reads the
  CALLER's pending set (differs when epoll probes through a fork child's
  description holder): now `current_tgid()`.

Known gaps, not wake bugs: TFD_TIMER_CANCEL_ON_SET is accepted and ignored
(a realtime clock step never re-translates or cancels absolute realtime
timerfds); closing an AF_UNIX listener leaves pending connectors without
HUP (if that changes, wake `poll_tag(UNIX, conn)`).

## Open / next

1. Laptop A/B (not touched, per instructions): laptop-deploy-kernel, boot with
   and without `epoll.ready=0`, YouTube, /proc/sysstat epoll ns per call for
   cosmic-comp (deskbusy2 baseline: 613 µs/call).
2. poll()/select(): relibc's poll() is a fresh epoll instance per call → a
   cache miss every time (11 % of calls on the desktop); it still gets the
   in-call savings. A per-thread cache keyed by the fd SET would fix it.
3. The first desktop run (Firefox VP9 page) showed "Video can't be played
   because the file is corrupt" after ~10 s of playback in BOTH modes — the
   test clip or the VP9 path, not this lane. `--put` of a 570 KB file over
   the serial shell takes ~50 min; use a small page (anim.html) instead.

Scratch (A/B images, scripts, logs):
/private/tmp/claude-501/-Users-forain-code-leandros/d7f39651-5cb0-4edb-ab21-fbb65b064269/scratchpad/epr2/
- `legacy-<arch>.img` (`epoll.ready=0`), `ws-{new,leg}-aarch64.img`
  (`sched.wakestat=1` [+ `epoll.ready=0`]): rebuild after a rebuild with
  `scripts/set-image-cmdline.sh <copy> "<cmdline>"`.
- `run.sh <arch> <tag> <runid> [runtests args]` waits for no QEMU, runs;
  `chain3.sh` (desktop A/B, `MODES`/`TAGX` env), `chain5.sh` (full suite).
- `ab.py <post.txt> <arch>` per-process sys + epoll table (names from the
  run's ps.txt), `join.py A B <arch>` side by side, `top.py` top syscalls.

Test commands:
`LEANDROS_RUN_ID=<id> python3 .claude/skills/run-leandros/runtests.py <arch> <tag> /bin/epolltest "/bin/epolltest bench" "cat /proc/wakestat"`
`... runtests.py <arch> <tag> --suite regress` / `--suite net-e2e`

## Pitfalls

- The session scratchpad is SHARED with sibling lanes: use a private subdir.
- `pgrep -f qemu-system` matches other agents' shell wait loops; check the
  executable name (`ps -A -o comm= | grep qemu-system`).
- runtests without `FFSESSION_OUT` writes to /tmp/ffsession/tests-<tag>.
- The guest shell has no `grep`; `grep -H . /proc/*/comm` in `--post` fails
  (use the run's ps.txt for names).
- Merge risk with lane sigfast (19bf2bf9): a trial `git merge-tree` shows ONE
  textual conflict, sched/src/task.rs — both lanes append fields at the END
  of `Task` and to the three constructors (keep both sides). runqueue.rs,
  lib.rs and servers/vfs auto-merge. Semantically independent: sigfast adds
  `sig_hint`/`sig_kick` and does not touch the poll-wake calls this lane
  relies on (signal posts wake `SIGNALFD(tgid)` via `unblock_port_tagged`,
  which records in listener rings; direct Ready-making paths leave the ring
  empty ⇒ full re-probe). If sigfast ever skips the signal poll wake for a
  thread that is not parked, signalfd under the ready list would stall:
  keep the wake unconditional.
