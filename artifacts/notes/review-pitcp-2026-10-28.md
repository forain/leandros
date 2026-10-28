# Review: lane pitcp (merge 4e90b640), 2026-10-28

Scope: `git diff 4e90b640^1 4e90b640`. This review is read-only. Line numbers refer to main 4e90b640.
F = servers/net/vendor/smoltcp/src/socket/tcp.rs, L = servers/net/src/lib.rs.

Verdict: the merge is safe to keep as the default. Loopback is effectively unaffected, and Reno is close to RFC 5681.
CUBIC has two real defects: it has no cwnd validation (finding 1) and it runs a 40-iteration softfloat cbrt on every ACK (finding 2).
Both are cheap to fix and should be fixed before calling CUBIC "done".

x86_64 full regress on the box: see the last section.

## Findings, ranked

### 1. HIGH: cwnd grows without bound while the flow is not cwnd-limited, so CUBIC's loss response becomes a no-op
- Where: `Cc::on_ack` F:53-61 (slow start `cwnd += len.min(mss)` on every ACK); `on_dup3` F:90 (`ssthresh = cwnd*7/10`). There is no clamp at F:2448 or F:2687.
- Scenario: slow start ends only on loss (`ssthresh = usize::MAX/2`). A flow limited by the send buffer (256 KiB), by rwnd, or by the application (ssh, HTTP keep-alive, Firefox) keeps adding MSS per ACK. The first loss then sets `cwnd = 0.7 * (inflated cwnd)`, which is still far above the flight, so nothing is throttled. That is the `cc=none` behaviour (full 256 KiB bursts) that caused the Pi stall. It needs about 15 loss events to unwind. It bites on links where 256 KiB bursts are fine at first and then become lossy: the laptop's iwlwifi, and e1000e behind a congested switch. Reno is protected because it uses `flight/2`. On the Pi the first loss came early (cwnd was about the flight), which is why the bench was fine.
- smoltcp 0.12 clamps cwnd to rwnd (`.min(self.rwnd)` in `on_ack`/`pre_transmit`). This port dropped that.
- Fix: grow cwnd only when it is the limit (RFC 7661 style: `flight + mss >= cwnd` when the ACK arrives, or track `max_flight` in the last RTT). Also clamp `cwnd <= max(remote_win_len, tx_buffer.capacity())`. In the CUBIC branches of `on_dup3` and `on_rto`, use `w_max = max(flight, 2mss)` and `ssthresh = flight*7/10`; RFC 9438 §4.6 permits flight_size.

### 2. MEDIUM (perf): CUBIC runs a softfloat f64 cbrt with 40 Newton iterations on every congestion-avoidance ACK, inside netd with IRQs masked
- Where: `cbrt` F:34-39, called at F:69 for every ACK once `cwnd >= ssthresh`.
- Facts: netd is linked into the kernel. Both kernel targets are softfloat: targets/x86_64-unknown-kernel.json `+soft-float,-sse*` and targets/aarch64-unknown-kernel.json `-neon,-fp-armv8`. Every f64 op is a compiler-rt call, and each ACK does about 80 `__divdf3` plus about 150 mul/add/sub.
- Correctness is fine. The iteration `g = (2g + x/g²)/3` is right, x <= 0 is guarded, the float-to-usize cast saturates, and 40 iterations from x/3 converge for any x below 1e11 segments.
- Cost (estimated, not measured): about 5 µs per ACK on the Pi's A76. At 29 MB/s (about 10k ACKs/s) that is 5-7 % of a core, spent in the IRQ-masked netd pass. On gigabit e1000e it is about 40k ACKs/s.
- Fix: compute K once per epoch, in `on_dup3`/`on_rto` or when `epoch` is first set, and cache it in `Cc`. Better still, go integer like Linux `tcp_cubic`: an integer cubic root with one Newton step, time in units of 2^-10 s, `C*t³` in fixed point, and no f64 at all. Also stop updating `w_max` per ACK (finding 4).

