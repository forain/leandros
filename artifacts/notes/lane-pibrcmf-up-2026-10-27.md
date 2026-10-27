# lane pibrcmf-up (2026-10-27): Pi 5 Wi-Fi "up but never associates" — observability, live ctl, wifiscan, MAC after kexec

Status: PARTIAL by design. Three causes FIXED (proven from code); the no-association cause is not
provable without the boot log, which the next Pi boot now shows without serial capture or grep.

## Pi report (main e2c6694e, serial kexec)
wifi0 state up, carrier down, MAC = nvram placeholder b8:27:eb:74:f2:6c, `wifiscan` = "no Wi-Fi device",
`/etc/brcmf.ctl` ignored, `[brcmf]` boot lines lost (serial busy with the transfer), no ESP so no `espctl klog`.

## Findings
- FIXED wifiscan: `/proc/net/wifi` = `iwlwifi::scan_lines`, which knew iwlwifi and wifi_sim only; with
  no Intel device it printed "no Wi-Fi device". Now it shows the brcmf manager's BSS cache.
- FIXED ctl dead while up: `brcmf_boot::run` polled `/etc/brcmf.ctl` only in its failure loop and
  `hand_off` never returned. Now `netdev::run_ctl` services it (1 s) in every state: status, scan, join
  [ssid=..], diag [lines=N], retry [knobs]. retry while up winds `wm::run` down (one unreleasable
  kill-switch pulse), hands the bus back, and brcmf_boot re-probes (WL_REG_ON cycle) as before.
  If BCDC init fails, the thread now parks answering the ctl instead of exiting.
- FIXED (cause proven, source unverified) placeholder MAC after kexec: `kexec/info.rs` hands the next
  kernel a synthetic FDT (`fdt_build`) with no sdio2 `wifi@1` node, so `fdt_fmac_mac` is always None
  after a serial kexec; only the VC-mailbox OTP tag was left. New last fallback: `brcmf.mac=` or the
  Pi firmware's `smsc95xx.macaddr=` + 1 (survives `kexec --reuse-cmdline`). Order: dtb, otp, cmdline.
  The cur_etheraddr set happens before WLC_UP (brcmf_boot, before netdev init) — order was right.
- NOT PROVEN: no association. Init did run to WLC_UP (state "up" is only set after `Brcmf::init` Ok,
  which ends with WLC_UP + SET_INFRA); autoconnect does read /etc/leandros/wifi.conf (legacy_fallback,
  logs the SSID). escan params layout re-checked against brcmfmac (72-byte fixed part, SSIDs at 72,
  n_ssids<<16) — matches. Candidates left: no ESCAN_RESULT events, or results that fail parse_beacon
  (all dropped -> empty cache -> autoconnect never picks). Both are now visible (see below).
- Side fix: firmware.rs no longer prints a `[FW] ... crc32` line for /etc/* reads (the ctl is now read
  every second; it also kept wifi.conf out of the log).

## Observability
- `/proc/klog`: newest 256 KiB of the kernel RAM log ring (`klog::tail`). `/bin/dmesg [text]` filters
  with a sh loop (no grep needed). New root images only; on the Pi's current root use `cat /proc/klog`.
- `/proc/net/nic` [wifi0]: stage, last_error, firmware (ver), mac_source (+ "BUT the firmware still
  reports another MAC" if the set did not take), last_scan (raw from firmware vs parsed + best),
  last_join (SSID, BSSID, JoinFail + reason), counters (scans/joins/events/bad_events/rx + last 8 events
  type/status/reason), manager state, and the last 40 `[brcmf]` lines with timestamps.

## Tests
- brcmf-tests 58/58 (+2: mac_from_cmdline..., ctl_commands_parse_in_every_state).
- check-rpi5.sh rpi5 + raspi4b clean. aarch64 QEMU: `/bin/dmesg 'netif eth0'` prints the boot line,
  /proc/klog 17 KB, wifiscan rc 0. Box aarch64 full regress: see merge report.

## Next Pi boot should show (cat /proc/net/nic)
`mac_source: <mac> otp` or `... cmdline smsc95xx.macaddr+1` (2c:cf:67:d8:ca:1b for this board), never
the placeholder; `last_scan: ... N BSS from the firmware, M parsed`; `brcmfctl join` answers in the
log section within ~10 s.
