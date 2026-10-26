# Lane pibrcmf-net (2026-10-26): CYW43455 FullMAC control + wifi0 netdev

Status: DELIVERED (host-tested; no hardware run yet, the chip/SDIO lanes are separate).

## What landed
- `drivers/src/brcmf/` created with the frozen `mod.rs`, `bus.rs`, `fwil.rs` exactly as in the
  Pi driver brief; `chip.rs`, `fw.rs`, `sdpcm.rs`, `bcdc.rs`, `sdio_host.rs` are one-line stubs
  for lanes pibrcmf-core / pisdio. `pub mod brcmf;` added to `drivers/src/lib.rs`.
- `event.rs`: bcmeth + `brcmf_event_msg` parser (big-endian header, LE payloads), encoder for
  tests, `brcmf_if_event`, and the 16-byte `event_msgs` mask builder (`EventMask::station()`:
  SET_SSID, JOIN, AUTH, DEAUTH(_IND), ASSOC, REASSOC, DISASSOC(_IND), LINK, ROAM, PSK_SUP, IF,
  ESCAN_RESULT).
- `cfg.rs`: `Brcmf<F: Fwil>`.
  - init: `cur_etheraddr` get, `country` {XX, rev -1, XX} (`brcmf.country=` / `wifi.country=`,
    default PT; refusal is logged, not fatal), `mpc` 0, `bus:txglom` 0, `event_msgs`, WLC_UP,
    WLC_SET_INFRA 1.
  - escan v1 (wildcard + directed SSIDs), results accumulated per sync id, deduplicated per BSSID
    (strongest wins, hidden sightings keep the known SSID), aborted on timeout.
  - join: SET_AUTH 0, WLC_SET_WSEC=AES, WLC_SET_WPA_AUTH=WPA2_PSK, `sup_wpa`=1,
    WLC_SET_WSEC_PMK (passphrase with WSEC_PASSPHRASE; a 64-hex PSK goes as a hex PMK), then the
    `join` iovar (ext join params, 20 MHz chanspec of the scanned channel) with WLC_SET_SSID as the
    fallback. Done = LINK up + PSK_SUP FWSUP_COMPLETED; failures classified (no network, assoc
    refused, handshake with/without msg2 + AP reason, timeout, preempted).
  - link tracking (deauth/disassoc/_IND, LINK-down, supplicant failure), disconnect
    (WLC_DISASSOC reason 3), reconnect (last request), RSSI, rate, radio on/off. Keys wipe on drop
    and print redacted.
- `netdev.rs`: wifi0 + netctl backend. `wifi_mgr::Radio` impl over `Brcmf` (the same Mgr that
  iwlwifi.mgr=1 and wifi_sim use, so netctl/NetworkManager/DHCP see the same shapes); TX/RX
  queues between the net daemon and the `brcmf` thread; `/proc/net/nic` info lines.
  `drivers/src/netdev.rs` registers `BRCMF_OPS` as Kind::Wifi `wifi0` when
  `brcmf::netdev::present()`, which only an rpi5 build can set (`brcmf.disable=1` opts out).
- Entry points for integration (call once the SDIO host + chip + firmware + BCDC are up):
  `drivers::brcmf::netdev::start(fwil)` (spawns `brcmf`, needs `F: Send`) or `run(fwil)` on the
  caller's thread.

## Evidence
- `cd drivers/brcmf-tests && cargo test --target aarch64-apple-darwin`: 22 net tests pass, 44 in the crate with the core lane's (mock Fwil
  recording ioctls/iovars, canned events: init order, refusals, scan dedup/stale sync/timeout,
  scan -> join -> LINK up -> data in/out -> deauth -> reconnect, wrong key (deauth 15 after M3
  wait), supplicant timeout, no network, assoc refused, join-iovar fallback, timeout/abort, link
  loss, open network). Guards shown to fail on mutants: LE event header (12 fail), no ifidx
  filter (1 fails).
- `scripts/check-rpi5.sh` rpi5 and CHECK_FEATURE=raspi4b: clean (no brcmf warnings).
- aarch64 release build + QEMU virt regress: PASS twice (before and after merging main with pisdio + pibrcmf-core); virt shows only `[NET] netif eth0 = virtio-net`, no wifi0.

## Next Pi boot should show (once integration calls `start`)
- `[NET] netif wifi0 = brcmfmac` at netdev init (rpi5 only; nothing on virt or the laptop).
- `[brcmf] init ok: mac b8:27:eb:..|d8:3a:dd:.. country PT`, or `[brcmf] init failed: ...; wifi0 stays down`.
- After a profile/autoconnect: `[brcmf] associated with xx:xx:.. on channel N`, then a DHCP lease on wifi0.
- Without integration: only the `[NET] netif wifi0` line and a Mgr attach line; wifi0 link down.

## Open
- Not exercised on hardware: event-mask length (16) and `country` rev -1 are what the 7.45
  firmware family expects, unverified on this chip. If `event_msgs` is refused, init fails loudly.
- WPA3/SAE out of scope (caps advertise PSK only).
- `run` registers nothing new at runtime; wifi0 exists from boot on rpi5 even if the chip never
  comes up (same as iwlwifi before its firmware loads).
