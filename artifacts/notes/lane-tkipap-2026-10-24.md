# lane/tkipap — 2026-10-24 (L6.6 HW half) — HANDOFF: own TKIP test AP is not possible on the box as is

Goal: run our own WPA/WPA2 mixed AP (pairwise CCMP, group TKIP, `wpa_group_rekey=60`) on the KVM
box's wlan0 and join it from the laptop to prove the TKIP group key on real 8265 hardware.
No code changed. L6.6 "HW TKIP join" stays OPEN.

## Premise refuted: the box's wlan0 is NOT unused
- wlan0 (MT7921K, mt7921e, kernel 7.2.7-arch1-1) is the box's ONLY uplink: it holds 172.16.158.150/16
  and the default route, connected to "Anvara 5G" on ch 40 / 80 MHz. eno1 has no address. Taking
  wlan0 into AP mode would cut SSH/tailscale to the box, so it was not done.
- `iw list`: AP is supported, and `#{ managed } <= 2, #{ AP } <= 1, #channels <= 1` is advertised, so
  a concurrent virtual AP (`ap0`, same channel as the STA) was tried instead.

## What was tried (all on ap0, wlan0 untouched; dead-man watchdog was armed)
- hostapd: not installable (`pacman -S hostapd`: mirror retrieval failed with a stale db, and
  `-Sy` = partial upgrade was not done). Built hostapd 2.11 from w1.fi in /tmp/tkipap
  (`CONFIG_DRIVER_NL80211 CONFIG_LIBNL32 CONFIG_IEEE80211AC`, TKIP enabled). No package installed.
- Configs: (1) mixed `wpa=3`, `wpa_pairwise=TKIP CCMP`, `rsn_pairwise=CCMP TKIP`, 20 MHz no-HT;
  (2) the same at HT40-/VHT80 matching the STA chandef (center 5210); (3) plain WPA2-CCMP control.
  hostapd_cli confirmed `group_cipher=TKIP`, state ENABLED, `AP-ENABLED`, all three.
- mt76 debugfs runtime-pm / deep-sleep set to 0 during the test (restored to 1).
- Result: the BSS is not on the air. The laptop's fresh scan (18 BSSs on ch 40 etc.) gave
  `connection to 'leandros-tkip-test' failed: no-ap (not found in a fresh scan)`. A fresh CoreWLAN scan
  on the Mac does not list it either (it lists Anvara 5G and VIVOFIBRA on ch 40). hostapd -d logged
  ZERO received management frames (no probe requests) over about 15 min, for every config including the
  CCMP-only control. So the cipher is not the cause. MT7921 STA+AP single-channel concurrency does not
  beacon or receive on this firmware (WM build 20260224) / kernel.

## Side facts
- The laptop was on "Anvara" (2.4 GHz, prio 10), not "Anvara 5G", at the start. It was left there,
  at 172.16.149.179 with the default route unchanged. The test profile was saved `--no-auto` and then
  forgotten, so the user's 2 profiles are untouched (generation 3). The PSK file was deleted on all hosts.
- LeandrOS `netctl scan` while associated returns cached results (scan generation stays 1, and klog shows
  no new scan). Only a connect's directed fresh scan rescans. That is minor, but it is why scans looked
  identical.
- The user's own mixed-mode SSID LEANDRO2026 is visible from the Mac (ch 5, -70 dBm, WPA/WPA2 mixed).
- Box cleanup: hostapd, dnsmasq, monitor and watchdog were killed by PID, ap0 deleted, PM restored.
  wlan0 was left UP because it was UP at the start as the primary link. The hostapd 2.11 build stays in
  `/tmp/tkipap/hostapd-2.11` on the box for reuse.
- Reusable scripts: `artifacts/notes/tkipap/` (up.sh mixed|wpa1, down.sh, watchdog.sh, cleanup.sh,
  boxmon.sh = ARP-flush + ping every 15 s across rekeys, tkipjoin.sh = laptop window with auto-revert,
  scan.swift = Mac CoreWLAN scan). They need the PSK in /tmp/tkipap/psk and /root/tkip.psk (mode 600).

## To close the HW half (needs a user decision)
1. The user gives the LEANDRO2026 PSK (simplest: a real mixed-mode router), OR
2. A USB Wi-Fi dongle on the box (AP on its own phy, wlan0 untouched), OR
3. The user allows a cable on eno1 as the box uplink for a short window, then hostapd runs on wlan0 itself.
Then run up.sh mixed, then tkipjoin.sh, and check klog for `rx: TKIP Michael MIC failure` (that would mean
the ICV-in-frame assumption is wrong), ARP/ping both ways in mon.log, and >= 2 rekeys in hostapd -d.
