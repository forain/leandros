# lane gpulock — 2026-10-19

Bug list items L5.2 and L5.6.

## L5.2 — self-deadlock on `VIRTIO_GPU` from the console drain (FIXED)

**Root cause.** Every kernel print stages its line and then calls
`console_drain_outbox`. The drain writes the queued lines through
`write_tagged`, and the `FlushBatch` drop at the end calls `fb_flush`, which
takes `VIRTIO_GPU.lock()`. If the printing CPU already holds `VIRTIO_GPU`, it
spins on its own lock forever. The print can come from the virtio-gpu driver's
own diagnostics, or from an IRQ or tick that runs on top of a holder. The
watchdog then reports `[WDOG] ... spinning for VIRTIO_GPU held by` that same
CPU. This is the case lane kfacil (2026-10-07) recorded and worked around in
`ktest` by logging through the raw UART.

**Fix.** `drivers/src/framebuffer.rs` `fb_flush` checks
`sched::lockwatch::holder(L_VIRTIO_GPU) == this CPU` (`gpu_held_here`). If it
does, the flush marks its rectangle dirty again, sets `FLUSH_PENDING`, counts
`FLUSH_DEFERRED_REENTRY` and returns. The next flush sends the rectangle; so
does `fb_flush_deferred()`, which the iostat task's once-a-second console kick
calls. "Held by this CPU" always means "held by this context or the one it
interrupted": holders never switch away (their spin windows run with
preemption off), so deferring here never drops a flush that could have gone
out.

**Reproducer.** `ktest=gpulock` (kernel/src/ktest.rs) takes `VIRTIO_GPU` and
calls `serial_print_str`, which goes through the framebuffer console path.
- Fix disabled (`if false && gpu_held_here()`), aarch64/HVF: the log stops at
  `gpulock: console line printed under VIRTIO_GPU`. The print never returns,
  and the kthread is stuck spinning.
- Fixed: `gpulock PASS`. The print under the lock took 418 us, and 1
  re-entrant flush was put back and sent later.

## L5.6 — greetd parent→worker datagram > 4092 B (premise partly stale; remainder FIXED)

The 4 KiB-ring premise is stale. Since `1a293783` (2026-09-28) the AF_UNIX
ring grows to `RING_MAX` = 212992, so a 5–200 KB datagram already worked. Two
things were still wrong:
1. A datagram bigger than the ring can ever hold still returned `EAGAIN`, and
   the blocking wrapper retried it forever. It is now `EMSGSIZE` when
   `len > RING_MAX - 32`, which is Linux's `sk_sndbuf - 32` check.
2. (Found on the way.) `sendmsg` on a unix datagram or seqpacket socket sent
   each iovec as its own datagram. `recvmsg` filled its iovecs one datagram at
   a time, so an iovec filled exactly was followed by the next datagram. Both
   now move one record: they gather into, or scatter from, a kernel buffer.

**Test.** scmtest `unix_dgram_sizes` (part of regress) checks five cases:
an 8000 B round trip, a 300000 B send that must be EMSGSIZE on a blocking
socket, a 2-iovec sendmsg that arrives as one datagram, a 2-iovec recvmsg
that takes one datagram, and a full non-blocking queue that is EAGAIN.
The old kernel would hang on the 300000 B send and fail the two iovec cases.

## Open / not done
- SCM_RIGHTS on AF_UNIX datagram sockets: the fds are silently dropped,
  because `unix_stream_end` only accepts streams. Linux delivers them.
- recv/recvmsg `MSG_TRUNC` (the real datagram length, and `msg_flags`) is not
  reported for unix datagrams.
- `SO_SNDBUF` does not change the AF_UNIX limit; it is fixed at Linux's default.
