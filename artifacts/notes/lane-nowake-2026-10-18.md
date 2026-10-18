# lane/nowake — 2026-10-18

Branch `lane/nowake` off local main 2953e95e. Not merged, not pushed.
Follow-up of lane epollready ("Open, scheduler, epoll and wakes" in TODO.md).

## Commits

| commit | what |
|---|---|
| 60c17ec8 | `TFD_TIMER_CANCEL_ON_SET` (Linux semantics) + absolute realtime timerfds follow a clock step; timertest `timerfd_cancel_on_set` |
| b4e7e35e | closing an AF_UNIX listener resets its pending connectors (HUP/ERR, ECONNRESET, EPIPE, wake) |
| ddd20751 | three no-wake producers from the invariant-16 audit: nested epoll after an inner epoll_ctl, pidfd reap, signalfd4 re-mask |
| 4c7ad28a | /proc/sysstat pseudo rows: epoll parks (65535) and ready-list misses by cause (65534 first, 65533 other instance, 65532 listener off) |
| e577c235 | epolltest: stray-wake verdict on the waiter's own parks (the flake); cases 25 and 26 |
| f31168f9 | **UNTESTED**: epoll keeps one other instance's snapshot per thread (`EpCache::others`) + epolltest case 27 `alternating_instances_cached`; builds on aarch64, never run |

## 1. TFD_TIMER_CANCEL_ON_SET

