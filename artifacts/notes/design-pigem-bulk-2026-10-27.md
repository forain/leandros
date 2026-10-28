# pigem-bulk: RP1 GEM bulk-TX stall after ~700 KB (design / diagnosis)

Date 2026-10-27. Base: main 1b87d219, real Pi 5, `pcie.rpi5=1`, polled GEM (`irq: polled`, `irqs: 0`).
Symptom: ssh bulk Pi -> Mac stalls after 640-740 KB (twice). Ping still answers afterwards.
No code was changed in this lane, and the real Pi was not touched.

## 0. What the evidence does and does not say

- The "+40 s" diag (`hw tx 42, rx 168, urun/fcs/hresp 0`) is **pre-transfer**. The diag fires only at link-up +10 s and +40 s (`drivers/src/macb.rs:522-528`). 700 KB needs about 480 full-size frames, and 42 is far below that. So it says nothing about the ring state at the stall. A stall-time snapshot is the first thing missing (section 2).
- `piethtest-3.txt` is also an early snapshot (eth0 `tx_packets 4`).
- "Ping keeps working" proves that **both** the TX and the RX path of the GEM still move frames after the stall. A ring that is dead in hardware is ruled out. Any driver explanation must stop **TCP egress only**, and the code has exactly one such path (H1).

## 1. Ring arithmetic (wrap hypothesis)

| ring | descs | desc size | desc memory | buffer | buffer memory | notes |
|---|---|---|---|---|---|---|
| TX | 256 (`macb.rs:77`) | 16 B ADDR64 (`macb_hw.rs:250`) | 4 KiB = 1 page | 2 KiB (`macb.rs:78`) | 512 KiB | max in flight 255 (`slot()`, `macb_hw.rs:427`); `tx_ready` high-water 248 (`macb.rs:658`) |
| RX | 256 (`macb.rs:76`) | 16 B | 4 KiB | 2 KiB, RXBS = 2048/64 (`macb_hw.rs:156`) | 512 KiB | MAX_FRAME 1514 (`macb.rs:79`) |

- One TX lap is 256 frames, about 256 x 1460 = 365 KiB of TCP payload. Two laps are about 731 KiB.
- The stall at 640-740 KB is about 440-510 data frames. Add about 42 frames before the transfer and the ssh setup, and you get about 500-570 posts. That straddles the end of TX lap 2 (512). This is suggestive, not proof: dd counts bytes before ssh buffering, so the wire offset is lower by an unknown 0-200 KB.
- The RX ring sees about 168 + ~240 ACKs. Its first lap ends in the middle of the transfer, not at the stall.
- A ring is a FIFO, so "ring size x MTU" is not a capacity limit. Wrap can only matter if the WRAP/USED handover is wrong. The code is correct there:
  - `post()` re-ORs WRAP into the last slot (`macb_hw.rs:431-438`).
  - `reset()` does the same (`macb_hw.rs:443-452`).
  - RX `release()` writes back `d.addr & !RX_USED`, which keeps WRAP (`macb_hw.rs:383-393`).
  - TX buffer addresses are set once at init and never clobbered.
- **Verdict: plain WRAP-bit mishandling is unlikely.** A lap-correlated hw/sw desync (H1) is still possible.

## 2. Ranked hypotheses

### H1 (about 40%): TX tail stuck, so `tx_ready()` holds all TCP egress, while ping bypasses the gate and also defeats the stall watchdog

- `TxRing::reclaim` stops at the first descriptor without USED (`macb_hw.rs:454-466`). If one completion is never seen (hw/sw desync: the GEM is past slot T but T never shows USED), `tail` freezes. `in_flight` then climbs to 248, and `tx_ready()` returns false (`macb.rs:657-659`).
- netd's `NicDevice::transmit()` returns None while `tx_ready` is false (`servers/net/src/lib.rs:2825-2828`). **All smoltcp socket egress stops**: TCP data, retransmits and window probes.
- The TxToken that comes paired with each **received** frame is not gated (`lib.rs:2814`). ICMP echo replies and ARP replies are built from that token, so they still reach `send_packet` (`macb.rs:632`), which refuses only at 255 in flight. **Ping keeps working.**
- The TxStall watchdog keys on `(hw TBQP slot, tail)` (`macb_hw.rs:331-340`). Every ping reply moves the hardware slot, so the 3-poll counter resets every second and **the watchdog never fires while a 1 Hz ping runs**. With no ping, the queue fills to 255 and the key freezes, so the watchdog would rewind after about 3 s.
- macb never kicks netd when its TX queue drains (the only kick is on link, `macb.rs:596`). netd falls back to 10 ms re-polls (`lib.rs:2743`, `3425-3428`). That is only slow, not fatal.
- Prediction at the stall: `head - tail` is 248-255 and frozen, `stack_tx held` keeps climbing, `tx_full` grows once ping has used up the 7 spare slots, and `TBQP slot != tail`.

