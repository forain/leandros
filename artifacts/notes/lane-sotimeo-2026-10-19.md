# Lane sotimeo — 2026-10-19

Bug list items L9.1 (SO_RCVTIMEO/SO_SNDTIMEO ignored, P0-grade hang) and
L9.2 (two SYNs for one listener in one poll → second gets RST).

## L9.1 — socket timeouts: FIXED

Root cause: `handle_setsockopt` fell through to `ok_reply()` for
`SO_RCVTIMEO`/`SO_SNDTIMEO` and stored nothing; `getsockopt` answered
`ENOPROTOOPT`; the kernel's blocking loops (`block_until_ready`) had no bound.

- Net server: `SockOpts` gains `rcvtimeo`/`sndtimeo` (ns, 0 = none, 1 ns for a
  negative tv_sec = "do not wait"). setsockopt follows Linux
  `sock_set_timeout` (EINVAL short struct, EDOM bad usec); getsockopt follows
  `sock_get_timeout` ({0,0} = none, prefix for a short buffer). `_OLD` (20/21)
  and `_NEW` (66/67) numbers. Accepted TCP sockets inherit them (whole
  `SockOpts` was already cloned); accepted AF_UNIX ones do not (Linux makes a
  fresh sock there). `net_server::sock_timeo()` for the kernel.
- Kernel: `net_block_timeo()` wraps `block_until_ready` for read/write,
  send*/recv*, splice output and accept: EAGAIN at the deadline, EINTR on a
  signal when a timeout is set (Linux `sock_intr_errno`; untimed waits stay
  ERESTARTSYS → SA_RESTART restarts). The timeout is looked up only after a
  first EAGAIN. TCP connect: SO_SNDTIMEO deadline → EINPROGRESS.
- **Also found and fixed: blocking `accept` never blocked.** `sys_accept`
  returned the server's EAGAIN straight to user space on a blocking listener.
  It now parks like recv (listener poll tag) — bounded by SO_RCVTIMEO.

## L9.2 — two SYNs in one poll: FIXED (first half)

Root cause: one LISTEN socket ("arm") per listener per stack, re-armed only
between polls by `listen::maintain`; smoltcp's poll drains every queued frame,
so SYN #2 met no LISTEN socket → RST. Always hit by back-to-back loopback
connects (both SYNs leave in one egress and return in the same ingress).

Fix: `servers/net/src/syngate.rs`. `SynGate` wraps each stack's device per
pass, passes one pure SYN (SYN, no ACK/RST) per `Interface::poll`, holds the
next and ends ingress; `poll_stack` then runs `maintain` (re-arms) and polls
again, ≤64 re-polls per pass (a SYN still held is dropped = lost segment, the
peer retransmits). The loopback stack uses a local `LoopDev` (same semantics
as `phy::Loopback`) so the gate takes frames without copying.

Left open: "delayed dup SYN can hit LISTEN" (a duplicate SYN of an already
accepted connection reaching the arm when the accepted socket's slot is above
it) — not addressed.

## Tests

`userland/udptest/sotimeo.c` (10 cases, in `udptest` → `regress`):
so_timeo_sockopt, so_rcvtimeo_udp/tcp/unix, so_sndtimeo_unix,
so_rcvtimeo_accept, so_timeo_zero, so_timeo_signal, so_sndtimeo_connect,
tcp_two_syn. All 10 PASS on the Linux desktop's host kernel (x86_64 musl
static build) before running on LeandrOS. aarch64 (HVF), rebased on main
e748a10f: full regress PASS (udptest 41/41). After a rebase onto fb2de205
(run in parallel with other lanes' QEMUs) everything passed except four
wake-count host tests (evherd, pollnarrow, listentag, nettag2). Re-run
alone under the qemu lock, all four PASS: the failures came from host load.

`nettag2` is flaky on main too (pre-existing, not this lane): main 42f5c03b
×3 gave 2 of 3 runs FAIL (tcp_recv, udpany_recv, icmp_recv: "unrelated
traffic woke the waiter", ~20–50k wakes/s). Which case fails changes per
boot and follows the waiter's bloom bit, so it looks like a 64-bit
poll-mask collision with the test's own AF_UNIX/pipe traffic keys rather
than a broadcast.
