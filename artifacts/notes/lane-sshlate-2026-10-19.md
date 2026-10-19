# lane/sshlate — 2026-10-19

Laptop (EliteBook 840 G5, Intel 8265 on 'Anvara 5G' ch 40, -79 dBm). Items L6.3 (late sshd / slow
SSH) and L6.2 (sparse ping loss, SSH blackout during playback).

## L6.3 late sshd: not reproducible since 10-17; the old "5 min" boots were power-offs

Timeline from every boot in the saved logs (artifacts/remote-logs, 50 boots 10-15 .. 10-19):
kernel `[RTC] epoch seconds` vs sshd.log `listening` / first `connection from`, and the previous
boot's `exec "/bin/reboot"`.

| | value |
|---|---|
| kernel start -> sshd listening | 4-6 s, every boot |
| kernel start -> DHCP bound (wifi0) | within the first 5-10 s klog window, every boot |
| `/bin/reboot` -> new kernel start | 22-27 s (firmware + Limine), 45 reboots since 10-17 |
| `/bin/reboot` -> first SSH connection from the Mac | 31-35 s |
| this lane's 3 deploy reboots | "back after 32 s" each; fresh `ssh true` 0.5 s, multiplexed 0.1 s |

The minute-long cases (10-15/10-16: 300, 306, 393, 938, 1287 s) all follow a `power-off`
([REBOOT] power-off in the klog) or have no reboot command at all: the time is a person powering
the laptop on, not the boot. Nothing to fix in the boot path.

## L6.2 sparse ping loss: ROOT CAUSE smoltcp neighbor cache expiry (fixed)

Baseline (good 682677b88365, idle): 1 Hz pings from the Mac lost 7/300 — seq 43, 103, 163-165,
225, 285: exactly one every 60 s. smoltcp 0.11 fills an ARP entry only from ARP and expires it
60 s later however busy the peer is; the next packet to it is dropped (`NeighborPending`) and an
ARP request sent instead. That packet was the echo reply (or a TCP segment on a busy flow), hence
"lost before our TX path": it never reached the driver.

Fix (`servers/net/vendor/smoltcp`, see LEANDROS-PATCH.md "Lane sshlate"): frames from a
neighbor's MAC refresh its entries; an IPv4 entry expired < 60 s is still used while a rate
limited ARP request refreshes it (Linux NUD_STALE). New `/proc/net/nic` line
`neighbor: unresolved N stale_sends N refresh_requests N refreshed_by_traffic N`.
Host tests: 2 new smoltcp tests (fail on upstream behaviour), 593/593.

Second TX-path drop found under load: replies sent while receiving bypass `tx_ready`, and
iwlwifi's 96-frame cap refused 2916-4679 frames per 3-minute download (`tx_counts capped`), echo
and ARP replies among them. Now only pure TCP ACKs are refused at the cap (`wf::is_pure_tcp_ack`,
host test; iwlwifi tests 135/135); everything else goes to the transport limit.

| run (Mac -> laptop, 1 Hz ping) | lost | notes |
|---|---|---|
| good kernel, idle, 300 s | 7/300 | every 60 s |
| trial (ARP fix), idle, 300 s, twice | 0/300, 0/300 | `unresolved 1` (boot) |
| good kernel, 180 s + bulk ssh download | 2/180 | 5 x 50 MiB |
| ARP fix only, same load (2 runs) | 2/180, 3/180 | capped 2916 / 4679 |
| ARP fix + ACK-only cap, same load (3 runs) | 1, 1, 3 /180 | 15-16 x 50 MiB; nothing dropped on our side |

Remaining load loss (~1 %): no counter on our side moves (RX: no drop, reorder skipped 0 /
timeouts 0, `old` = AP retransmissions of frames we had; TX: busy/notready 0, fail_fw 2 per run).
The download saturates the AP's queue toward us (RTT avg 160 ms, max 0.5-1 s), so the AP
tail-drops ping requests; not fixable here.

SSH blackout during playback: not reproduced. A 1 Hz SSH heartbeat during the bulk downloads never
gapped more than 2.6 s (good kernel) / 1.6 s (lane). Playback itself was not tested (needs a
desktop login with the user's password). The likely mechanism is covered by the ARP fix: every
60 s the router's and the Mac's entries lapsed and packets to them were dropped until ARP came
back; with ARP lost on a loaded link, TCP backed off (RTO doubling) — now stale entries keep
working for 60 s while the refresh is retried. Open: confirm during a real playback.

## Tests

- aarch64 (HVF) regress: all pass except one `udptest udp_broadcast` failure ("1 and 0 copies"),
  then 3/3 pass. Cause: test race in `got()` (udp2cases.c): with `ms = 1` the clock could pass the
  deadline before the first look, so a queued datagram counted as missing. Fixed (always look
  once); udptest 3/3 pass after. net-e2e PASS (nmtest, nmrealtest, netroamtest, udppeertest).
- Laptop: trial kernels sshlate-arp / -cap / -ack each booted (back after 32 s);
  sshlate-ack (319792106a65, ARP fix + ACK-only cap, built on main 9f4f1753) PROMOTED good.
  The previous good 682677b88365 is no longer a slot; factory stays.
