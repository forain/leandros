# lane/piwifi-up 2026-10-27 — Pi 5 wedge right after wifi0 carrier up

Status: PARTIAL (root cause not hardware-confirmed; most likely cause fixed + made visible).

## Ranked hypotheses
1. FIXED — unbounded bcdc waits under steady RX. `dcmd` (control response) and `wait_credit`
   (tx window) charged time only on passes where `pump()` read 0 frames
   (drivers/src/brcmf/bcdc.rs, old dcmd loop / wait_credit). Before carrier up the firmware
   sends almost nothing, so every wait timed out normally; after carrier up broadcast/multicast
   data frames arrive continuously, so a control request without an answer (wifi_mgr's
   GET_RATE/GET_RSSI right after "connected", DHCP-time iovars) or a window that stays closed
   loops forever on the brcmf kthread with IRQs masked (kthreads are non-preemptive and run
   IRQ-off). Host guards show it: on the old accounting the mock's 1e6-frame limit trips
   ("host wait unbounded under steady RX"); fixed: times out at the budget.
2. FIXED (hardening) — `service()` never opened an IRQ window while frames or TX kept coming
   (msleep only when idle) and drained the whole TXQ per pass. Now cond_resched() every pass,
   max 16 TX frames per pass.
3. FIXED (hardening) — wifi_mgr queried `tx_rate_kbps()` (a firmware dcmd) while holding the
   `mgr.st` spin lock; with (1) that wedges every status reader on other CPUs too.
4. OPEN — firmware trap / bus stall on the first data CMD53 or on flow control: we ignore the
   per-FIFO `flowcontrol` mask when sending data. Now visible via the datapath line + slow log.
5. Not investigated: "Handshake { msg2: true, reason: Some(2) }" on the autoconnect right after
   `netctl save` — looks like a back-to-back join (previous auth still on the AP); manual retry OK.

## Changes (f80a0f4d)
- bcdc: FRAME_COST_US=50 per frame read in a wait, optional real `clock` (kernel: monotonic),
  stats ctl_timeouts/credit_waits/credit_timeouts/max_wait_us, mirrored in `bcdc::dp` atomics.
- netdev: stage marker (tx/poll/rx/ctl/sleep), `datapath:` line in /proc/net/nic,
  `[brcmf] data tx failed: ...` (first error) and `[brcmf] data path slow: <stage> took N ms (...)`
  (any op >= 500 ms, <= 1 line / 5 s).
- tests: 3 new brcmf-tests (69/69); check-rpi5 rpi5 + raspi4b clean; aarch64 regress PASS.

## Next Pi boot should show
- Either a working link (DHCP lease, ARP reachable), or within seconds of carrier up one of:
  `[brcmf] data path slow: ctl took 2500 ms (... ctl_timeouts 1 ...)` (firmware stopped answering),
  `[brcmf] data tx failed: Timeout (... credit_timeouts N seq S max M fc 0x..)` (window/flow control),
  and the shell stays alive; `cat /proc/net/nic` → `datapath: stage ... ` counters.
- If the machine still wedges with no `[brcmf]` line at all, the stall is below bcdc
  (an SDIO MMIO access that never completes / UART path) — next step is a sdhci-level marker.
