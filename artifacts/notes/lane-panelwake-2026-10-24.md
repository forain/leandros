# Lane panelwake (panelwake / panelwake2 / panelwake3), 2026-10-23..24 — FIXED (partial on the panel)

Goal: cut useless epoll wakes on the idle COSMIC desktop (cosmic-panel ~17-24 useless/s).

## Root causes and fixes
1. DRM event wakes went out bit-only via the deferred `POLL_WAKE_PENDING` mask (drm_event_wake fallback when
   try_wake was contended): ~37 tasks woken per call. Now keyed: `drm_event_poll_key(open)`, VFS_POLL replies the key.
2. Child exit wakes (WAIT / PIDFD) were bit-only: every parked poller sharing the bloom bit through an unrelated keyed
   interest woke. Now keyed; PidFd moved off `poll_vnode_foreign` (foreign interests are skipped by keyed wakes).
3. Signal wakes keyed (`sched::signal_wake_key(tgid)`), SignalFd keyed.
4. **SIGCHLD-ignored wake bug**: `post_signal_process` set the chosen Blocked thread Ready even when the group discards
   the signal (SIG_IGN, or default-ignore SIGCHLD/SIGURG/SIGWINCH). Linux `sig_ignored` drops it. This was ~1 useless
   epoll wake per child exit and the dominant guard hit (d47a327f: useless=25).
5. Probe commit b2d0f103 (wakestat::note in request_poll_wake_tagged) REVERTED: relaxed atomics, but it put bit counts
   in the "woken" column (wrong semantics for /proc/wakestat) and its question is answered.

## Guard
`/bin/epolltest stat` -> `stat_exit_wake_keyed` (also `/bin/epolltest xwake` alone): 96 idle eventfds, 24 children
exit during one epoll_wait. main fd772fe9 + guard: useless=37 parks=38 (RED, both modes). Lane: 0 (1 only on the
first test after boot). That residual is not an exit wake: `xwake` alone and a second `stat` read 0; /proc/wakestat
attributes it to the eventfd write site during boot-time session teardown; u16 fingerprints (96 parked) vs several
hundred boot-window keyed wakes => fingerprint collision, inherent. Bound set to <= 1 with that comment.

## Desktop, 10 s delta (QEMU aarch64/HVF --virgl, ffsession --nofirefox, two /proc/epollstat snapshots)
| | before (main 3537f33e) | after (lane, run after3) |
|---|---|---|
| total useless/s | 260 | 156 |
| total parks/10 s | 2782 | 1734 |
| cosmic-panel useless/s | 17 | 13.6 (ep37: 46, pipe fd 92 = 45) |
DRM site: 0.8 woken/call (was 17.8). Panel residual is its own pipe (fd 92), single run; probe2 run read 2.6/s.

## Tests (aarch64)
regress pw3reg: all pass except sshsmoke ssh_hostkey (known flake; passes on rerun). --virgl: drmsmoke,
drmsmoke --tokenscope, --maplife, gpuprobe gl pass; sigtest, sigchldtest, waittest, jobtest pass.

## Open
- net `unix_end_release` (servers/net/src/lib.rs ~7284) still bit-only on unix close, ~6 calls/10 s, 18.7 woken/call:
  convert to wake_poll_key(poll_key(UNIX, conn_idx)) (unix_end_keys already registers it). Skipped (not trivial to verify).
- cosmic-panel pipe fd 92 useless wakes: the wake attribution per candidate bit, next lane should name the writer.
