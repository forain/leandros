# lane/laptopdesk — 2026-10-24 — scripted laptop login + idle desktop census (L2.7, L2.8, L3.1)

## Scripted login: DELIVERED (e98a4d57)
- `/bin/greetd-login` (userland/greetdlogin, std/musl, no deps): password on stdin, greetd IPC on
  `$GREETD_SOCK` or the newest `/run/user/990/greetd-*.sock`: cancel_session, create_session,
  post_auth_message_response, start_session with cosmic.desktop's `Exec=` (`/bin/sh /bin/start-cosmic-leandros`)
  and the XDG_SESSION_TYPE/CURRENT_DESKTOP/SESSION_DESKTOP env. Built by build-all.sh, staged into the rootfs.
- Mac side, one command: `LEANDROS_REMOTE=172.16.149.179 scripts/desktop-login.sh <creds-file>`
  (file has `user=` / `pass=` lines). Pushes the binary if it differs, pipes the password over SSH stdin
  (never on a command line or in a log), then polls /proc until cosmic-comp and cosmic-panel run as the
  user's uid. Exits 0 at once if the session is already up. Hold the laptop lock around it.
- Laptop run: kernel ef3a1b6583 (slot good 8f46ed46696d), "session up after ~6 s (cosmic-comp + cosmic-panel
  as uid 1000)". `blanktest --status` = On before and after both census windows (all within ~3 min of login).

## Census (laptop, idle COSMIC session, no Firefox, two runs x two 10 s windows)
Same commands as panelwake/epnameless (/proc/epollstat + /proc/wakestat, `epn-an.py`), nowake (/proc/sysstat
rows 65532-65535) and percpu4 (/proc/rqhold, `percpu4/an.py`). Script: census.sh = 3 snapshots 10 s apart.

| item | run1 w1 / w2 | run2 w1 / w2 | verdict |
|---|---|---|---|
| L2.7 useless epoll wakes /10 s (total) | 4 / 8 | 4 / 4 | CONFIRMED-FIXED (QEMU before ~1250, after ~10) |
| L2.7 cosmic-panel useless /10 s | 0 / 0 | 0 / 0 | (QEMU before ~140) |
| L2.7 residual owners | sh 4-5 (ssh shell running the census), busd 3 | sh 4 | nameless: all 3-4, named 0-1, bits 0 |
| L2.8 epoll parks /10 s | 198 / 200 | 199 / 198 | panel ~80, cosmic-comp ~78, sysbus ~22 |
| L2.8 ready-list misses inst / first / off | 0 / 0-1 / 0 | 0 / 0-1 / 0 | CONFIRMED (no other-instance misses; cosmic-comp 0) |
| L3.1 TASKS acq/s | 27228 / 26946 | 30215 / 30198 | |
| L3.1 TASKS held ms/s | 20.2 / 19.8 | 19.9 / 20.0 | |
| L3.1 TASKS contended/s | 31 / 28 | 35 / 33 | idle: OK (QEMU playback ~3.6k/s) |
| L3.1 TASKS spin ms/s | 0.4 / 0.3 | 0.3 / 0.4 | idle: OK; playback on the laptop still unmeasured |
| RQ* acq/s, spin ms/s | 2824, 0.71 / 2786, 0.55 | 2826, 0.59 / 2794, 2.60 | |

## New findings (idle, laptop)
- `sched/src/lib.rs:1837` `tgid_of` = 26-29k TASKS acquisitions/s on an idle desktop (96 % of all TASKS
  acquisitions; 0.45 ms/s held, ~0 spin). Cheap per hold but it is the TASKS acquisition rate; 100 call sites,
  the hot caller is not yet named.
- `sched/src/lib.rs:7062` `percpu_check` (lane percpu checker, once per tick under TASKS + every RQ lock):
  100 holds/s x ~144 us = 14.4 ms/s, i.e. ~72 % of all TASKS held time on the idle laptop. A debug checker
  this heavy on hardware is worth gating or thinning.

## Open
- L3.1 playback confirmation: after `desktop-login.sh`, start Firefox as leandro (docs/development/remote.md)
  and run percpu4's m.sh within 15 min of login.
- The laptop was left logged in (session for leandro, uid 1000).