### 3. MEDIUM: on a repeated RTO, ssthresh and w_max collapse to 2 MSS and 1 MSS
- Where: `on_rto` F:94-102, called on every RTO firing (F:2569).
- Scenario: the first RTO rewinds `remote_last_seq` and sends one MSS. If that segment also times out (Wi-Fi outage, AP roam), `flight` is 1 MSS, so `ssthresh = 2 MSS`, and CUBIC sets `w_max = cwnd = 1 MSS`. RFC 5681 §3.1 says ssthresh is held constant when the segment was already RTO-retransmitted. The result is that after any outage of 2 RTOs or more, Reno crawls up from 2 MSS (about 6 s to reach 50 KB at a 170 ms RTT), and CUBIC loses its w_max memory.
- Fix: in `on_rto`, if `self.rtte.rto_count > 0` (already backed off), set only `cwnd = mss` and leave `ssthresh`/`w_max` alone. Read `rto_count` before `rtte.on_retransmit()`, as the current call order already does.

### 4. MEDIUM: CUBIC deviates from RFC 9438. w_max is overwritten per ACK, and there is no Reno-friendly region and no fast convergence
- Where: F:67 `if self.w_max < self.cwnd { self.w_max = self.cwnd }`, followed by K being recomputed from the moving w_max.
- Effect: in the convex region w_max is dragged up behind cwnd, and K grows with it, so growth stays near the plateau instead of being cubic. RFC 9438 changes W_max only on a congestion event. Also, §4.3 (the Reno-friendly W_est) is missing. On a LAN with sub-millisecond RTT, recovery after a loss takes K seconds (for example K = 3.3 s at W_max = 50 segments), whereas Reno takes about 15 RTTs, i.e. milliseconds. That regression hits exactly the e1000e/QEMU links that now default to CUBIC. Fast convergence (§4.7) is also missing, which is minor. smoltcp 0.12 also lacks W_est, so this is no worse than upstream, but upstream has not deployed CUBIC as a default on LAN links.
- Fix: set w_max only in `on_dup3`/`on_rto`. If `epoch` starts with `cwnd >= w_max` (no loss yet), use K = 0 and w_max = cwnd (RFC 9438 §4.2). Add `W_est` with alpha = 3(1-β)/(1+β) ≈ 0.53 and use `max(W_cubic, W_est)`.

### 5. MEDIUM: no restart after idle (RFC 5681 §4.1) and the CUBIC epoch is not reset on idle
- Where: nothing in `Cc` looks at the time since the last send.
- Scenario: request/response traffic (HTTP keep-alive, ssh, git fetch over a reused connection) on the Pi's eth0. After seconds of idle, the stale cwnd (which can be huge, see finding 1) allows a full `min(rwnd, 256 KiB)` burst. That is the line-rate burst pattern this lane exists to remove. For CUBIC, `t` also counts the idle time, so the first ACK after idle jumps to the far convex region.
- Integer overflow check: `(target - cwnd) as u64 * len` wraps once t - K exceeds about 7 h. Release builds have no overflow-checks (Cargo.toml profile.release), and `.min(len)` bounds it, so this is garbage-but-bounded rather than a panic.
- Fix: record `last_send`. On send, if `now - last_send > rto`, set `cwnd = min(cwnd, IW*mss)`, and for CUBIC shift `epoch` forward by the idle time (Linux `bictcp_cwnd_event(CA_EVENT_TX_START)`).

### 6. LOW-MEDIUM: fast recovery has no inflation and no bounded exit, so the sender stalls for one RTT and then bursts
- Where: `on_dup3` sets `cwnd = ssthresh` with the window measured from snd_una (F:2448/F:2687), and nothing changes on exit (F:2309 `Some(_) => recover_seq = None`).
- Effect: during recovery, flight (about the old cwnd) is greater than cwnd (half of it), so no new data goes out until the full ACK. Then snd_una jumps to `recover`, and up to `ssthresh` bytes leave in one pass at line rate. RFC 6582 §3.2 step 3 suggests `cwnd = min(ssthresh, flight + mss)` on exit to avoid this burst.
- Fix: on a full ACK, apply that min, or add per-dupack inflation and deflation (RFC 5681 §3.2 steps 4-6).

### 7. LOW: after an RTO, `recover_seq = None` (F:2584), so dupacks provoked by the go-back-N resend can trigger `on_dup3` again
- RFC 6582 §3.2 (step 1 after timeout): set `recover` to the highest sequence sent at the RTO, so duplicate ACKs from retransmitted segments do not cause a second reduction.
- Fix: in the RTO branch, set `self.recover_seq = Some(old remote_last_seq)`, capturing it before the rewind. Check that `test_recovery_ends_at_recover_point` still holds.

