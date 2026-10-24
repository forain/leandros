# lane/playcost4 (L3.1) - 2026-10-24 - FIXED in QEMU (laptop trial pending)

## Problem
During laptop playback the remaining TASKS/RUN_QUEUE costs were keyed `wake_poll_key` (unix send lib.rs:~4891
2085/s x 6 us, unix recv POLLOUT ~6309 1702/s x 5 us) and `widen_poll_masks` from epoll_ctl (1021/s x 13 us).
The wake index (`wx_row`, lane wakescan) narrowed a keyed wake only to the slots indexed under the key's
1-of-64 bloom bit; most were then dropped by the park mirrors, but each visit costs ~5 scattered mirror loads.
`widen_port_masks` still walked every parked slot (the playcost prefilter only avoided the Task deref).

## Fix (16ddedbe)
`runqueue.rs`: two more bitmap index families maintained in `wx_sync` (every index-relevant write already
calls it): `ux_row[b]` = slots whose bit `b` some UNREFINED interest explains (`unref_mask`: park part outside
`pk_kb`, listener part outside `ls_kb`), `kx_row[h]` = slots with refined bits whose fingerprint bloom (`fpb`)
has bit `h` (`kx_bl` = indexed copy). Keyed wake and poll-channel widen visit
`wx_all | ux_row[b] | (wx_row[b] & kx_row[fp & 255])` - a superset of the existing mirror check, so no wake is
dropped that the full scan would deliver (`sched.wakeidx=2` verify mode covers it; `=0` = full scan). Bitmaps,
so no overflow; multi-bit tag wakes keep the old row path, non-poll ports the full scan (counted as `full`).
Widen also skips refined fingerprint misses by mirror. Empty occupancy words are skipped.
Counters (/proc/rqhold): `# kwake calls slots deref full`, `# widen calls slots full`.

## Guard (75c6cee8)
`/bin/futextest kwake_scan_indexed`: 32 threads in epoll_wait on 16 idle eventfds each; 300 eventfd write+read
(600 keyed wakes) and 300 not-ready epoll_ctl ADD/DEL. Unindexed (old scan, same counters): 4.00 slots/wake,
47.00 slots/widen -> FAIL. Fixed: 1.00 slots/wake, 0.00 slots/widen -> PASS (bound 2.0 each).

## Desktop (aarch64 HVF, ffsession --nofirefox --gpu virgl, 10 s idle window, then 8 s with /bin/epolltest)
| | keyed slots/wake idle | load | widen slots/call idle | load |
|---|---|---|---|---|
| unindexed | 22.1 (517 wakes, 2.0 deref) | 23.5 (4149) | 243 (338 calls) | 237 (635) |
| fixed | 2.5 (522 wakes, 2.0 deref) | 2.4 (4145) | 2.9 (340 calls) | 2.2 (645) |
Boot to desktop: widen slots 13.3 M (unindexed) vs 0.24 M (fixed).

## Verification (aarch64)
regress PASS; schedstress full, epolltest, polltest, futextest, nettest PASS. x86_64 not built (merge batch).

## Open
- Laptop trial: rqhold hold time at lib.rs wake_poll_key sites and epoll_ctl widen during YouTube.
