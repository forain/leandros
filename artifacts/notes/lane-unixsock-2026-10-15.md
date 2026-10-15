# lane/unixsock (2026-10-15): chroottest `chroot_unix_socket` failing on aarch64

## Symptom

On main d7f04ea2 (after the lane/laptop merge), aarch64/HVF failed
`chroot_unix_socket` most of the time with "host connect to the jail's socket
failed (errno 3, ...)". It passed on 9f4f1753 and on x86_64/TCG.

## Root cause: a race in the test, exposed by slower tmpfs lookups

- **errno 3 was not from connect().** The test printed `errno` after the
  cleanup ran (`kill`, `waitpid`, `close`, `umount`, `rm_rf`). The jail child
  had already exited, so `kill(pid, SIGKILL)` set ESRCH. With the errno
  captured at the failing call, every failure reads:
  `host connect to the jail's socket host/rel failed (errno 111, jail child status 0)`.
- **The race is in the jail child.** In `in_jail_unix`, the child connects its
  own socket `c` to `/tmp/rel` and then counts each listener as done at its
  first accept. The rel listener's first accept is `c`, not the host's
  connection. As soon as the host's abs connection arrived, the child had
  "both" accepts and exited 0, closing its listeners. The host's following
  `stat` + `socket` + `connect(rel)` then hit a closed listener and got
  ECONNREFUSED. That answer is correct Linux behaviour; the kernel is not at
  fault.
- **What changed the timing: a9c4743d** (lane/laptop, "tmpfs pool 512 ->
  4096"), `servers/vfs/src/lib.rs` `MAX_TMP_FILES`. Every tmpfs name lookup
  (`tmp_find` etc.) scans the whole pool linearly. An entry is about 2.5 KiB
  (the xattr arena dominates), so a missed name now touches about 10 MiB
  (4096 entries) instead of 1.3 MiB. That made the host's stat/connect
  between the two connects slow enough to lose the race almost every time.

## Evidence (aarch64 HVF, instrumented test, `chroottest -u N`)

| build | result |
|---|---|
| d7f04ea2 as is (pool 4096) | 27/30 FAIL, all `host/rel errno 111, child status 0` |
| d7f04ea2 with `MAX_TMP_FILES = 512` | 0/30 FAIL |
| old child logic + 20 ms host pause (pool 4096) | 20/20 FAIL (deterministic) |
| fix (child waits for the host's connections) | 0/50 with `-u 50`, plus 12/12 full chroottest runs PASS |
| original test, 10 full runs on d7f04ea2 | 9/10 FAIL |

**tmpfs lookup cost.** brush loop of 3000 `test -e /tmp/nope` (a missed
name, i.e. a full pool scan):
- pool 512: 0.105 s
- pool 4096: 0.195 s

That is about +30 us per missed /tmp, /dev/shm or /run lookup on HVF.

## Fix (userland/chroottest/chroottest.c only)

- The jail child waits for 1 accept on abs and 2 on rel (its own `c` plus the
  host's) before it exits.
- The host sleeps 20 ms before its second connect. On the old child logic this
  turns the race into a deterministic failure, so a regression shows up at
  once.
- The real errno is captured at the failing call, and the message names which
  socket failed (abs or rel).
- `chroottest -u N` runs only `chroot_unix_socket`, N times (stress loop).

## Verification

- aarch64 (HVF): `chroottest -u 50` 50/50 PASS. 12 full chroottest runs 12/12 PASS.
  regress + ptytest + privtest all PASS.
- x86_64 (TCG): regress + ptytest + privtest PASS, except one scmtest
  `tcp_connect_refused` flake (see the follow-ups). 8 reruns of
  scmtest + chroottest all PASS.

## Follow-ups (not done here)

- **tmpfs lookup is O(pool) with a cache- and TLB-hostile stride.** Since
  a9c4743d every missed /tmp, /dev/shm or /run name costs about 8x more than
  before. Options:
  - Keep a high-water mark of used slots and scan only up to it. Every
    allocation site must update it; there are about 34.
  - Move the xattr arena out of `TmpFileEntry` into a parallel array, so the
    scanned struct is about 400 B.
  - A name hash index.
- **scmtest `tcp_connect_refused` flake on x86_64/TCG, 1 in 9.** A
  nonblocking connect to a refused loopback port answered ECONNREFUSED at once
  instead of EINPROGRESS, so `SO_ERROR` was then 0. The loopback RST was
  already processed by the time `tcp_connect_status` ran. Pre-existing and
  unrelated to this lane (the lane's diff is userland-only). The kernel should
  either always answer EINPROGRESS for a nonblocking connect, or the test
  should accept an immediate refusal.
