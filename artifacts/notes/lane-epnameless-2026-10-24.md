# lane/epnameless (2026-10-24) - FIXED

## Root cause
`sys_epoll_ctl` ADD/MOD of a not-ready fd called `widen_poll_masks(epoll_self_tag, fd_bits)`. That matched
EVERY parked task whose poll mask shared the instance's 1-of-64 bloom bit, ORed the new fd's bits into its mask,
cleared its key refinement (kbits) for them and pushed them into its listener ring. The new fd's later wakes then
woke unrelated parkers, which re-probed nothing they had registered ("nameless" useless wakes). calloop/`polling`
re-arms EPOLLONESHOT with MOD after every event, so this ran constantly. Ready-now ADD/MOD woke bit-only on the
instance tag, with the same collision.

## Fix (c164da53)
Each epoll instance gets a per-instance KEY (`PollSet::add_key(epoll_self_key(slot))`); ctl ready-now wake is
`wake_poll_key(epoll_self_key)`; `widen_port_masks(port, match_key, add)` requires a fingerprint match when the
bit is refined; `ep_kids_need` accepts the inner instance's key. Instrument: `# epnameless` line in /proc/wakestat.

## Guard
`/bin/epolltest kctl` (also in `stat`): waiter on 96 idle eventfds, helper does 64 ADD rounds (alternating not-ready+write /
ready) each on its own epoll instance (64 instances sweep every bloom bit; with one instance the guard collided only
1/64 and did NOT go red). RED (only c164da53's kernel/sched/vfs changes reverted): useless=54 parks=55 FAIL.
GREEN: useless=0 parks=1 PASS.

## Desktop A/B (aarch64, --nofirefox, same merged base, two 10 s windows, useless epoll wakes / 10 s)
| build | total | cosmic-panel | named | self | bits | all |
|---|---|---|---|---|---|---|
| unfixed run1 | 791 / 781 | 112 / 111 | 300 / 287 | 18 / 17 | 472 / 467 | 2 / 2 |
| unfixed run2 | 1286 / 1220 | 146 / 140 | 293 / 266 | 43 / 41 | 948 / 910 | 2 / 1 |
| fixed run1 | 18 / 3 | 3 / 0 | 16 / 0 | 0 / 0 | 0 / 0 | 2 / 2 |
| fixed run2 | 16 / 3 | 0 / 0 | 14 / 0 | 0 / 0 | 0 / 0 | 3 / 3 |

## Verification
aarch64 full regress PASS; `/bin/epolltest` all + `stat` PASS; x86_64 release build OK (not booted).
