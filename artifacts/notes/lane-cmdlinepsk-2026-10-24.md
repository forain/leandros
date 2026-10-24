# lane/cmdlinepsk (L7.1, P0 security) — 2026-10-24 — FIXED

## Design
- Consumer of `wifi.psk=` is the in-kernel iwlwifi driver (`load_credentials`, also the
  legacy fallback profile for `iwlwifi.mgr=1`). Root-only `/etc/leandros/wifi.conf`
  (`ssid=`/`psk=`, mode 0600) already won over the cmdline; it is now the only supported source.
- Kernel (`kernel/src/main.rs store_scrubbed_cmdline`) removes every secret-looking knob
  (`cmdline_redact::is_secret_key`) from its stored command line at boot (Limine and direct
  boot) and parks the value in `drivers::cmdline_secrets`; iwlwifi `take()`s `wifi.psk` once
  and logs a DEPRECATED warning. /proc/cmdline, console/klog echo, kexec `--reuse-cmdline`
  and all driver copies never hold it.
- /proc/cmdline redacts secret knobs for ALL readers; root gets the raw line only with
  `cmdline.raw=1`. Image builder stages wifi.conf as 0600.

## Evidence
- psklogtest (aarch64) 5/5: lifted_control, proc_root, proc_user, klog, serial; FAKE `SECRET123-fakepsk`.
  Unfixed-tree run not obtained (main's images did not boot in the harness); the old root path
  wrote the raw line by construction.
- aarch64 regress: overall PASS.
- Laptop: PSK copied in-guest from /proc/cmdline into /etc/leandros/wifi.conf (0600 root, 2 lines).
  Trial abdb6af670d0 (`#1 a32e554da6`, contains main 88ffeee9) with a PSK-free cmdline: 0 secret
  tokens in /proc/cmdline, iwlwifi "credentials from /etc/leandros/wifi.conf: SSID 'Anvara'",
  auto-joined "Anvara 5G", SSH fine. The stored netctl "Anvara" profile had no secret
  (`connect` -> no-secret); saved it in-guest from wifi.conf (`netctl save Anvara`, new uuid
  9cbc2450), then joined "Anvara" 2412 MHz and rejoined "Anvara 5G". PROMOTED: good=abdb6af670d0,
  cmdline contains iwlwifi.mgr=1 and no wifi.* knob. Factory entry remains the fallback.

## Open
- `netctl connect SSID` (explicit) first fails `unsupported-security` for both Anvara SSIDs
  (WPA3 hidden sibling BSSs on the same AP?); the automatic attempt then succeeds.
- Stale secret-less "Anvara" profile cd982814 still stored next to the new one.
