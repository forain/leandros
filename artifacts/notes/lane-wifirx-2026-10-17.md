# lane/wifirx — 2026-10-17

Symptom: on the laptop (EliteBook 840 G5, Intel 8265, legacy 54M association
on 'Anvara' ch 1, -67 dBm), transfers into the laptop crawl (scp 16.7 MB in
93 s) and YouTube starts slowly. Base: local main 3f165a43; laptop ran
`main-3f165a43` during the measurements (read-only, user playing YouTube).

## Measurements on the laptop (read-only)

| | result |
|---|---|
| ssh `cat > /dev/null`, 2 MB Mac -> laptop | 0.13 / 0.15 / 0.18 MB/s (three runs) |
| ssh `head -c` 2 MB laptop -> Mac | 0.05 MB/s |
| ping Mac -> laptop, idle-ish, 20 ms spacing | 4-30 ms, same spread as Mac -> gateway (no power-save pattern) |
| ping Mac -> laptop during YouTube | 15-174 ms, avg 85, 5 % loss |

The Mac side of the upload, from `TCP_CONNECTION_INFO` sampled by an ssh
ProxyCommand relay every 0.5 s: 5 retransmitted packets out of ~1400, cwnd
grows to 1.1 MB, the window we advertise stays at 2 MB (scale 6), and the
smoothed RTT climbs 27 -> 167 -> 387 -> 660 ms while the transfer runs at
~0.2 MB/s. So: no loss, no window limit, a queue that keeps growing in
front of a ~1.6 Mbit/s bottleneck. The laptop stack is not that queue:
`netd max_rx_per_pass` is 26 since boot, `rx_dropped` 0, sshd's socket never
fills (window open). The queue is upstream: the AP's delivery to the laptop
(with YouTube sharing it) runs at ~1.6 Mbit/s.

The laptop's own transmissions are bad on air: per-rate success 10-60 %
even at 1-11 Mb/s (`wlan tx state sr`), retries spread up to 15, while ACKs
from us mostly get through (AP duplicate rate `dup` 3 %). The Mac is on the
same 2.4 GHz channel 1 (802.11ax, -77 dBm). Long frames failing while short
ones pass points at a busy/interfered channel rather than the rate table.
What the AP sends *to* us (its rates, how often it retries) was not
visible: there was no counter. Added now (`rx_quality`, below).

A laptop->Mac transfer put 4.36 MB on the air for 2 MB of payload, but
YouTube's uplink runs alongside, so that ratio alone proves nothing; QEMU
below shows the stack part.

## QEMU: the stack under a Wi-Fi-like link

New test knob `netshape=<if>:<tx_kbit>:<rx_kbit>[:<txq>[:<rxq>[:<delay_ms>[:<loss_permille>]]]]`
(`drivers/src/netshape.rs`): a FIFO each way at the given rate, TX held at
`txq` frames (iwlwifi's 64 backpressure), RX queue with tail drop (an AP's),
one-way delay, random loss. aarch64/HVF, vmnet, peer = macOS TCP,
`netshape=eth0:8000:8000:64:256:2[:loss]`, 4 MB x3 each way over ssh:

| build | loss | up (Mac -> guest) | down (guest -> Mac) | down wire/payload |
|---|---|---|---|---|
| main + netshape | 0 | 0.78 MB/s | 0.90 MB/s | 1.05 |
| main + netshape | 1 % | 0.91 MB/s | **0.51 MB/s** | **1.46** |
| lane (patched smoltcp) | 1 % | 0.85 MB/s | **0.90 MB/s** | **1.07** |
| lane, 20 Mbit/s, 5 ms, 3 % | 3 % | 0.92 MB/s | 1.62 MB/s | 1.13 |
| lane, unshaped | 0 | 40.1 MB/s | 38.7 MB/s | 1.05 |

(1.04-1.05 is ssh/TCP framing; the link is ~0.95 MB/s of payload at 8 Mbit/s.)
Receive side: at the link rate with and without loss, no change needed —
the 2 MB buffer with window scaling, ACK every second segment and SACK of
out-of-order data were already fine. Transmit side: with loss our TCP put
46 % extra on the wire and lost 43 % of the throughput.

## Root cause in the stack (fixed): smoltcp 0.11 go-back-N recovery

`servers/net/vendor/smoltcp` (vendored, `[patch.crates-io]`), see its
`LEANDROS-PATCH.md`. In upstream 0.11:
- a fast retransmit rewinds to SND.UNA and resends the whole window in
  flight (here up to 64 queued frames + what follows);
