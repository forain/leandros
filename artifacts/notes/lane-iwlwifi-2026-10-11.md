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
### 2026-10-12 lane/iwlfix — first real-hardware boot (intelpc-boot1.log)
Probe, OTP MAC, fw download, INIT ALIVE 0xcafe all fine; then on the FIRST
NVM_ACCESS (section 0) the firmware asserted: LMAC 0x71 NMI_TRM_HW_ERR
(last_cmd 0x188 = our NVM_ACCESS, parsed fine), UMAC 0x38 BAD_COMMAND with
`cmd_hdr 0x78b71b7c` = data1 0x7c / data2 0x1b: the UMAC read a *garbage*
second command header. Cause: `send_cmd` padded every command to 20 bytes
(16-byte NVM_ACCESS + 4 zero bytes in the TFD); with the wide header the
firmware consumed 16 and parsed the pad as a further command. Linux never
pads (TB0 = min(total, 20)). The wide-header/DEF_ID part of 288e38f is
CORRECT (mvm/ops.c `trans->wide_cmd_header = true`; iwl-trans.c DEF_ID).
Fixes: no padding; header encoding moved to `iwl_cmd::hcmd_header/hcmd_group`
with a host test pinning every command's group/header against Linux;
`parse_nvm_resp` judges status before length (section 0 answers status 1 with
no data -> was our "short NVM response"); a firmware error now wins over a
racing response in `send()`; 3 consecutive runtime fw errors on connect stop
the driver instead of restarting forever.
Boot 2 grep: `grep -a '\[iwlwifi\]'` -> expect `NVM read in N ms; sections
1:… 3:… 4:… 5:… 12:…`, `NVM version`, then INIT_COMPLETE / runtime ALIVE.
If it still asserts, read UMAC `cmd_hdr` (bytes = cmd, group, seq) and LMAC
`last_cmd` first.

### earlier
Head: `lane/iwlwifi` (see `git log`), worktree
`/Users/forain/code/leandros/.claude/worktrees/agent-a5f024c96931546d4`.
Not pushed, not merged.

Done + verified (2026-10-11, second session):
- Host tests: `cd drivers/iwlwifi-tests && cargo test --target aarch64-apple-darwin` -> 57 passed
  (new: command ids pinned to Linux `fw/api/commands.h`).
- `./scripts/build-all.sh` (release) exit 0.
- QEMU x86_64 AND aarch64 (`LEANDROS_RUN_ID=iwlwifi`): boot to login, root shell,
  `[iwlwifi] no Intel 8265/8260 ... driver inert`, `wifiscan -v` prints
  "no Wi-Fi device" + the virtio-net /proc/net/nic block, no panics.

Bring-up review vs Linux 6.6 (path of the first laptop boot only):
- Transport (`iwl_trans.rs`): NO blockers. Checked register-by-register against
  pcie/trans.c, tx.c, rx.c, iwl-csr/fh/prph.h and cfg/8000.c: NIC_READY/PREPARE,
  APM init (8000 udelay(2) before CLOCK_READY, no APMG/L1-active steps since
  `apmg_not_supported`), legacy RX (8265 has no `mq_rx_supported`) register
  sequence, 4K RBs, SCD/CBBC/KW, FH service-channel load with LMPM extended
  window, 8000 secure CPU1/CPU2 load-status handshake, RELEASE_CPU_RESET,
  fw_alive SCD init, gen1 TFD + bc table in dwords, RX RB walk. All match.
- Op mode (`iwlwifi.rs`, `iwl_cmd.rs`, `iwl_fw.rs`): NO blockers. Firmware
  36 dumped: CMD_VERSIONS TLV present (ALIVE v3, ADD_STA v10, PHY_CONTEXT v1,
  SCAN_REQ_UMAC v7, PHY_CFG v1) and matches; ADAPTIVE_DWELL set, ADWELL_V2 /
  REDUCED_SCAN_CONFIG / SCAN_EXT_CHAN_VER clear => scan req v7 + SCAN_CFG v1 +
  channel cfg v1 are correct. INIT and runtime command order, struct sizes
  (ALIVE 68, NVM_ACCESS 8, SF 92, ADD_STA 48, PHY ctxt 36, MCC 32, SCAN_CFG
  36+52, SCAN_REQ v7) all match.
- Fixed (minor, cheap, Linux-parity):
  - INIT image got CSR_HW_IF_CONFIG radio type/step/dash = 0 (phy_config was
    set only after INIT); now set before `start_image(IMG_INIT)` like
    `iwl_mvm_nic_config`.
  - Host commands always use the wide header (group-0 ids sent as LONG_GROUP),
    as Linux MVM does (`iwl_trans_send_cmd` DEF_ID); notification matching
    accepts group 0 or 1 (`notif-wait.c`).
  - OTP MAC re-read while the INIT image runs (Linux
    `iwl_set_hw_address_family_8000`); probe-time read is the fallback.
  - `SHARED_MEM_CFG` id 0x8c -> 0x25 (unused today).
