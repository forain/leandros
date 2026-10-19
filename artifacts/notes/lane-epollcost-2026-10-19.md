# lane/epollcost — 2026-10-19

Branch `lane/epollcost`, rebased onto local main af0d9b76 (efdsem + sfdflake). Not merged, not pushed.
Commits: `epoll: nested registrations probe only the inner fds a wake names`, `epolltest cost: ...`, this note.

## Where cosmic-comp's 368 µs per epoll_pwait went

Counters added to `/proc/wakestat` (`# epollx inner passes recheck gencancel useless probe_ns filt_ns`) split the
ready-list path. The cost was neither RUN_QUEUE nor bloom false positives: it was **nested epoll registrations**.
The ready list treated a nested registration as "always re-probe", and one probe of it asked every inner registration
twice (`epoll_any_ready_nested`, then `epoll_tag_mask`), plus once more per filter build (`epoll_collect`). Any
epoll_ctl anywhere moved the global `EPOLL_CTL_GEN`, which marked the nested answer stale, forcing a rebuild and a
recheck pass. cosmic-comp's calloop instance nests wayland-server's (one AF_UNIX end per client) and libinput's;
cosmic-panel nests its applet server the same way. Probe time was ~95 % of epoll kernel time.

## Fix

`EpKids`: a nested registration keeps a snapshot of its inner instance. Its kids are diffed every pass (no IPC) and
probed only when a recorded wake names one (`ep_hit` on the kid's own tag), when one fired last time or its answer
changed, or when one has no per-object tag. The listener filter holds the kids' own tags and keys. The registration
fires (POLLIN, level) while a kid fires. A twice-nested kid is still probed whole. `epoll.kids=0` restores the old rule
for A/B runs. The invariants are the ready list's: kid readiness turns on only through a wake on its tag, a MOD, or an
ADD (the last two are seen by the diff). A pass that runs out of `maxevents` marks the kids it would have probed
`ready`, so their wake is not lost with the ring.

## Results (aarch64 HVF, release; old = same image with `epoll.kids=0`)

`epolltest costbench`: the outer set holds 3 eventfds, a 60 Hz timerfd, a signalfd, an AF_UNIX listener and a pipe,
plus nested W (32 socketpair clients) and L (4 pipes). A client thread does request→reply round trips.

| | old | new |
|---|---:|---:|
| comp, unpaced: kernel µs per epoll_wait | 36.2 | 5.2 |
| comp+noise (300 µs pacing, another process doing AF_UNIX ping-pong + epoll_ctl churn): µs per call | 149.5 | 11.2 |
| comp+noise: inner probes per call | 125 | 2.8 |
| stray wakes (inner pipe written and drained before the waiter looks): useless wakes / 1000 pushes | 1022 | 1021 |
| stray: kernel µs per useless wake | 87.3 | 9.9 |
| stray: inner probes per useless wake | 76.8 | 1.1 |

Useless wakes per call in comp+noise are 0 in both: the keyed filter already rejects the unrelated traffic. The panel's
pattern (many parks per return) is a cost-per-wake problem, and that cost drops ~9×.

Desktop A/B (COSMIC + Firefox on a CSS animation, virgl, 12 s window; `ffsession.py ... --post` with sysstat `e` rows):

| | old | new |
|---|---:|---:|
| cosmic-comp epoll | 741 ms / 5688 calls = 130 µs | 219 ms / 9911 calls = 22 µs |
| cosmic-panel epoll | 387 ms / 104 calls | 3.5 ms / 96 calls |
| all epoll waits | 1212 ms | 346 ms |
| inner probes in window | 722 k | 46 k |
| filter build time | 291 ms | 2 ms |

cosmic-comp made 74 % more calls in the new run: it rendered more frames.

Tests: `epolltest cost` (new, in `regress` and the `epollcost` suite) covers nested LT, ET, ONESHOT re-arm (including a
re-arm while away), an inner fd closed, a dup'd inner fd, and ADD while parked (inner and outer). It also asserts
inner+direct probes per call ≤ 8 and inner probes per stray wake ≤ 4. On the old rule it FAILS both assertions and
passes the correctness cases. Post-rebase on af0d9b76: `--suite epollcost` PASS, `--suite regress` PASS,
`--suite net-e2e` PASS (45/45 commands RC=0). x86_64 not run (per lane rule).

## Open

- Laptop A/B (deploy with and without `epoll.kids=0`, YouTube, sysstat `e` rows for cosmic-comp/panel).
- The panel's stray wakes themselves (239 parks/s for 9 returns on the laptop) still happen. They are now cheap; finding
  which kid they hit needs a laptop run with the epollx counters.
- Scratch: `scratchpad/epc/` (run.sh, chain.sh, desk.sh, py/erows.py, tests-epc-*, run-desk-{new,old}).