### H2 (about 25%): lost TSTART (the GEM "TX used-bit-read" race), with only 1 s recovery in polled mode, compounded by smoltcp RTO backoff

- If TSTART lands while the GEM is still finishing a frame after it read the next descriptor as USED, the controller idles with frames posted. Linux recovers on the TXUBR interrupt (`macb_tx_restart`). Here the GEM is polled: only a later `send_packet` (`macb.rs:651`) or the 1 s driver-thread retry (`macb.rs:564`) restarts it.
- With a full window there are no further sends. Each event costs up to 1 s, smoltcp's RTO doubles up to 10 s (`vendor/smoltcp/src/socket/tcp.rs:216`, `:303`), and the transfer looks dead for long stretches.
- Prediction at the stall: TSR.TGO is clear, `TBQP slot == tail`, and `desc[tail]` has USED clear. Frames move again within 1 s of each driver poll. Also, `rto` in `tcp_recovery` climbs.

### H3 (about 25%): above the NIC (socket writability or TCP sender)

- The NIC demonstrably still works. A missed POLLOUT/EPOLLOUT wakeup for sshd, or a sender-side smoltcp window bug, gives exactly "permanent stall, ping fine".
- Prior art: lanes pollout, epollready2 and piepollnull all touched this area.
- **Discriminator:** `/proc/net/tcp` `tx_queue` for the ssh socket at the stall.
  - Equal to 0 means sshd stopped writing: the wakeup layer, not macb.
  - Greater than 0 and frozen means the stack or the driver (H1/H2).
- Second discriminator: the same transfer over wifi0. If it also stalls, the cause is not macb.

### H4 (about 5%): RX refill under ACK load

- Unlikely. RX `release` is correct, polling runs at 1 kHz while traffic is active (`lib.rs:3532-3537`), `RX_BATCH` is 64 (`lib.rs:2732`), and ping still gets in.
- Worth watching: `rx_used_bit_read` and `ovr`/`res` at the stall.

### H5 (about 3%): cache maintenance on reused buffers

- The default `macb.dma=nc` (`macb.rs:349`) puts the rings in Normal-NC, so the descriptor CMO race in `wr_word` (`macb_hw.rs:292-299`) is inactive.
- Buffers are cleaned on TX (`macb.rs:647`) and invalidated on RX (`macb.rs:673`). Corruption there would cause retransmits, not a permanent stall.
- Rule it out with one A/B run on `macb.dma=dev`.

### Excluded

- **TSO and checksum offload:** TXCOEN is cleared (`macb_hw.rs:154-155`) and there is no LSO. smoltcp sizes data segments with a 20 B header and no options (`tcp.rs:2549-2553`), so the frame is at most 1514 = MAX_FRAME, matching `caps.max_transmission_unit = 1514` (`lib.rs:2837`).
- **IRQ storm or W1C ordering:** the IRQ is not wired. `sample()` only reads and clears ISR for counters (`macb.rs:738-745`).

## 3. Single diagnostic build (one kernel answers H1, H2 and H3)

### macb counters

Add to `drivers/src/macb.rs` statics and expose them in `info_lines`:

```
tx_head / tx_tail / tx_inflight / tx_hw_slot / tx_tgo / tsr      (live, read under RINGS lock)
tx_ready_false   # tx_ready() returned false (netd held)
tstart_rings     # every NCR_TSTART write
tx_restarts      # polled restarts (fix 2; 0 in the diag build)
tx_desync        # reclaim saw desc[tail] !USED while a later slot in [tail, hw_slot) has USED
tx_tail_age_ms   # time since tail last moved with in_flight > 0
```

### macb driver-thread watch line

Rate-limited to 30 lines per boot. Print it when `in_flight > 0` and the tail has not moved for 2 or more polls:

```
[macb] txwatch: head H tail T inflight N hw S tgo G TSR 0x.. full F held R desync D tstart K |
       d[T] CTRL d[T+1] CTRL d[S-1] CTRL d[S] CTRL | used-after-tail FIRST..LAST | rxhead X rxhw Y ubr U
```

