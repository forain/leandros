# Lane polish — 2026-09-26

Branch `lane/polish` from origin/main `2af5ad4`. Pushed. Worktrees: Mac `~/code/leandros-polish` (aarch64 builds),
laptop `~/Projects/leandros-polish` (x86_64/KVM, Venus->zink on ANV and virgl/iris). Scratch on the laptop:
`~/Projects/polish-{postkill,sesskill,cubekill,loadtest}.sh`, results in `~/Projects/polish-out*/`.

Commits: `32fd84a` drm per-open events, `c21be30` jobctl, `7505486` zinkbench+init, `52ebc2c` ctx fence drain/abandon,
`b22c758` jobtest case 7.

## 1. drmsmoke FLIP_EVENT_DELIVERED_ON_FENCE after a compositor dies: FIXED (two kernel bugs)

Repro: killing the greeter, even at 8 s, 14 s, 20 s, 26 s, 30 s or 45 s, gave 0 failures in 12 runs. SIGKILLing a full
COSMIC session gave 0 in 10. The reliable repro is
`MESA_LOADER_DRIVER_OVERRIDE=zink timeout -s KILL 6 kmscube; drmsmoke`. That is a Venus client killed with work in flight.
Baseline on 2af5ad4 plus a diagnostic: **11 of 20 runs failed**, in two different ways.

- **A. Stale flip event (10 of 20).** `PENDING_FLIPS` and `READY_EVENTS` were global. The dead client's queued event went to
  drmsmoke's first read(). The `user_data` was wrong, so every later read was one event behind: READ_FLIP_EVENT,
  FLIP_TS_SUBTICK and the GPU_IRQ burst failed (failed=7), with d_flips_on_fence=1.
  Fix: events are tagged with their open. read and poll use the VFS cookie in slot 4, and `drm_release_open` purges that
  open's events.
- **B. Fence floor hole (1 of 20). This is the reported shape: 0/32 on the fence, failed=1, latency 30 ms.**
  The diagnostic showed `floor` stuck at `0x6A14`. The stuck fence was `0x6A15`, a SUBMIT_3D on ctx 0xF ring 1 (RING_IDX),
  still in flight after CTX_DESTROY, while the ahead count kept climbing. The host never answers a context-ring fence
  for a destroyed context. The system heals once `fences_ahead` overflows, which takes about two runs.
  Fix: `drm_release_open` waits up to 200 ms, with the lock dropped, for that context's fenced commands before CTX_DESTROY.
  After the destroy, `ctx_abandon_fences` retires any leftovers in the accounting and prints `[GPU] ctx N destroyed with
  unanswered fences` (never printed in verification: the drain sufficed).

Refuted: the premise that host latency beats the 20 ms fallback. Under host CPU burn, latency was 22–37 ms and all runs
still delivered 32/32 on the fence.

Verification (x86_64/KVM): Venus kmscube-kill **25/25** clean (baseline 9/20). virgl kmscube-kill **20/20** clean.
Venus greeter-kill: **18/18** clean across 9 boots (a tenth boot's login flaked). aarch64/HVF drmsmoke failed=0, with 32/32
on the fence.

## 2. zinkbench leaves text-login: FIXED
zinkbench no longer reboots. It sets the marker, kills greetd, waits until greetd is gone plus 6 s, and removes the
marker. Every step is a short serial command, and a host-side `finally` is the backstop. A marker that was already present
is left alone. init also unlinks `/run/greetd-init.pid` at boot and when greetd exits: /run is on the persistent f2fs, and a
stale pid made my first repro kill the serial shell. Verified: the run printed "greeter stopped for this boot; image
unchanged", and the next boot had no marker and greetd running as pid 4.

## 3. jobtest orphan: FIXED, two kernel bugs
The leftover was jobtest's TOSTOP writer, found as `28 T ppid=2 pgrp=28 sid=22`.
(a) SIGKILL resumed it, but the in-place job-control retry raised SIGTTOU again and it stopped again. `jobctl::check` now
returns EINTR when `fatal_signal_pending()`.
(b) `kill_orphaned_pgrps` ran after `reparent_children`, so the child-group half never matched. That half now runs from
the reparented set inside `reparent_children`. New jobtest case 7 (`orphaned_child_pgrp_sighup`) covers it.
Results: aarch64/HVF jobtest, sigtest, waittest, sigchldtest, ptytest, vttest and pthreadtest all RC=0, with no stopped
leftovers. x86_64/KVM: the same set plus idletest and killmt RC=0, case 7 PASS, no leftovers.

Shared files: `sched/src/lib.rs` and `sched/src/signal.rs` (overlap with scheduler/teardown lanes), `servers/tty/src/jobctl.rs`,
`drivers/src/{drm_device_interface,virtio_gpu}.rs`, `servers/drm/src/lib.rs`, `userland/init/src/main.rs`.
Open: each abandoned fence still leaves one control-queue chain parked host-side (QEMU never completes it). This is only
reached if the 200 ms drain times out.
