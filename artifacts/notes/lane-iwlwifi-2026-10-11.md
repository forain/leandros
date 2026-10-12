# lane/iwlwifi — 2026-10-11 — Intel Wireless 8265/8275 Wi-Fi for the laptop

Branch `lane/iwlwifi`, base main `571ce20`. Not pushed, not merged.
Target: laptop (i5-8350U, Sunrise Point-LP), Wi-Fi Intel 8265/8275 (PCI
`8086:24fd`, Linux `iwl8265_2ac_cfg`, firmware `iwlwifi-8265-36.ucode`, PCIe
gen1 transport, MVM). Goal: `eth0` over WPA2-PSK via `netdev` → net server.
QEMU cannot emulate this NIC: the hardware path is written against Linux 6.6
iwlwifi (read as documentation) and is UNPROVEN until the laptop boots it.

## Files

| piece | file |
|---|---|
| PCIe transport: CSR/PRPH/SRAM, APM, NIC_READY, FH service-channel fw download (8000 secure CPU1/CPU2 + FH_UCODE_LOAD_STATUS), legacy RX ring, TFD rings, SCD, host cmds, error-table dump | `drivers/src/iwl_trans.rs` |
| op mode: probe, INIT (NVM, PHY DB), runtime (paging, SF, PHY DB replay, BT, DQA, aux sta, PHY ctxt, temp, power, MCC, SCAN_CFG, MAC ctxt), scan, connect, EAPOL, keys, data path, /proc lines | `drivers/src/iwlwifi.rs` |
| TLV `.ucode` parser + CPU1/CPU2/paging split | `drivers/src/iwl_fw.rs` |
| command codec W0–W2 (ALIVE, NVM, PHY cfg/DB, BT, MCC, paging, PHY ctxt, ADD_STA v10, SCD_QUEUE_CFG, SCAN_CFG v1, SCAN_REQ_UMAC v7, RX PHY/MPDU) | `drivers/src/iwl_cmd.rs` |
| command codec W3 (MAC ctxt v1 148 B, binding v1, quota v2, time event v2, ADD_STA_KEY v1, LQ v1, TX_CMD v6, TX_RSP v3, CCMP hdr, TIM) | `drivers/src/iwl_assoc.rs` |
| 802.11 frames (beacon/IE/RSN parse, auth, assoc, deauth, data<->Ethernet, A-MSDU) | `drivers/src/wifi_frame.rs` |
| WPA2 crypto (SHA1, HMAC, PBKDF2, PRF, AES-128, key wrap) + EAPOL supplicant (4-way + group rekey); hand-rolled: no FP/SIMD in kernel | `drivers/src/wifi_crypto.rs` |
| host tests (56 pass: real 8265-36 file, RFC/FIPS/802.11 vectors, struct sizes) | `drivers/iwlwifi-tests` |
| netdev: Wi-Fi = fallback backend (used when wired has no link and Wi-Fi is up); switch bumps link_generation; net server re-reads MAC on gen change | `drivers/src/netdev.rs`, `servers/net/src/lib.rs` |
| `/proc/net/wifi` (scan table), `/bin/wifiscan` | `servers/net/src/procnet.rs`, `servers/vfs/src/lib.rs`, `userland/wifiscan/wifiscan.sh` |
| firmware fetch, pinned linux-firmware 20240909 + sha256; staging /lib/firmware (x86_64); `$LEANDROS_WIFI_CONF` → /etc/leandros/wifi.conf | `ports/iwlwifi/fetch.sh`, `scripts/mkfs-f2fs-populated.py` |

Reference source: linux-6.6.tar.xz (sha256 d926a06c…56d0) extracted at
`~/.cache/leandros/linux-src/linux-6.6/drivers/net/wireless/intel/iwlwifi`.

## Milestone status
- W0 probe/power-up: implemented, boots as no-op in QEMU aarch64 (log `[iwlwifi] no Intel 8265/8260 ... driver inert`). Laptop unproven.
- W1 firmware: TLV parse verified on host against the real file; download/ALIVE/NVM/calib/runtime bring-up implemented, unproven.
- W2 scan: implemented (UMAC v7, results via RX_MPDU beacons, /proc/net/wifi), unproven.
- W3 assoc + WPA2-PSK + data path: implemented (legacy non-HT, no WMM, LQ legacy table, firmware CCMP), unproven.