`vfs::timerfd_clock_was_set(old_off, new_off)`, called by the kernel's
`step_realtime_ns` (clock_settime, settimeofday) after the offset is stored.
`TimerFdEntry` gained `abs_realtime`, `might_cancel`, `canceled`, `cancels`.
Rules (Linux `timerfd_setup_cancel` / `timerfd_clock_was_set` /
`timerfd_canceled`): the flag counts only with TFD_TIMER_ABSTIME on a
CLOCK_REALTIME fd, armed or not (systemd's clock-change watch); every settime
clears a pending cancellation; a cancelled fd polls POLLIN and its next read
answers ECANCELED (and drops expirations); `cancels` moves the poll seq so ET
sees each one. Wake: `wake_poll_key(timerfd_key(slot))`, the key the probe
reports. Also fixed on the way: an absolute CLOCK_REALTIME timer (any flags)
kept its MONOTONIC deadline across a step; it now keeps its wall instant
(deadline moved by the step, registered with the deadline tick, woken if due).

## 2. AF_UNIX listener close

`free_bound_idx` → `unix_listener_gone(sock_id)` when the bound address's last
reference goes (close, exit, xfer_drop, GC). Every `UnixPendingAccept` entry
for that sock_id (all fork copies) becomes `UnixConnected { is_a: true }`; the
conn gets `closed_b`, `refs_b = 0`, `reset_a` (new field), its pre-accept
ring_ab bytes and fdq_ab fds are discarded. `reset_a`: POLLERR while set; the
first recv/recvmsg finding no data answers ECONNRESET and clears it (not with
MSG_PEEK). Wake: `wake_poll_tagged(poll_tag(UNIX, conn))`, the state key end A
reports. Not done: SO_ERROR does not report the ECONNRESET.

## 3. The `unrelated_unix_traffic_no_wake` flake

Root cause: the verdict measured the wrong thing. It compared the
system-wide `/proc/wakestat` count of narrow-mask tasks woken (net sites) in a
pass with the waiter against a control pass without it; in a live session the
other tasks' wakes differ between passes by more than the threshold. Evidence:
the new runs print that difference next to the waiter's own parks —
`system_woken_delta=-11 waiter_parks=1` (aarch64, quiet VM: the "delta" is
already noise of ±11 with a perfectly quiet waiter). No stray wake of the
waiter was ever involved. The verdict is now the waiter's own epoll parks
(sysstat row 65535 of the test's tgid; the main thread makes no epoll call
during the pings): `(parks - 1) * 8 < 600`. Measured: parks = 1 on both
arches. `timerfd_set_not_woken_by_unix_traffic` shares the harness and got
the same verdict.

## 4. relibc poll(): premise refuted

relibc's `poll()` does create a fresh epoll instance per call — but nothing on
the desktop links relibc. relibc-linked binaries are exactly the
`RELIBC_LINKED` list in scripts/build-userland.sh (test programs, ping,
drmsmoke, evtest2, vttest ...); COSMIC, Firefox, busd, sshd, NetworkManager
are static musl, whose poll() is the kernel's poll/ppoll. So the 11 % desktop
miss rate cannot come from relibc, and caching in relibc would not change it.
No relibc branch was made (relibc master 10ef96ee untouched).

What can miss on the desktop: a thread's first call; a thread alternating
epoll instances (the snapshot holds one); a thread whose listener was ended
by a kernel poll()/select() park in between (`block_on_port_until_keyed`
ends it because it overwrites the shared `poll_fps`). 4c7ad28a adds the
per-tgid cause census to find out which.

### Desktop miss census (aarch64 HVF, COSMIC + Firefox on fftest.html, code at e577c235)

`# epoll calls 123835 miss 3862` (3.1 % here; the 11 % was a YouTube run).
By cause: other instance 3223 (83 %), listener off 367, first call 267.
"Other instance" is almost all cosmic-comp: pid 143 (session cosmic-comp)
2109 of 12029 calls, pid 34 (gone by the end; likely the greeter's
cosmic-comp) 753 of 5205, cosmic-panel 360 of 968. cosmic-comp's main thread
alternates calloop's epoll and wayland-server's (zero-timeout
`dispatch_clients`, nested in calloop's). So the "equivalent" of the relibc
cache is a per-thread multi-instance snapshot in the kernel: f31168f9,
untested. Run: `ffsession.py aarch64 nwmiss --url file:///tmp/fftest.html
--wait 30 --post "cat /proc/sysstat; cat /proc/wakestat"`; parse rows
65532-65534 per tgid from post.txt, names from the `/proc/<pid>/cmdline`
blocks in ps.txt.

## 5. Invariant-16 audit

Explore-agent audit of every probe arm vs its state writers, beyond the list
lane epollready already covered. Confirmed and fixed (ddd20751):
- **Nested epoll** (real hang): see the commit. `EpCache::cgen`. Negative
  control: epolltest `nested_inner_add_while_away` with the fix disabled
  FAILS (4 trials, max 1312 ms: rescued only by the writer thread's exit
  broadcast; an earlier version of the test whose writer exited at once
  passed on the broken kernel — exit broadcasts hide lost keyed wakes, keep
  that in mind for every wake test).
- **pidfd reap**: POLLHUP appears at reap with no wake → wait4/waitid wake
  `poll_tag(PIDFD, pid)`.
- **signalfd4 re-mask**: wider mask can make a pending signal readable →
  wake `signal_wake_tag(current_tgid)`.
Suspicious, not fixed: process-directed signals parked in one thread's private
pending set are invisible to a signalfd polled by another thread (semantics,
not wake); `probe_interest`'s in-flight path drops keys (over-inclusive, OK);
signalfd4 re-mask rewrites only the calling table's entry (dup/fork copies keep
the old mask).

## Results (code at e577c235, i.e. everything but f31168f9; Mac: aarch64 HVF, x86_64 TCG, release)

| run | aarch64 | x86_64 |
|---|---|---|
| `--suite regress` (27 commands) | PASS (nw-regress-aarch64-r1) | PASS (nw-regress-x86_64-r1) |
| `--suite net-e2e` (5) | PASS | PASS |
| epolltest | 26/26 | 26/26 |
| timertest `timerfd_cancel_on_set` | PASS, blocked read woken 188 µs, poll 172 µs | PASS, 1083 / 1946 µs |
| `unix_listener_close_resets_pending` | events=IN\|ERR\|HUP in 102 ms, ECONNRESET, EOF, EPIPE | same, 101 ms |
| `nested_inner_add_while_away` | 4/4, max 102 ms | 4/4, max 106 ms |

## Remaining (for a continuing lane)

1. Test f31168f9 (multi-instance snapshot): build both arches, run
   `/bin/epolltest` (expect 27/27; case 27 `inst_misses` <= 4, while e577c235
   should give ~80), then `--suite regress` and `--suite net-e2e` on BOTH
   arches, then the desktop census run above (expect cosmic-comp's "other
   instance" misses near 0, desktop renders, Firefox up, no stall). If
   anything hangs, `git revert f31168f9`: everything before it is tested.
   Review points: every ring the listener hands over goes through
   `ep_share`; every listener restart (`poll_listen_begin`) clears `others`;
   the filter build adds the kept snapshots' `filt` entries and their
   instance self-tags; the miss path keeps the listener id via
   `poll_listen_update(ALL)` instead of restarting it.
2. Not done: SO_ERROR does not report the listener-close ECONNRESET;
   signalfd4 re-mask rewrites only the caller's fd entry (dup/fork copies
   keep the old mask); process-directed signals parked in one thread are
   invisible to a signalfd polled by another thread.
3. TODO.md still lists the fixed items under "Open, scheduler, epoll and
   wakes" (CANCEL_ON_SET, listener HUP, the flake, relibc poll): drop them at
   merge and replace the relibc line with the census result.
   docs/kernel/event-loop.md still has the relibc poll() bullet.

Scratch: `/private/tmp/claude-501/-Users-forain-code-leandros/d7f39651-5cb0-4edb-ab21-fbb65b064269/scratchpad/nowake/`
(`runs/`, `suites.sh <sfx> arch:suite...`, `stage.py` hunk stager).
