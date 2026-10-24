# lane/usbstick — 2026-10-24 (L8.2, laptop parts)

## SuperSpeed hubs: FIXED (host-tested only)
- Before: `setup_hub` returned "SuperSpeed hubs are not supported" for speed >= SS.
- Now (drivers/usb/src/hubproto.rs, xhci/mod.rs): 12-byte 0x2A descriptor, SET_HUB_DEPTH(tier) before any port request,
  warm-reset completion (C_BH_PORT_RESET) accepted and cleared with C_PORT_LINK_STATE, children enumerated as SuperSpeed,
  no TT. The USB 2.0 decode would have read SS PORT_POWER (bit 9) as LOW_SPEED. Ports > 15 are clamped (route string limit).
- Guard: 9 host tests in drivers/usb/msc-tests/src/hubproto_tests.rs (45/45 green). QEMU has no SS hub model (only usb-hub, USB 2.0).
- Laptop: no SS hub exists (bus 1: BT 8087:0a2b, fingerprint 138a:00ab, webcam 0408:5371 HS, stick 0781:55ab SS, all on root ports;
  bus 2 Thunderbolt xHCI: 0 devices), so nothing new enumerates there; verify on real SS hub hardware when one is attached.

## Stick latency on the laptop (SanDisk 0781:55ab, fsbench wrlat, new subcommand)
- pwrite+fsync 4 KiB: p50 21.6 / p99 58.9 / max 138 ms (trial: 19.7 / 53.7 / 71.7). 64 KiB: 27.1 / 57.7 / 88.5 ms.
  1 MiB: 136 / 231 / 245 ms (trial: 99.7 / 176 / 178 ms).
- Device per-command write max 62 ms ([usbms] klog); no 0.5–1.3 s writes on this stick (those were the PC stick 13fe:4300).
- stallprobe (bulk 2 MiB+fsync + dbwrite + cold statwalk /usr/share): cold walk max 3.5 ms, hot stat max 0.13 ms,
  read lock wait max ~6 ms, "behind <= 0 wr". Read priority + per-command write splitting (usb_storage.rs) already bound
  a cold read to one write command. REFUTED on the laptop; PC re-check deferred.
- SYNC CACHE → NO SENSE: already handled since lane/pcfixes (treated as success like ILLEGAL REQUEST/RECOVERED ERROR,
  guarded by flush_no_sense_* host tests). The SanDisk logs no sense at all.

## Tests
- aarch64 regress PASS; usbunplugtest all PASS; usbstalltest PASS; x86_64 release build OK.