## Cmdline (first laptop boot)
`iwlwifi.start=1 iwlwifi.debug=1` plus credentials either by building with
`LEANDROS_WIFI_CONF=<host file with ssid=... / psk=...>` during
`./scripts/build-all.sh`, or `wifi.ssid=<ssid %20-escaped> wifi.psk=<pass>`
via `LEANDROS_USB_EXTRA_CMDLINE`. Run `ports/iwlwifi/fetch.sh` before build.
Other knobs: `iwlwifi.disable=1`, `iwlwifi.scan=0`, `iwlwifi.connect=0`,
`iwlwifi.lq=0` (fixed 12 Mb/s data, if LQ is rejected), `iwlwifi.fw=<path>`.
Default without `iwlwifi.start=1`: probe only (logs rev/RF id/RF-kill/OTP MAC).

## What to grep in the stick log
`grep -a '\[iwlwifi\]' <log>`: `found`, `CSR_HW_REV`, `NIC ownership`, `RF id`,
`hardware RF-kill`, `firmware ... loaded`, `INIT image ALIVE`, `NVM version`,
`INIT calibrations complete`, `paging:`, `runtime image ALIVE`, `regulatory: MCC`,
`scan complete`, BSS lines, `connecting to`, `authenticated`, `associated: AID`,
`pairwise key`, `group key`, `wlan: link up`; failures: `no ALIVE`, `SecBoot`,
`error table`, `REPLY_ERROR`, `no response to`, `firmware error`.

## HANDOFF
Head: commit on `lane/iwlwifi` (see `git log`), worktree
`/Users/forain/code/leandros/.claude/worktrees/agent-a5f024c96931546d4`.

Done + verified:
- Host tests: `cd drivers/iwlwifi-tests && cargo test --target aarch64-apple-darwin` → 56 passed.
- `./scripts/build-all.sh` (release) EXIT=0; image packs `iwlwifi-8265-36.ucode` and `wifiscan`.
- QEMU aarch64 (`LEANDROS_RUN_ID=iwlwifi`, driver.py start/login): boots to shell, iwlwifi inert, virtio eth0 DHCP bound.

NOT done:
- x86_64 QEMU boot after the change was not run (wrap-up order). Run it first.
- A read-only deep review of the hardware code vs Linux was launched and its
  findings were NOT collected/applied. Re-run such a review (prioritise: wide
  vs short cmd headers per Linux groups, SCAN_CFG v1 vs v2 for this fw, ALIVE
  version, MAC_CONTEXT/TX_CMD layouts, aux station need with ADD_STA v10,
  queue-before-station ordering) and fix BLOCKERs before the laptop boot.
- `/proc/net/wifi` reads empty in QEMU and `[ -r /proc/net/wifi ]` failed in
  brush (wifiscan printed "not available"): check vfs `proc_net_open` routes
  new names to `procnet::generate` (maybe a name whitelist elsewhere in vfs);
  `/proc/net/nic` works.
- QEMU ping to 10.0.2.2 failed but DHCP bound on 192.168.105.x (macOS vmnet,
  not slirp) — likely environment, compare with main before blaming the lane.

Next steps in order: (1) x86_64 QEMU boot; (2) fix /proc/net/wifi; (3) review
+ fix; (4) laptop stick with `iwlwifi.start=1 iwlwifi.debug=1`, read log,
iterate W0→W1→W2→W3; (5) later: HT/WMM association, MSI instead of polling,
PMF.

Pitfalls / design facts: firmware API 36 has NO TLC offload (driver LQ table),
NO session-protection cmd (TIME_EVENT used), ADD_STA v10 (<12 ⇒ old station
API ⇒ aux station required for scans), adaptive dwell v1 ⇒ scan req v7, paging
0x27000 B secured; MCC_UPDATE cmd is 28 bytes, TIME_EVENT 36 bytes; bc table in
dwords on family 8000; RX is legacy (RX_PHY 0xc0 + RX_MPDU 0xc1), CCMP header
+ MIC left in decrypted frames (skip 8/8). Crypto hand-rolled because RustCrypto
selects SIMD at runtime (kernel must not touch FP/vector regs). Interrupts never
delivered; everything polled (driver thread 1 ms, net daemon via poll_receive).
