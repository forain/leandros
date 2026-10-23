# lane/dhcpretry — L5.13 "eth0 never gets a DHCP lease" (2026-10-23)

Verdict: FIXED (test infrastructure); guest DHCP client REFUTED as the cause.

## Evidence
- All 247 vmnet runs in /tmp/ffsession scanned: exactly 4 lacked a lease
  (bomanet 18:58, nb1 19:10, irq23reg 19:33, f2fsmisc 19:36) — every run in
  the 18:56–19:37 window, none before or after (dozens on both sides,
  same commits green later: nb2..nb5). Not "1 in 4": a host outage window.
- All 4 show zero RX for the whole run (no "[NET] first RX interrupt"),
  although netd polls the virtio RX ring every pass (100 Hz idle) and
  smoltcp's dhcpv4 socket retransmits DISCOVER. Nothing reached the guest.
- socket_vmnet 1.2.2 relays each frame to every client with a blocking writev
  from one thread. Host-only repro (scratch hol.py, now vmnethealth.py
  --selftest): a reader saw 3000/3000 broadcasts and 50/50 probes normally;
  with one extra client that never reads, it saw 7/3000 and 0/50 probes, and
  all 3010 arrived the moment the staller disconnected. One stalled VM
  (SIGSTOPped/paused QEMU, or a hung guest whose RX ring is full so QEMU stops
  draining its socket) deafens every VM on the Mac.

## Fix
- `.claude/skills/run-leandros/vmnethealth.py`: probe (reader + 3 host
  broadcasts on 192.168.105.255, 1.5 s) and stopped-QEMU listing; `--selftest`.
- driver.py `_socket_vmnet_prefix` probes once per process; wedged ⇒ warning
  naming stopped QEMUs, and the run uses SLIRP instead of failing ~6 suites.
  `LEANDROS_VMNET_PROBE=0` disables.
- runtests suite `vmnethealth` (host selftest, not in regress — it deafens
  all vmnet VMs for ~2 s): wedge_detected + recovers PASS.

## Open
- Which VM held the bridge 18:56–19:37 is unknown (no timestamps in the
  socket_vmnet log). run-qemu.sh (interactive) does not probe.
