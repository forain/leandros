# lane/netidle (L9.9) — 2026-10-23/24 — FIXED

## Problem
netidletest (idle net daemon passes over 20 s, bound 10/s) was red on main (95/s) and flaky on the first
lane cut (7.5/s in one boot, 167.7/s in another) with no kick and no smoltcp-timer sleep to explain it.

## Root cause (REFUTED premise first)
The suspected bare-bit park on NETD_TAG was NOT the cause: the new `early` counter (parks that end before
their deadline with no kick pending) stayed 0 in every run. The extra passes came from the daemon's own
cadence: every frame sent or received armed a 200 ms window of 1 kHz passes (`ACTIVE_NS`, the backstop for
a lost RX interrupt), i.e. ~200 passes per stray broadcast. The idle host LAN delivers 5–24 frames per 20 s
window (varies per boot/host chatter), hence 8–170 passes/s.

## Fix (81be3f96 + 89b4f02c)
- servers/net/src/lib.rs: the 1 kHz window only applies while some NIC is polled; when every NIC's receive
  path kicks (`rx_irq_driven`), the frame itself starts the next pass and `rx_rearm` closes the re-arm race.
- Daemon parks keyed on NETD_KEY (`block_on_poll_prepare_tag`, every kick is a keyed wake); deadline is the
  bounded fallback. Defensive: keyed wakes of other objects sharing the bit no longer end the sleep.
- /proc/net/nic `netd_why`: `early N [early_site file:line under sched.wakestat=1]`, `active N`.
- netidletest prints the virtio RX IRQs taken during the window.
- (89b4f02c, previous cut) 1 s idle cap with IRQ-driven NICs, kicks on inet socket ops, IRQ_ARMED.

## Evidence (aarch64 HVF)
- netidletest 3 separate boots: 1.9/s (24 rx irqs in window), 1.3/s (5), 1.5/s (10) — all PASS, early 0.
  Earlier lane cut with the 1 kHz window, same build otherwise: 8.2/s (early 0, kicked +4) and 167.7/s.
- regress (default suite incl. nettag2test, sshsmoke, wifisimtest): PASS.
- nettest 17.2 / 18.3 s, udptest 21.1 / 21.1 s (main 20.4/20.1, 21.9/22.1) — no throughput loss.
- net-e2e: all PASS (nmrealtest + netroamtest hung in Limine "Loading module" on first try — the known
  pre-kernel own-boot hang — and PASSED on rerun).

## Open
- e1000e/r8169 250 ms link-monitor kthreads -> LSC interrupt (real HW only).
- iwlwifi rx_irq_driven stays false (poll_receive services the device), so wifi keeps the 10 ms / 1 kHz
  cadence; laptop idle on Wi-Fi is not improved by this lane.
- Limine own-boot hang ("Loading module") seen 2x in one net-e2e run (harness/firmware, not this lane).

## After merging main (sched/netctl changes), rebuilt
- netidletest 1.1/s PASS (4th boot). nettest, sshsmoke 5/5, nettag2 9/9 PASS on rerun.
- First post-merge try: nettest tcp_keepalive_probes FAIL (192.168.105.1:53 connect -1) in the same window as
  sshsmoke 0/5 (host->guest port 22 timed out) and nettag2 unbound_epollet (known wake-count flake): the
  host<->guest vmnet path was down for that window; all three passed on rerun. The keepalive_probes case
  is the same one seen once on the first lane cut — watch it (repro: runtests aarch64 <tag> /bin/nettest).
