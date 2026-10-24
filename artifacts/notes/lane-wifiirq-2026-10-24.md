# lane/wifiirq (L9.9 follow-up) — 2026-10-24 — FIXED (partial on the pass-rate target)

## Problem
After netidle the net daemon sleeps to timers only when every NIC kicks on RX. iwlwifi was fully polled
(INTx off, MSI never enabled): the daemon kept 10 ms / 1 kHz, and the driver thread itself polled at 1 kHz
while associated (`monitor_once` -> `wait_notif` -> `msleep(1)`). Laptop idle on Wi-Fi was unimproved.

## Fix (lane/wifiirq, 2 commits)
- drivers/src/iwlwifi.rs: MSI requested at probe (`iwlwifi.irq=0` keeps polling; aarch64/QEMU: no MSI ->
  polled as before). ISR masks CSR_INT_MASK, kicks NETD_KEY and a new driver-thread key (IWL_KEY).
  `rx_rearm` reopens the mask and asks for another pass when rx_eth / the RB ring is non-empty or DEV is held.
  `rx_irq_driven` = MSI live. `service()` hands work to the other side (thread queued data -> kick netd;
  netd queued a notification/response -> kick thread). While the link is up `wait_notif` parks keyed (<=100 ms,
  the monitor's housekeeping budget) instead of msleep(1); bring-up/scan/handshake still poll at 1 ms.
- drivers/src/iwl_trans.rs: ack CSR_FH_INT_STATUS RX bits on FH_RX/SW_RX (left set they re-raise the IRQ),
  IRQ_WANTED (enable/disable_interrupts) so re-arm never reopens a device being reset, `rx_pending()`.
- /proc/net/nic iwlwifi `irq:` line: irqs, empty, last CSR_INT, thread parks/kicked, rearm_busy.

## Evidence (laptop, greeter idle, Wi-Fi only; good = laptop2 kernel c804a453b17a)
| | good | trial 1 | trial 2 (promoted f40d175b8cae) |
|---|---|---|---|
| netd passes/s (20 s) | 490 | 60 | 113 |
| idle total CPU | 8.2 % (iwlwifi 1.9, netd 1.1) | 6.9 % | 6.8 % (iwlwifi 0.9, netd 0.6) |
| SSH heartbeat 300 x 1 Hz | (laptop2 note: max 2.11, 1 gap>2 s) | max 4.07, 36 gaps>2 s | max 1.25, 0 gaps>2 s |
| scp 50 MiB pull / push | 6.9 / 22.7 Mbit/s | 12.0 / 22.2 | 9.4 / 22.0 |
- Trial 1 bug: `rx_rearm` answered "drained" when DEV was held by the driver thread, leaving frames the
  daemon itself had queued in rx_eth until the 1 s idle cap (gaps of exactly 1.07+n s). Fixed in commit 2.
- Wi-Fi associated + DHCP on every boot (all checks ran over Wi-Fi), MSI vector 0x43, no IRQ storm.
- QEMU aarch64 regress (incl. wifisimtest, nettag2test, sshsmoke): see merge log below.

## Open
- netd still ~110 passes/s, not 1-2/s: ~30 handled IRQs/s (LAN chatter + beacon-driven notifications) plus
  ~30 "empty" IRQs/s (CSR_INT already acked by the other servicer) and rearm_busy ~54/s (the driver thread
  holds DEV when the daemon re-arms). Next: let the ISR wake only the daemon, and have the daemon hand
  notifications to the thread (thread drops its own RX servicing while associated), or coalesce via
  CSR_INT_COALESCING; filter beacons/statistics notifications in firmware (beacon filter / power save).
- Driver thread still wakes >=10/s (100 ms monitor budget: send_lq_if_due, poll_tx_agg, agg reorder tick).