### 8. LOW: initial window and SYN loss
- `IW = 10*mss` matches RFC 6928 for MSS 1460. On loopback (MSS about 65495) it is 655 KB, more than the 256 KiB sndbuf, which is harmless because the loopback queue cannot lose. RFC 6928 wants IW = 1 MSS if the SYN or SYN-ACK was retransmitted, and that is not done (minor).
- cwnd is byte-granular, so after `*7/10` or the CUBIC `inc` the cwnd tail is sub-MSS. With TCP_NODELAY (ssh, Firefox) that sends small tail segments. It is cosmetic, and Linux counts cwnd in segments.

### 9. LOW: /proc/net/tcpsnd `t_ms` is always positive
- F:1542: `(expires_at - now)` uses smoltcp 0.11's `Instant - Instant`, which returns `Duration` of the absolute difference, so an expired timer shows as positive. Use `expires_at.total_millis() - now.total_millis()`.

### 10. INFO: knobs
- `read_net_knobs` (L:2439-2451): the clamps are sane. `clamp(1,5000)` stays under RTTE_MAX_RTO 10000, so the `clamp` in `retransmission_timeout` cannot panic. An unknown `tcp.cc=` value silently stays CUBIC, so log it.
- `cmdline_word` matches the exact key followed by `=`, so `tcp.cc` does not match `tcp.ccx=`. That is fine.
- `tcp.sndbuf_kb` affects only the first TCP_BIG_SOCKETS sockets, and `SO_SNDBUF` reports the configured value even when `alloc_net_buf` fell back to half. This was pre-existing behaviour.
- The knobs are read at the top of `net_daemon`. Sockets cannot exist earlier, and the CC mode is read per call, so the ordering is fine.

## Checked and OK
- Units: cwnd, ssthresh, flight and w_max are all in bytes. CUBIC converts to segments for K and the target and back (F:68-72). The Reno CA uses an appropriate-byte-counting accumulator (`acked >= cwnd` adds 1 MSS).
- Reno on 3 dupacks uses `ssthresh = max(flight/2, 2mss)` (RFC 5681 eq. 4). On RTO: `cwnd = 1 MSS` with flight taken before the rewind, which is correct for the first RTO. A reduction happens only once per recovery (guarded by `recover_seq.is_none()`), and there is no growth while recovering.
- The send window is `snd_una + min(rwnd, cwnd)` at both sites (F:2448, F:2687), which is consistent.
- Time source: `stack_now()` = `sched::monotonic_ns` (L:3644), which is monotonic. `rtte.rtt` is used as the RTT in `t` (RFC 9438 §4.2).
- Interaction with LEANDROS-PATCH items 1-6 (RTO restart, 200 ms floor which is now `RTO_MIN_MS`, one-segment fast retransmit, NewReno `recover_seq`, the window-update dupack rule, reset): there is no conflict. `reset()` also clears `cc` and `ctr`. One nit: LEANDROS-PATCH.md still says "Not done: congestion control", so it needs a pitcp section.
- Loopback and local TCP (COSMIC/dbus, Firefox localhost, sshd on 127/8): the loopback stack is `syngate::LoopDev`, an unbounded VecDeque with MTU 65535, so there is no loss, no dupacks and no reduction. The 655 KB IW exceeds the 256 KiB sndbuf, and slow start never leaves (ssthresh = MAX/2), so the CUBIC f64 path is never reached. The only change is the per-ACK `on_ack` cost, which is a few integer ops. An RTO on loopback (netd stalled for 200 ms or more) gives cwnd = 64 KB and slow start back out within a few RTTs. Unix sockets do not touch smoltcp. Risk: negligible.

## x86_64 full regress (box, 4e90b640)
PASS: rid rf-4e90b640-x86_64-29799, build 282 s + tests 598 s, 57 suites, 0 bad, 0/1 boots failed (nettest/udptest/net* included; KVM, virtio-net/slirp, so it covers loopback + the QEMU NIC with CUBIC on by default, not real e1000e/iwlwifi line rate). Logs on box /tmp/lrf/rw-4e90b640/logs.
