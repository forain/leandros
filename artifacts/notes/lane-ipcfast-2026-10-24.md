# lane/ipcfast (ipcfast, ipcfast2, ipcfast3) — L3.7, 2026-10-24

**FIXED (lock contention); the 2-pair/1-pair time ratio is PARTIAL: noise-bound under HVF.**

Two independent AF_UNIX pairs slowed each other down because every unix socket in the
system serialized on SOCK_TABLES (a plain spin mutex, write-locked on every send) and on
UNIX_CONNS (held across the user copy of the payload, both directions).

## Changes
- `/proc/lockstat` counters; lock ids `L_UNIX_CONNS` (12), `L_SOCK_TABLES` (13);
  `TrackedGuard::unlocked`, `TrackedRwLock` (sched/src/lockwatch.rs).
- SOCK_TABLES is a reader-writer lock; the hot paths take `.read()`, including
  `handle_send_k`, which still took the write lock on every send() and every no-fd sendmsg()
  (that was the 64 B SOCK_TABLES spin left over after ipcfast2).
- UNIX_CONNS: reserve → copy with the lock released → commit (`UnixRing::reserve_write/commit_write`,
  `reserve_read/commit_read`, `wbusy/rbusy`; slots, rings and buffers are not reset, reused, grown
  or shrunk while a copier is inside). Covers stream sendmsg (ipcfast2), stream recv/recvmsg
  (`unix_stream_read_locked`) and send/sendto/write plus the pre-accept write (`unix_stream_write`).
  Copies under 1 KiB (`UNIX_UNLOCKED_COPY_MIN`) stay under the lock: they cost less than the
  extra release and re-take.
- kernel/src/syscall.rs `net_block_lazy`: the fd's O_NONBLOCK (NET_GETFL, a whole net-server
  round including SOCK_TABLES) is looked up only when the first attempt returns EAGAIN, not on
  every send/recv/read/write/accept. 1-pair 64 B: 7900 → ~6300 ns per round trip.

## Numbers (aarch64 HVF, on-CPU send+recv ns per round trip, best of 3, `scmtest pairbench 4000`)

| case         | original 1p / 2p / ratio | ipcfast2 WIP          | final (3 runs)                       |
|--------------|--------------------------|-----------------------|--------------------------------------|
| stream 64 B  | 9117 / 15984 / 1.75      | 7900 / 13266 / 1.67   | ~6300 / 8500-10900 / 1.36-1.69       |
| stream 64 KiB| 51887 / 98692 / 1.90     | 18727 / 27545 / 1.47  | ~21400 / 25400-34600 / 1.23-1.60     |
| seqpkt 4 KiB | ~1.8                     | 14541 / 18285 / 1.25  | ~11500 / 13800-18500 / 1.29-1.52     |

Lock spin, 2 pairs: SOCK_TABLES 30 ms → 0-4 µs per run; UNIX_CONNS 32 ms (64 KiB) → ~1.1-1.3 ms.
Spin share of a 2-pair round trip: original 160-230 ‰ (64 B: 3750 ns of 15984; 64 KiB: 16 µs
of 98692), final 7 ‰ (64 B), 52-54 ‰ (64 KiB, seqpacket).

## Guard
`/bin/scmtest pairbench [iters]` fails when the 2-pair lock-spin share exceeds 100 ‰ or the
time ratio exceeds 2.0. The ratio alone cannot be the guard: the same final build gave 64 KiB
1.24, 1.45, 1.60 and 64 B 1.36, 1.42, 1.49, 1.69 in successive runs, overlapping the original
1.75 (2 pairs = 4 busy threads on 4 vCPUs, with the second QEMU slot loading the host). The
spin share separates the two by >1.6× on each side. The original numbers come from the
ipcfast2 measurements on the same counters; the guard was not re-run on the original tree.
`/bin/scmtest stress` (also in the default `scmtest` as `unix_concurrent_stress`) hammers
concurrent senders/receivers with fd passing.

## Tests (aarch64, final build)
scmtest (default, incl. fd_pass_mounted_closed + stress), scmtest stress, pairbench, nettest,
epolltest, polltest, `schedstress full`, sshsmoke: all PASS. regress: 45/45 PASS.

## Open
- The remaining 2-pair cost is not lock spin: the next suspects are cache-line bouncing on
  UNIX_CONNS/SOCK_TABLES lock words and the RUN_QUEUE wake path (`/proc/rqhold` not profiled).
- 64 KiB/seqpacket still spin ~5 %: a ring that drains is freed and re-allocated each message
  (`shrink_if_drained` → `grow` under UNIX_CONNS); hysteresis would remove it.
- Seqpacket double copy / zero-fill (`data.resize(at+len, 0)`) untouched.
