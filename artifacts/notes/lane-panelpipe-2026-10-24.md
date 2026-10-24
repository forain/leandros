# Lane panelpipe, 2026-10-24 — FIXED (the panel's pipe wakes); panel residual open

## Who holds the pipe
cosmic-panel fds 90..98 are its applets' stdio pipes: each applet child (cosmic-applet-a*, -n*, -t*,
leandros-applet) has fd 0/1/2 on a pipe whose other end the panel holds (from /proc/*/fd, run ppdesk2:
e.g. panel 91 <-> cosmic-applet-a fd 0, panel 94 <-> fd 1, panel 97 <-> fd 2). The applet writes its
log output; the panel's event loop reads it to EAGAIN.

## Root cause (kernel semantics bug, not the app)
Both ends of a pipe had ONE poll key (`pipe_key(ring)`). Every read that did not free a full ring paid a
deferred keyed wake (`request_poll_wake_key`, coalesced to the next tick) meant for EPOLLET writers. The
key also named the read end, so the reader, already re-parked on the now empty pipe, took that wake,
re-probed, found nothing: one useless wake per drain. Writes to a non-empty pipe likewise woke parked
writers.

## Fix
- `poll_class::PIPE` = read end (data, EOF), new `poll_class::PIPE_WR` = write end (space, last reader gone).
- VFS: `pipe_rkey`/`pipe_wkey`/`pipe_end_key`; drains wake only the write end, appends only the read end;
  last writer close wakes the read end, last reader close the write end; probe tags/keys per end.
- kernel splice/tee park on `PIPE` for the input ring and `PIPE_WR` for the output ring.
- net `unix_end_release`: bit-only close wake -> `wake_poll_key(poll_key(UNIX, conn))` (every connected
  end's probe names that key in `unix_end_keys`; bit-only parkers are still reached by a keyed wake).

## Guard
`/bin/epolltest drain` (also in `epolltest stat`): `stat_pipe_drain_keyed`, reader epoll on a pipe, a
helper thread writes 1 byte x 20, reader drains to EAGAIN each time.
main 04f1c383: useful=20 useless=15 / 19 (RED). Lane: useful=20 useless=0 (9/9).
Note: a retry that reuses a closed epoll fd number reads the stale /proc/epollstat row (useful=0
useless=0); the guard keeps its ep open and requires useful >= 1.

## Desktop (aarch64/HVF --virgl, ffsession --nofirefox, two /proc/epollstat snapshots 10 s apart)
| | panelwake after3 (main) | lane ppdesk2 |
|---|---|---|
| cosmic-panel useless/10 s | 136 (ep37: 46, pipe fd 92: 45) | 86 |
| panel useless attributed to a pipe | 45 | 0 |
| total useless/10 s | 1560 | 1567 |
Baseline is the panelwake note's single run (its probe2 run read 2.6/s), not a same-build A/B.
Total desktop useless did not move: the pipe wakes were a small share.

## Open
- cosmic-panel residual: four epoll instances (ep 7/18/22/25) at ~20 useless/10 s each with parks ==
  useless and NO fd attribution (the wake named none of their registrations): look at which wake site
  (/proc/wakestat) hits instances whose registrations are not named, likely bit-only or ALL wakes.
- Desktop-wide useless ~156/s unchanged; top tgids 147/175/371/302/182 (~13/s each), same signature.
