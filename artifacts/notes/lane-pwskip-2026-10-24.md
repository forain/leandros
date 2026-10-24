# lane/pwskip (L3.1) - lock-free skip for keyed poll wakes - REFUTED

Date: 2026-10-24. Lanes pwskip + pwskip2. aarch64 QEMU (HVF), `--gpu virgl`.

## Idea
`wake_poll_key` always takes RUN_QUEUE (TASKS). Publish a seqlocked summary of the keyed wake index
(wx_all / wx_row / ux_row / kx_row bits) and skip the lock when no parked listener can match the key
(Dekker: waker = readiness store, epoch RMW, SeqCst fence, summary load; parker = index + summary store
under TASKS, SeqCst fence at the end of wx_sync, then readiness re-probe).

## Census (sched.pwskip=2: always lock, count would-skip; gate = would >= 30 % of calls, bad = 0)
| window | eligible keyed wakes | would skip | allnz (wx_all set) | race | bad |
|---|---|---|---|---|---|
| COSMIC desktop idle 10 s | 444 | 0 (0 %) | 10 | 0 | 0 |
| desktop + /bin/epolltest 8 s | 4119 | 12 (0.3 %) | 9 | 0 | 0 |
| Firefox on YouTube 20 s | 3498 | 0 (0 %) | 16 | 0 | 0 |
| since boot (desktop run, incl. boot) | 41216 | 1775 (4.3 %) | 464 | 0 | 0 |

The summary is correct (bad = 0) but almost never empty. The B.md risk (wx_all usually non-empty) is
NOT the reason: only ~0.5-2 % of reads see a POLL_TAG_ALL listener. The keyed wakes simply have a
matching parked listener almost every time - servers wake keys because someone waits on them. The
lock hold is the real wake work, so there is nothing to skip. Top sites (vfs:8422, net:4891,
net:6309, vfs:8525, vfs:8192) had 0.03-2 % empty reads each.

## Outcome
- Not merged: the summary maintenance (incremental populations in wx_sync, seqlock publish, an extra
  SeqCst fence per park, reordered epoch load in poll_listen_begin_since) costs on every park and buys
  nothing. Code preserved on branch lane/pwskip (d6f84611 + 6d5868c6 reason counters allnz/torn).
- Merged (lane/pwskip2): only the userland guard `futextest poll_lost_wake_stress` (8 ping-pong pairs
  over eventfd+epoll, pipe+poll, unix socketpair; 128000 round trips, lost 0, 2.4 s on aarch64 HVF).
  Caveat: with injected bug 2 (skip path without the epoch bump) it still passed, because the skip
  path rarely ran - it is a generic lost-wake guard, not proof of sensitivity to any one bug.
- Implication for the TASKS-lock work: poll-key wake cost must be cut inside the locked path
  (shorter hold per wake, batching repeated wakes of one key in the servers), not by skipping it.