`used-after-tail` scans T+1..head for descriptors with USED set. A non-empty range together with `d[T]` lacking USED is the H1 desync signature.

### Periodic diag

While the link is up, repeat `diag_line` every 15 s. Cap it at 20 lines, and skip a line when `TX_PKTS` and `RX_PKTS` have not changed since the last one. This replaces the one-shot +10 s and +40 s lines, so a diag lands during and after the stall.

### netd (servers/net)

- **procnet `tcp_dbg:` line.** One line per TCP socket with `send_queue > 0`: state, send_queue, recv_queue, unacked (`remote_last_seq - local_seq_no`), remote_win_len, remote_mss, rto, timer kind.
  - Add a small `pub fn debug_snapshot()` accessor to the vendored smoltcp `TcpSocket`, marked `// LEANDROS`.
- **netd log line.** `netd: eth0 tx held >2 s (tx_held N)`, emitted once per episode, when `tx_held` increments continuously for 2 s.

## 4. Intended fixes (the top two hypotheses; ship both, they are independent)

### Fix 1 (H1): watchdog keyed on tail progress, desync repair, drain kick

The changes cover `drivers/src/macb_hw.rs` (`TxStall::tick`, `TxRing::reclaim`) and `drivers/src/macb.rs` (`tx_ready`).

```diff
--- a/drivers/src/macb_hw.rs
+++ b/drivers/src/macb_hw.rs
@@ impl TxStall
-    pub fn tick(&mut self, hw_slot: Option<usize>, tail: usize, in_flight: usize, tgo: bool) -> bool {
-        if in_flight == 0 || tgo {
+    /// Keyed on the software tail only: frames posted after a lost completion
+    /// (ping replies through the ungated RX-paired token) move TBQP every poll
+    /// and used to reset the count forever. A 1 Gbit/s queue never keeps the
+    /// same head-of-queue frame for 3 s (PAUSE is off: NCFGR.PAE clear).
+    pub fn tick(&mut self, _hw_slot: Option<usize>, tail: usize, in_flight: usize, _tgo: bool) -> bool {
+        if in_flight == 0 {
             *self = TxStall::default();
             return false;
         }
-        let key = (hw_slot.unwrap_or(usize::MAX), tail);
+        let key = (0, tail);
@@ impl TxRing
+    /// The controller is idle on `hw` and every slot in (tail, hw) it already
+    /// completed (USED), but `tail` itself never came back: its completion was
+    /// lost. Retire tail..hw so the queue does not wedge. Returns slots retired.
+    pub fn resync_to(&mut self, hw: usize) -> usize {
+        let mut k = 0;
+        while self.tail != hw && self.tail != self.head {
+            unsafe { wr_word(self.descs, self.tail, 1, TX_USED | Self::wrap_of(self.n, self.tail)); }
+            self.tail = (self.tail + 1) % self.n; k += 1;
+        }
+        k
+    }
```

```diff
--- a/drivers/src/macb.rs
+++ b/drivers/src/macb.rs
@@ driver_thread, before stall.tick
+            // Lost completion: GEM idle past the tail with later slots done.
+            if !tgo { if let Some(s) = slot { if s != r.tx.tail() && r.tx.desc(r.tx.tail()).ctrl & hw::TX_USED == 0
+                    && (r.tx.tail() + 1..).take(r.tx.in_flight()).any(|i| r.tx.desc(i).ctrl & hw::TX_USED != 0) {
+                TX_DESYNC.fetch_add(1, Ordering::Relaxed);
+                r.tx.resync_to(s);
+                crate::netdev::kick();
+            } } }
@@ pub fn tx_ready()
-    RINGS.lock().as_mut().map(|r| { r.tx.reclaim(); r.tx.in_flight() < NUM_TX - 8 }).unwrap_or(false)
+    let ok = RINGS.lock().as_mut().map(|r| { r.tx.reclaim(); r.tx.in_flight() < NUM_TX - 8 }).unwrap_or(false);
+    if !ok { TX_HELD.store(true, Ordering::Relaxed); TX_READY_FALSE.fetch_add(1, Ordering::Relaxed); }
+    ok
@@ driver_thread after r.tx.reclaim()
+            // Drain kick (iwlwifi TXQ_LOW equivalent): netd parked on tx_ready.
+            if r.tx.in_flight() < NUM_TX / 2 && TX_HELD.swap(false, Ordering::Relaxed) { crate::netdev::kick(); }
```

