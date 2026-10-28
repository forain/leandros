# lane pigem-bulk (2026-10-27/28): RP1 GEM bulk TX stall

Result: PARTIAL. Diagnostics shipped; H1 (tail desync) and H2 (lost TSTART) REFUTED on the real Pi 5. The stall is above the NIC (H3).

Built from main afeaaa22 (+pikexecdma2, rpi5fix). Knobs: `macb.fix1=0` (tail-keyed watchdog, lost-completion resync, drain kick; default on), `macb.fix2=1` (polled TSTART re-ring in poll_receive; default OFF, see below). New /proc/net/nic counters: tx_head/tail/inflight, tx_used_after_tail, tx_ready_false, tstart_rings, tx_restarts, tx_desync, tx_stalls, tx_drain_kicks, tx_tail_age_ms; `txwatch:` serial line; diag every 15 s while traffic moves; netd logs `tx held >2 s`. Host tests: 2 new, all pass.

Hardware (Pi -> Mac over eth0, `head -c N /dev/zero` via ssh; `dd count=` is unusable, it ends at ~1.4 MB on any link):
- fixes on (both): 20 MB stalled at 8.1 MB (90 s timeout). tx_restarts 8654, tx_desync 0, tx_stalls 0.
- fixes off: 20 MB ok in 16.6 s (1.26 MB/s); with 1 Hz ping: stalled at 18.3 MB.
- fix1 only: stalled at 20.5 of 20.9 MB.
- wifi0 control: 20 MB in 15.2 s (1.4 MB/s), no stall.
At a live stall: macb tx_inflight 0, tx_ready_false 0, no txwatch lines, desync 0, yet the Pi ssh socket holds tx_queue 0x40000 (256 KB) and crawls at ~40 pkt/s (~60 KB/s) with rx ~17 ACK/s; Mac recv-q 0 and ping fine. So the ring is drained and healthy; smoltcp/TCP sender (cwnd/RTO/ACK pacing, or ACK loss on RX) is the bottleneck. Next lane: smoltcp tcp_dbg snapshot (cwnd, rto, remote_win, unacked) and tcpdump on the Mac; also throughput is only ~1.3 MB/s on a 1 Gbit link even when it completes.
Fix2 never helped and issues thousands of spurious TSTARTs, so it is opt-in. PCIe->PCIe kexec check: serial showed `[kexec] quiesced: dma40 xhci0 xhci1 gem pcie-perst`, `[pcie] early reset: ... (bridge was already in reset)`, eth0 back every time; no FAILED/never reached/registry busy.