- the retransmission timer is not restarted by ACKs of new data, so a long
  transfer over a queued link hits the RTO while ACKs still arrive;
- the RTO floor is 10 ms;
- a window-update ACK resets the duplicate-ACK count (a peer that autotunes
  its receive buffer never reached three: every loss went to the RTO).

Fixed: RFC 6298 5.3 timer restart, 200 ms floor, one-segment fast
retransmit with NewReno partial-ACK retransmits (recovery point fixed at
entry), window updates neither count nor reset the run, counters in
`/proc/net/nic` `tcp_recovery:`. Two bugs found on the way, both caught by
the counters and now covered by unit tests: committing the retransmit before
the device accepted the frame (under backpressure it looped: 41 855 "fast
retransmits" and an RTT estimate inflated to a 56 s transfer), and moving
the recovery point on each retransmit (every later ACK looked partial:
3 134 spurious resends). Final run: 134 fast retransmits, 46 partial ACKs,
25 RTOs for ~180 lost frames.

smoltcp unit tests: 582/582 (crates.io Cargo.toml copy, host), 3 new/changed.

## Laptop diagnostics added (iwlwifi)

`/proc/net/nic` (wifi0) `rx_quality:` and a `wlan rx:` line every 60 s:
`data N retry N (P%); rb_max X/256 rb_high N; svc_gap_max N ms; rate 1M:.. 54M:..`
- **retry %**: data frames the AP had to resend. High (>20 %) = the AP's
  frames or our ACKs are lost on the air -> RF/channel (try Anvara 5G,
  `iwlwifi.txant=b`, `btcoex=wifi`).
- **rate**: the AP's rates to us. Mostly 1-11 M = the AP's rate control gave
  up on OFDM for us; mostly 36-54 M with a 1.6 Mbit/s result = airtime is
  the limit (busy channel) -> 5 GHz and HT/A-MPDU.
- **rb_max / rb_high**: the RX ring nearly full at a poll = we drain too late
  (frames dropped in the NIC without an ACK -> AP retries). Then make RX
  interrupt-driven (now polled: driver thread every ~1 ms + netd at 1 kHz).
- **svc_gap_max**: the longest the ring went undrained.

## Not changed, with reasons

- TCP buffers/window: 2 MB RX with scale 6, the Mac saw a 2 MB window.
- netd RX cadence / RX ring (256 x 4 KiB): max 26 frames per pass on the
  laptop, no drops in `rx_eth`; the evidence does not point there. `rb_max`
  will confirm on the next boot.
- Congestion control (smoltcp 0.12 has Reno/CUBIC): the iwlwifi backpressure
  bounds our queue; not needed for the measured problem.

## Next step: HT (802.11n) + A-MPDU (plan, not in this lane)

Legacy caps one AP->laptop frame per channel access with a 54M ceiling.
1. WMM/QoS association (WMM IE in the assoc request, QoS data frames with a
   TID, EDCA from the AP's WMM parameters into MAC_CONTEXT `qos_flags`/ac).
2. HT capabilities IE (HT20, SGI20, 2 streams, RX STBC), MAC_CONTEXT HT
   protection flags, ADD_STA with HT (MIMO, mpdu density, max A-MPDU),
   LQ with HT SISO/MIMO2 rates, rate control over MCS.
3. RX BlockAck: answer the AP's ADDBA request, ADD_STA add_immediate_ba_tid
   + SSN, and a per-TID reorder buffer in the driver (the 8265 single RX
   queue does not reorder). This is the downlink win (AP->laptop A-MPDU).
4. TX aggregation (ADDBA request, SCD agg queue, BA notifications) last.
Also: try Anvara 5G (5 GHz, less contention) once the rx_quality numbers
are in.

## Tests

- aarch64 (HVF): `runtests.py --suite regress` PASS (25 steps),
  `--suite net-e2e` PASS.
- x86_64 (TCG): `--suite regress` PASS, `--suite net-e2e` PASS; shaped
  8 Mbit/s 1 % loss: up 0.90, down 0.87 MB/s, wire/payload 1.06.
- iwlwifi host tests: 82/82.
- Laptop kernel built: `LEANDROS_KERNEL_TAG=wifirx-ee511780` (not deployed).
  After deploying: `cat /proc/net/nic` during an upload -> `rx_quality:` and
  `tcp_recovery:`.