### Fix 2 (H2): polled TX restart, the equivalent of Linux `macb_tx_restart`, at 1 kHz instead of 1 Hz

```diff
--- a/drivers/src/macb.rs
+++ b/drivers/src/macb.rs
@@ pub fn poll_receive(out: &mut [u8]) -> Option<usize> {
     let r = g.as_mut()?;
     r.tx.reclaim();
+    // Polled GEM has no TXUBR interrupt: a TSTART that lost the race with the
+    // controller's used-bit read leaves frames posted and TX idle. Re-ring when
+    // the queue is idle on the tail (one PCIe read, only with frames queued).
+    if r.tx.in_flight() > 0 && !IRQ_ACTIVE.load(Ordering::Relaxed) {
+        let mut m = Mmio(BASE.load(Ordering::Relaxed));
+        if m.r32(hw::TSR) & hw::TSR_TGO == 0 {
+            wmb(); ncr_set(&mut m, hw::NCR_TSTART, 0);
+            TX_RESTARTS.fetch_add(1, Ordering::Relaxed);
+        }
+    }
```

Optional, behind `macb.txrecover`: also apply fix 2 in `tx_ready()`, so a netd that is only sending (no RX) still restarts the queue.

## 5. Hardware test plan (fast-worker, `scripts/remote.py`, aarch64 rpi5 build)

Use the pi-remote skill flow. The Pi's eth0 is 192.168.2.4 (DHCP; confirm with `remote.py pi-status`). Run diagnostics over **wifi0** so they do not depend on eth0.

### Run A: diagnostic build, no fix

1. Build the release rpi5 kernel with section 3 only. Then:
   ```
   LEANDROS_REMOTE=pi python3 scripts/remote.py pi-deploy --append pcie.rpi5=1
   ```
   This uploads the kernel to `/boot/pi/trial` and kexecs into it. Use `pi-boot trial --append pcie.rpi5=1` for re-runs.
2. From the Mac, with the ssh user and key that `remote.py` uses, run with **no ping**:
   ```
   ssh <user>@192.168.2.4 'dd if=/dev/zero bs=64k count=320 2>/dev/null' | dd of=/dev/null bs=64k
   ```
   That is 20 MB. Wrap it in `timeout 90` and record the bytes and the time.
3. Repeat step 2 with `ping -i 1 192.168.2.4` running in parallel.
4. If either run stalls (no progress for 10 s), collect over wifi0 while it is still stalled:
   - `remote.py run 'cat /proc/net/tcp'`
   - the net info dump (the same source as `piethtest-3.txt`)
   - `remote.py logs`, then `grep -E 'txwatch|tx stalled|diag \+|netd: eth0 tx held'`
5. Control: repeat step 2 against the wifi0 address, if the Mac can reach it.

### How to read run A

| observed at the stall | verdict |
|---|---|
| `tcp_dbg` send_queue > 0, `stack_tx held` climbing, txwatch `inflight >= 248`, `desync >= 1` or `used-after-tail` non-empty; stalls only with ping (or longer with ping) | **H1** |
| txwatch `tgo 0`, `hw == tail`, `d[T]` without USED, short freezes that resume about 1 s after `tstart` moves, rto climbing | **H2** |
| `/proc/net/tcp` tx_queue 0 for the ssh socket, macb inflight 0, wifi0 control also stalls | **H3**: hand off to a socket-wakeup lane, not macb |
| `ovr`/`res`/`rx_used_bit_read` rising, txring healthy | **H4** |

### Run B: diagnostic build plus fix 1 and fix 2

Run B uses a single trial boot. Repeat steps 2 and 3 three times each with 100 MB (`count=1600`). Expected:

- All 6 transfers complete in under 60 s each, at 20 MB/s or more. The cipher-bound ceiling on the Pi is about 40-80 MB/s.
- The dmesg `tx stalled` count is 0 or unchanged. `txwatch` prints nothing, or at most once.
- The info dump shows `tx_desync` equal to the number of episodes (H1 confirmed and repaired) or `tx_restarts` > 0 (H2 confirmed). `tx_ready_false` may be non-zero, but `stack_tx held` stops climbing between transfers.
- `ping -c 20 192.168.2.4` during the transfer shows 0% loss.

### Last check

A/B `macb.dma=nc` against `macb.dma=dev` on run B. Both must pass, which rules out H5.
Before merging, run the both-arch QEMU regress (aarch64 per lane; both arches per merge batch). macb is not present in QEMU, so this only checks that the build and netd are not regressed.