- `/proc/net/wifi` "not available": generation was fine (`cat` worked); the
  bug was `access(2)` on ANY /proc path — VFS_ACCESS proxied it to the root
  filesystem, which answered ENOENT, so brush `[ -r /proc/net/... ]` failed.
  `handle_access` now returns ENOSYS for /proc so the kernel's stat-based
  fallback answers (servers/vfs/src/lib.rs). `[ -r /proc/net/nic ]` and
  `wifiscan` now work.

Open / not done:
- Minor review items left: no LTR_CONFIG (0xee, power only); `restart_rt`
  replays the old PHY DB instead of re-running INIT (only after fw error);
  probe template lacks the DS Parameter Set element (cosmetic).
- `[ -r /proc/meminfo ]` still fails (generated /proc files outside
  /proc/net; stat path) and `[ -r /dev/null ]` fails — pre-existing, not
  Wi-Fi related.
- QEMU ping to 10.0.2.2 earlier: vmnet environment, not investigated.
- Everything hardware is still UNPROVEN until the laptop boots it.

Next steps: (1) laptop stick with `iwlwifi.start=1 iwlwifi.debug=1`, read log;
the log names the failing stage (NIC_READY, MAC clock, per-chunk FH_TX ack
5 s, SecBoot CPU1/CPU2 status after ALIVE timeout, NVM, INIT_COMPLETE) —
iterate W0->W1->W2->W3; (2) later: HT/WMM association, MSI instead of polling,
PMF, LTR.

Pitfalls / design facts: firmware API 36 has NO TLC offload (driver LQ table),
NO session-protection cmd (TIME_EVENT used), ADD_STA v10 (<12 => old station
API => aux station required for scans), adaptive dwell v1 => scan req v7, paging
0x27000 B secured; MCC_UPDATE cmd is 28 bytes, TIME_EVENT 36 bytes; bc table in
dwords on family 8000; RX is legacy (RX_PHY 0xc0 + RX_MPDU 0xc1), CCMP header
+ MIC left in decrypted frames (skip 8/8). Crypto hand-rolled because RustCrypto
selects SIMD at runtime (kernel must not touch FP/vector regs). Interrupts never
delivered; everything polled (driver thread 1 ms, net daemon via poll_receive).

## Review findings vs Linux 6.6 (collected after handoff; NOT yet applied — do these first)
- BLOCKER B1: `iwl_trans.rs` `send_cmd` pads commands to 20 bytes and group-0 cmds go with short headers → fw sees wrong lengths (NVM_ACCESS 8→16 etc). Linux: mvm sets `wide_cmd_header=true`, every group-0 cmd except REPLY_ERROR becomes group 1 (LONG) with 8-byte wide header (len = payload); TB0 = min(total,20), no padding. Fix: drop padding (`len = total`), send all host cmds as `Some((LONG_GROUP,0))` unless they have their own group (DQA=5, TEMP=4). TX_CMD stays short header.
- MAJOR M1: `t.phy_config` is 0 during INIT download → HW_IF_CONFIG radio bits wrong. Set it from `fw.info.phy_config` (masked) before the first `start_hw` in `driver_thread` (expected |= 0x9000 for PHY_SKU 0x330018).
- MAJOR M2: `Dev::dispatch` treats any queue-0 packet without SEQ_RX_FRAME as a response (ALIVE may arrive with seq 0 before tx_start) → only treat as response if cmd queue active and index in [read, write); `send()` must also check `p.cmd == id`; ALIVE predicate accept group 0 or 1.
- MAJOR M3: MAC_PM_POWER_TABLE (0xA9, 44 B: id_and_color, flags 0, keep_alive_seconds = 25 (or max(25, 3*dtim*bi*1024/1e6)), rest 0) never sent; Linux sends after MAC_CONTEXT ADD and before every TIME_QUOTA.
- MINOR: re-read WFMP OTP MAC after INIT ALIVE; SCAN_ABORT payload 8 bytes; don't fail scan on non-zero SCAN_REQ response dword (just log); unassoc MAC ctxt BSSID 00:00:00:00:00:00 not ff; EDCA txop units (Linux txop*32 → VI 3008, VO 1504) and no-QoS aifs 2 / cw 15/1023; ADD_STA flags_msk (3<<26)|(3<<28)|(1<<17) on ADD and MODIFY; adwell budget 100 when an SSID is given; PHY DB replay only types 1,2,4,5.
- Reviewer confirmed correct: APM/prepare/sw_reset/stop, all register addresses, FH fw download + load-status handshake, section split vs real file, ALIVE v3/v4 parse, RX/TX ring init, SCD tx_start, TFD/BC table, NVM/PHY DB/paging/SF/BT/DQA/MCC/SCAN_CFG v1/SCAN_REQ v7 layouts, ADD_STA v10, PHY ctxt v1, MAC ctxt 148 B, binding/quota/time event, ADD_STA_KEY v1, LQ, TX_CMD v6, TX_RSP v3, RX PHY/MPDU parsing.
