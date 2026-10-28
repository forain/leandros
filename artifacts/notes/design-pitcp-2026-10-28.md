# design pitcp (2026-10-28): Pi 5 TCP bulk send ~1.3 MB/s and stalls

Scope: diagnosis + design only. Input: artifacts/notes/lane-pigem-bulk-2026-10-27.md
(NIC ruled out: inflight 0, desync 0). Static read of main e590b436.

## Where things live

- smoltcp is NOT at vendor/smoltcp: it is `servers/net/vendor/smoltcp` (0.11.0 +
  LEANDROS patches), wired by `[patch.crates-io]` in `Cargo.toml:48-52`;
  `servers/net/Cargo.toml:13` asks for `smoltcp = "0.11"`. Patch log:
  `servers/net/vendor/smoltcp/LEANDROS-PATCH.md` ("Not done: congestion control
  (0.12 has Reno/CUBIC), SACK-based recovery").
- TCP sender: `servers/net/vendor/smoltcp/src/socket/tcp.rs`
  - `seq_to_transmit` 2275-2336 (window = min(remote_win_len, tx_buffer.len()); Nagle at 2326)
  - `dispatch` 2366+; RTO = go-back-N rewind to SND.UNA 2416-2431; segment size 2531-2553
  - ACK processing / dup-ACK / partial ACK (NewReno) 2100-2165
  - post-send timer handling 2664-2697; `Timer::set_for_retransmit` doubling 403-418
  - RTO min 200 ms, max 10 s: 212-216; `poll_at` 2713-2750
- netd: `servers/net/src/lib.rs`
  - socket buffers: TCP_TX_BUF 256 KiB (2431) — the observed tx_queue 0x40000 is exactly
    a FULL send buffer, so sshd is not the limiter during a stall.
  - pass loop / sleep cadence 3532-3600: `irq_driven` = ALL netifs `rx_irq_driven()`;
    active (traffic in last 200 ms) and not irq-driven -> 1 kHz; idle not irq-driven -> 100 Hz;
    irq-driven -> up to 1 s, woken by kicks.
  - `NicDevice::transmit` backpressure 2834-2846 (tx_ready), MTU 1514 (2853).
- GEM: `drivers/src/macb.rs` — on the Pi the GEM is POLLED (the lane note's fix2 issued
  8654 TSTART re-rings, and fix2 only runs when `!IRQ_ACTIVE`, macb.rs:737). So on the Pi
  ACKs are only seen at the netd pass cadence (1 kHz while active). brcmf claims
  `rx_irq_driven() == RUNNING` (brcmf/netdev.rs:293) but `all()` is false because of GEM.
- Counters already present: `/proc/net/nic` lines `netd_why:` (passes, immediate, timer
  sleeps, active) and `tcp_recovery: fast_retransmit rto partial_ack dup_acks`
  (servers/net/src/procnet.rs:118-147). The lane note did NOT record them — first thing to grab.

## What the numbers say

- Stall: Pi sends ~40 seg/s, receives ~17 ACK/s => ~2.35 segments per ACK, i.e. the Mac is
  ACKing every other in-order segment (dup ACKs for out-of-order data would be ~1:1).
  60 KB/s with ~17 rounds/s => ~3.5 KB per round => the sender has only ~2-3 MSS in flight
  per round although 256 KiB are queued and the Mac's receive queue is empty.
- Steady state 1.3-1.4 MB/s on BOTH eth0 (1 Gbit) and wifi0 => link-independent, i.e.
  ~1 MSS per millisecond, the same as "~1 segment per 1 kHz netd pass" or "~4 KB per
  ~3 ms round". Both readings say: **the sender's effective flight is a few MSS, not the
  window**. A 256 KiB flight at a 1 ms pass cadence would give >100 MB/s.
- Only eth0 stalls; wifi0 (air-paced, slower bursts) does not. Ping 1 Hz made a stall
  more likely. => the stall is a loss/recovery-state transition that eth0's line-rate
  bursts trigger.

## Ranked hypotheses

H1 (most likely, explains stall) — **Sender stuck in a degenerate recovery state with no
congestion control.** smoltcp 0.11 has no cwnd: after any loss on eth0 (a 256 KiB burst
at 1 Gbit into the Mac's bridge101/adapter queue) recovery is (a) NewReno one-hole-per-RTT
without SACK (tcp.rs:2154-2160), every partial ACK forces `Timer::FastRetransmit`, and
(b) on RTO a go-back-N rewind to SND.UNA (tcp.rs:2420-2431) that re-blasts the whole
window, re-overflowing the same queue; and `set_for_retransmit` doubles `delay` each time
it is re-armed after expiry (tcp.rs:411-416) up to 10 s. Fits: eth-only stalls, long stall
durations, ping sensitivity, Mac recv-q 0 (data parked behind a hole). Weak spot: the 2.35
seg/ACK ratio looks in-order. Decisive: `tcp_recovery` rto/partial_ack climbing during a
stall, Timer=Retransmit with delay >> 200 ms, or `recover_seq` set for seconds.

H2 (most likely, explains the steady 1.3 MB/s cap) — **Flight limited to a few MSS by
the send-side window bookkeeping**: `remote_win_len` (Mac's window as seen by the Pi)
small or `remote_last_seq` repeatedly rewound to SND.UNA by the FastRetransmit/partial-ACK
path so new data never goes far past SND.UNA. Code for window scale looks right
(tcp.rs:1914, 2061-2068; wire/tcp.rs:868), so the snapshot must show it, not assume it.
Decisive: snapshot `snd_wnd` vs `snd_nxt - snd_una`.

H3 — **netd pass cadence × small flight.** GEM is polled, so the ACK clock is the 1 kHz
pass (lib.rs:3550-3553). This multiplies H2 (1 MSS per pass = 1.4 MB/s) but cannot by
itself cap a large flight. 40 seg/s does not match any fixed period in the tree (no 25 Hz
timer found); it matches the Mac's delayed-ACK clock with a 2-3 MSS flight. Decisive:
`netd_why` passes/s and segments-per-pass histogram.

H4 — **sshd refill / producer** (`head | sshd` pipe, cipher). Cannot explain a stall
(send buffer is full then) but could cap the steady state. Decisive: sshd-free bulk (nc /
tcpblast) and `send_queue` sampled during a GOOD run (low => producer-limited).

H5 — Nagle/delayed-ACK. Nagle only holds sub-MSS segments with data in flight
(tcp.rs:2326); a full 256 KiB buffer always yields full segments. Only relevant if H2
shows the window is not a multiple of MSS (window 2-4 KB + Nagle + Mac delayed ACK =
the stall clock). Low.

H6 — Retransmission storm on the wire. Falls under H1; the `tcp_recovery` counters
and tx/rx packet counters settle it.

Ruled out: NIC (lane note); MSS 536 (remote_mss taken from the SYN, tcp.rs:1898-1947;
local MSS 1460 from MTU 1514); window scale parsing (code read).

## Diagnostic plan (guest-side, no tcpdump needed)

D0. Reproduce in QEMU first (cheap): aarch64 guest -> host bulk over the user/tap NIC with
`ssh guest 'head -c 200000000 /dev/zero' > /dev/null`. If QEMU also caps near 1-2 MB/s the
fix iterates locally and the Pi only confirms. QEMU NICs are irq-driven, so a much higher
QEMU number points at H3 (polled GEM) as a multiplier.

D1. Sender snapshot accessor (vendored smoltcp, LEANDROS-marked):

```rust
// socket/tcp.rs
#[derive(Debug, Clone, Copy)]
pub struct SenderSnapshot {
    pub snd_una: u32, pub snd_nxt: u32,          // local_seq_no, remote_last_seq
    pub snd_wnd: u32, pub wscale: i8,            // remote_win_len, remote_win_scale (-1 none)
    pub mss: u16, pub tx_queued: u32, pub nagle: bool,
    pub timer: u8,                               // 0 idle 1 rto 2 fast-rtx 3 close
    pub timer_in_ms: i32, pub timer_delay_ms: u32,
    pub srtt_ms: u32, pub rttvar_ms: u32, pub rto_ms: u32, pub rto_count: u8,
    pub dup_acks: u8, pub recover: Option<u32>,  // recover_seq - snd_una
    pub cwnd: u32, pub ssthresh: u32,            // 0 until CC lands
}
impl Socket<'_> { pub fn sender_snapshot(&self, now: Instant) -> SenderSnapshot { .. } }
```

Plus per-socket counters on the socket: segs_sent, segs_retrans, bytes_acked, acks_in,
dupacks_in, rto_fired, fast_rtx, partial_acks.

D2. netd: `/proc/net/tcpsnd` — one line per established TCP socket:
`<local>-<remote> una nxt flight wnd ws mss q timer t_ms delay srtt rto rc dup rec cwnd ssth sent rtx acks`.
And in `netd_why`: per-pass histogram of segments emitted (0/1/2-7/8-63/64+) and ACKs ingested.

D3. Capture recipe (in-guest, 10 Hz): `while :; do cat /proc/net/tcpsnd; grep -E 'netd_why|tcp_recovery' /proc/net/nic; sleep 0.1; done > /tmp/snd.log &`
then run the bulk; pull the log. Read it as:
- flight ~ snd_wnd and snd_wnd small -> H2/window (check ws == -1, Mac SYN)
- flight << snd_wnd, timer=rto/fast-rtx, rec set, rtx climbing -> H1
- flight << snd_wnd, timer idle, segments/pass = 1 -> H3/egress path (device Exhausted, neighbor_missing)
- q low during good phases -> H4

## Fix (intended diff)

F1 (H1, needed for >=30 MB/s regardless): congestion control in the vendored 0.11,
ported from smoltcp 0.12's `socket/tcp/congestion/{reno,cubic}.rs` (the API is small:
`window()`, `on_ack(now,len,rtt)`, `on_retransmit(now)`, `on_duplicate_ack(now)`,
`pre_transmit(now)`, `post_transmit(now,len)`, `set_mss`).

```diff
 // socket/tcp.rs
+    congestion: congestion::AnyController,   // LEANDROS: Reno default (tcp.cc knob)
@@ seq_to_transmit
-        let max_send_seq =
-            self.local_seq_no + core::cmp::min(self.remote_win_len, self.tx_buffer.len());
+        let win = self.remote_win_len.min(self.congestion.window());
+        let max_send_seq = self.local_seq_no + core::cmp::min(win, self.tx_buffer.len());
@@ dispatch, segment size (2531)
-                let win_right_edge = self.local_seq_no + self.remote_win_len;
+                let win_right_edge = self.local_seq_no
+                    + self.remote_win_len.min(self.congestion.window());
@@ RTO branch (2420)
                 self.remote_last_seq = self.local_seq_no;
+                self.congestion.on_retransmit(cx.now());   // cwnd = 1 MSS: no full-window re-blast
@@ third dup ACK (2117)
                     if self.local_rx_dup_acks == 3 {
+                        self.congestion.on_duplicate_ack(cx.now()); // ssthresh = flight/2
@@ ACK of new data (2146)
             if ack_len > 0 {
+                self.congestion.on_ack(cx.now(), ack_len, &self.rtte);
@@ after send (2687)
+        self.congestion.post_transmit(cx.now(), repr.segment_len());
@@ on SYN / SYN-ACK MSS learnt (1903, 1947)
+        self.congestion.set_mss(self.remote_mss);
```
Initial window 10 MSS (RFC 6928), Reno: slow start then +1 MSS/RTT, halve on 3 dupacks,
1 MSS on RTO. Also cap the RTO timer backoff at the estimator's rto*2^n with the 10 s max
(already) and reset `delay` to the estimator value on any new-data ACK (already done at 2146).

F2 (H2, only if the snapshot shows it): whatever the snapshot names — e.g. wscale lost on
the accept path (fix in `listen.rs` / SYN processing), or the FastRetransmit rewind not
restoring `remote_last_seq` (2668-2676). No speculative diff.

F3 (H3 multiplier): make the GEM RX IRQ-driven on the Pi (RP1 IRQ 6 / MSI-X) like r8169's
NAPI pattern (ISR masks RX + kicks, `rx_rearm` unmasks), so ACKs start a pass at once;
until then keep 1 kHz active polling. Separate lane if D3 shows passes are the clock.

F4 (H4): if producer-limited, look at sshd's write size and the pipe; out of this lane.

Later (not needed for 30 MB/s on a clean LAN): SACK-based recovery (RFC 6675).

## Knobs (kernel cmdline, read once into atomics in servers/net)

- `tcp.cc=none|reno|cubic` (default reno after the fix; `none` = today's behaviour for A/B)
- `tcp.iw=<segments>` initial cwnd, default 10
- `tcp.rto_min_ms=<ms>` default 200
- `tcp.sndbuf_kb=<k>` default 256 (try 1024 for >=30 MB/s at higher RTT)
- `net.cadence_us=<us>` active polled cadence, default 1000

## Test plan (fast-worker)

1. Build release rpi5 + aarch64 (`./scripts/build-all.sh`). Host unit tests for the vendored
   crate as in LEANDROS-PATCH.md (copy with crates.io Cargo.toml, `cargo test --lib socket::tcp`):
   add tests for cwnd growth, halving on 3 dupacks, 1 MSS after RTO with no full re-blast.
2. QEMU aarch64 (D0): bulk 200 MB guest->host, `tcp.cc=none` vs `tcp.cc=reno`; regress
   net tests (nettest) stay green.
3. Pi: `python3 scripts/remote.py pi-deploy --kernel <rpi5 build> --append "pcie.rpi5=1"` then
   `python3 scripts/remote.py pi-boot trial --append "pcie.rpi5=1 tcp.cc=none"` -> run D3
   capture + `ssh pi 'head -c 200000000 /dev/zero' | pv > /dev/null` on the Mac (x3), eth0
   then wifi0. Repeat with `tcp.cc=reno`, then `tcp.cc=cubic`, then `tcp.sndbuf_kb=1024`.
   Pull /tmp/snd.log each run (`remote.py pull`). Also one sshd-free run (nc to Mac `nc -l`)
   to separate H4. Promote with `pi-promote` only after the target is met.
4. Both arches in QEMU once on the merge batch.

## Expected results

- D3 with `tcp.cc=none` shows the stall as H1 (timer=rto, delay climbing, rtx >> 0) and/or
  flight of 2-3 MSS (H2). If it shows neither, H3/H4 are next and F3/F4 apply.
- After F1 (+F2/F3 as indicated): eth0 >= 30 MB/s sustained for 200 MB, 0 stalls in 3 runs,
  `tcp_recovery rto` near 0; wifi0 >= 5 MB/s. If eth0 plateaus at ~10-15 MB/s with
  flight == cwnd, the pass cadence (F3) or sshd cipher cost (H4) is the next ceiling.
