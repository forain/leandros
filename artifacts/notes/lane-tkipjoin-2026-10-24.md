# lane/tkipjoin — 2026-10-24 (L6.6 HW half) — FIXED: real TKIP-group join works on the laptop

The laptop (EliteBook 840 G5, Intel 8265, good kernel `97ffd47688`, main 97ffd476, includes lane tkip
5d42eeb8) joined the user's real WPA/WPA2 mixed-mode network LEANDRO2026 (14:23:33:fa:5b:e8, 2.4 GHz,
pairwise CCMP, group TKIP). Group-addressed traffic is received and delivered. No code changed.

## Method
- One `lock.sh laptop` hold. PSK pushed to a mode-600 file on the laptop, read by the test script, never
  printed; all pulled logs were grepped for it (0 hits). The copy was deleted afterwards.
- Self-reverting script under nohup: join LEANDRO2026 (`netctl save --no-auto --prio 0`, then connect),
  stay ~180 s, snapshot `/proc/net/wifi`, `/proc/net/dev`, `netctl status`, klog, then reconnect Anvara
  by UUID. A separate nohup fallback reconnects Anvara at +300 s unless it is already there.
- LEANDRO2026 bridges onto the same LAN as Anvara (same DHCP server: same lease 172.16.149.179/16,
  gw 172.16.0.2). So SSH and Mac-side traffic keep working during the window.

## Evidence (5 joins, all successful)
- klog on every join: `WPA/WPA2 mixed mode: pairwise CCMP, group TKIP (receive only)`,
  `group key 1 (TKIP) installed`, `4-way handshake complete`, `connected to 'LEANDRO2026' in 2945 ms`,
  then `DHCP discover restarted` and `DHCP configured, address: 172.16.149.179 (wifi0)` (fresh lease).
- SSH from the Mac over LEANDRO2026 worked mid-window. Mac→laptop unicast ping: 267/280.
- Group RX proof: a broadcast ping from the Mac (`ping 172.16.255.255`, sent from the Anvara side and
  relayed by the LEANDRO2026 AP as group-addressed TKIP frames) was answered by the laptop
  **118/120** times while it was connected to LEANDRO2026. The Anvara (CCMP group) baseline was 3/4.
- RX filter counters across the window: `undecrypted` stayed at 5 (those 5 predate the TKIP windows),
  `replay 0`, no `tkip sw` / `michael fail` (both printed only when nonzero), and no
  `rx: TKIP Michael MIC failure` lines. So every TKIP group frame was decrypted by the firmware
  (the TTAK path). The host SW-decrypt fallback was not exercised.
- No GTK rekey happened in the ~10 min spent on LEANDRO2026 in total (no `group rekey` / key-id change
  in klog). The router's interval is probably the usual 3600 s.

## Cleanup / state
- Laptop is back on "Anvara" (2.4 GHz), at 172.16.149.179, kernel unchanged (`97ffd47688`).
- The LEANDRO2026 profile was REMOVED (`netctl forget`). 2 user profiles remain, untouched (generation 9).
- The PSK copy and the test scripts were deleted on the laptop. No PSK in notes, logs or the cmdline.

## Open / new
- GTK rekey on real hardware is still unobserved. It needs a window longer than the router's rekey
  interval (likely 1 h), or the 65536-frame IV32 roll-over to hit the host SW-decrypt path.
- NEW: the laptop's `/bin/ping` gets 0 replies to every target (1.1.1.1, 172.16.0.254, 172.16.0.2,
  the Mac), even on Anvara, while the Mac→laptop ping and SSH work. It also prints relibc ld_so debug
  lines (`ld_so::init tp=0x0`, `PH type=...`). Repro on the laptop: `ping -c 2 1.1.1.1`, which gives 100% loss.
- Minor: the test script's "back on Anvara" check used ping, so it reconnected Anvara 6 times
  (harmless). Future scripts should test reachability with `netctl status` or TCP instead.
