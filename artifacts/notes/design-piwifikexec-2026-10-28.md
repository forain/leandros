# design piwifikexec 2026-10-28: "wifi0 lost after trial->trial kexec"

## Verdict
Not a kexec / quiesce / SDIO bug. The chip comes back cleanly after every kexec; wifi0 is lost
because the autoconnect manager **permanently blocks the profile for the rest of the boot** after a
single handshake timeout on one weak BSS, which it classifies as "wrong passphrase".
The trial->trial correlation is a coincidence (1 of 3 trial->trial kexecs in the log failed).

## Evidence (scratchpad/piirq/boot.log)
Per boot (build, line of `Kernel starting`) -> brcmf outcome:
| boot | build | from | chip/init | wifi0 DHCP |
|---|---|---|---|---|
| 29832 | 3bc3d35e trial | kexec | init ok | yes |
| 30173 | 3bc3d35e trial | kexec from trial | init ok | yes |
| **30527** | 3bc3d35e trial | kexec from trial | **init ok** (sdio wloff=200ms, chipid ok, F2 ready, clm, ver) | **no** |
| 30891..32677 | good/trial alternating | kexec | init ok | yes (every one) |

Old-kernel side of every kexec is clean: `[brcmf] shutdown: disassoc ok WLC_DOWN ok`,
`[REBOOT] brcmf: disassoc+down in 10 ms; sdio2 host reset, ... WL_REG_ON low`, then
`[kexec] quiesced: dma40 xhci0 xhci1 gem`, `[kexec] bus quiesced: pcie-perst` (lines 32595-32605).
The pikexecwifi fix (WL_REG_ON low in quiesce_phase1, 200 ms off on boot) works; the
pikexecdma/dma2 ordering does not touch SDIO (brcmf runs in phase 1, before Device/Bus stages).

The failing boot 30527, join sequence (the SSID 'Anvara' has 3 BSSes):
1. 84:78:48:d8:7e:79 ch1 -56 dBm: `AP rejected (reason 2) after message 2` -> DEAUTH_BY_AP, BSS backoff 15 s.
   (This BSS does this on EVERY boot, cold boots too: lines 4577, 9137, 11344 ... — environmental.)
2. 74:83:c2:b7:ec:19 ch6 -67 dBm: `Assoc { status: 1 }` -> ASSOC_REJECTED, BSS backoff 15 s.
3. 8c:30:66:6a:6f:9d ch1 **-81 dBm**: `eapol rx 2 tx 2, message 1 after message 2: 1` ->
   `4-way handshake timed out after message 2: wrong passphrase` -> `connection ... failed: wrong-key`.
4. Then nothing until `[KEXEC] loaded` (~4 min): no further autoconnect, although (1)/(2) leave
   back-off after 15 s and the same passphrase connected on every other boot.

Code path:
- drivers/src/wifi_mgr.rs:324 `None if msg2_sent => Fail::new(reason::WRONG_KEY, "4-way handshake timed out after message 2: wrong passphrase")`
  (and :321-322 for reasons 15/14/23/1/2 with mic_suspect != Some(false)). At -81 dBm a lost/late
  msg2 or msg3 gives exactly the "msg1 re-sent after msg2" pattern.
- drivers/src/wifi_mgr.rs:1130-1132 `note_failure`: WRONG_KEY/NO_SECRET -> `st.wrong_key.push(uuid)`; no BSS backoff, no expiry.
- drivers/src/wifi_mgr.rs:1043 `pick_auto`: `if st.wrong_key.contains(&p.uuid) { continue; }` -> the profile
  is skipped for every BSS until a user Connect (:1324) or profile reload (:1354/:1645/:1682).
- The profile has `timestamp_s == 0` (autoconnect logs "not connected before", wifi_mgr.rs:1389), so there
  is no stored "this secret worked" evidence either.

## Fix (intended diff, all in drivers/src/wifi_mgr.rs; no driver/kexec change)
A wrong-key verdict blocks the profile only with corroboration, like wpa_supplicant's
temp-disable (`WRONG_KEY` -> ssid temp-disabled with growing back-off, never a permanent block
from one BSS):
1. `St` gains `wk_bss: Vec<([u8;16] uuid, [u8;6] bssid, u64 ms)>` (bounded, MAX_BSS) — WRONG_KEY
   verdicts this boot per (profile, BSS).
2. `note_failure`, WRONG_KEY branch (not NO_SECRET):
   - always `note_bss_failure(bssid)` (the BSS backs off like any other BSS fault);
   - record (uuid, bssid); block the profile (`st.wrong_key.push`) only when the WRONG_KEY verdicts
     for this uuid within WK_WINDOW_MS (10 min) cover >= WK_BLOCK_BSS (default 2) distinct BSSes.
     A profile proven this boot (`st.used` contains uuid) or by a stored `timestamp_s != 0` is never
     blocked, only backed off;
   - otherwise put the profile on the normal `backoff` (BACKOFF_MS 10/30/60 s) and log
     `wrong-key verdict on <bssid> (n of 2 BSSes), not blocking the profile yet`.
   - when the block does happen, log `profile '<ssid>' blocked: wrong passphrase on N access points`
     (so a real wrong passphrase still stops hammering after the 2nd BSS).
3. Optional, same lane: `handshake_failure_ev` stays as is (the label is fine as a hint); only the
   policy changes. NO_SECRET keeps blocking immediately.
4. note_success clears `wk_bss` for that uuid.

Knob: `wifi.wrongkey_bss=N` (default 2; 1 = old behaviour) parsed in `wifi_pref::set_knobs`
(drivers/src/wifi_pref.rs:22), stored in an AtomicU8.

Non-goals / notes for the orchestrator:
- 84:78:48:d8:7e:79 rejects us with reason 2 on every boot (cold too); it costs one failed attempt
  per boot. Separate low-priority item (possibly the AP keeps our old PMKSA/assoc; a deauth on
  shutdown already goes out — `disassoc ok`).
- Possible follow-up: stamp `timestamp_s` on a successful connect so "proven" survives reboots.

## Tests
Host (drivers/brcmf-tests/src/net_tests.rs, which already drives wifi_mgr):
- T1: profile P, BSS A WRONG_KEY -> P not in wrong_key, A backed off, P backoff 10 s; after 15 s
  pick_auto returns BSS B (regression for this bug).
- T2: WRONG_KEY on A then on B -> P blocked (real wrong passphrase still stops autoconnect).
- T3: P connected this boot, then WRONG_KEY on A and B -> not blocked (backoff only).
- T4: `wifi.wrongkey_bss=1` -> old behaviour (blocked after one).
- T5: NO_SECRET -> blocked immediately (unchanged).
- Existing brcmf-tests all pass; aarch64 QEMU regress (lane rule: aarch64 only).

Pi (fast-worker, pi-remote skill; do not hold the Pi longer than needed):
1. `pi-deploy` the trial build (rpi5) and kexec into it from good 902a07c4.
2. trial -> trial kexec x3. After each: `cat /proc/net/...`/netctl status shows wifi0 with
   172.16.162.184 within 60 s; serial/klog must show
   - old kernel: `[brcmf] shutdown: disassoc ok WLC_DOWN ok`, `[REBOOT] brcmf: disassoc+down ...WL_REG_ON low`,
     `[kexec] quiesced: dma40 xhci0 xhci1 gem`, `[kexec] bus quiesced: pcie-perst`
   - new kernel: `[sdio] chipid=0x15264345 ... ok`, `[brcmf] init ok`, `[brcmf] connected to 'Anvara'`,
     `[NET] DHCP configured, address: ... (wifi0)`.
   - if a weak BSS hits the timeout: `wrong-key verdict on 8c:30:66:6a:6f:9d (1 of 2 BSSes), not blocking`
     followed by another `autoconnect:` line and a successful connect within ~20 s.
   Unwanted: `profile 'Anvara' blocked`, no `autoconnect:` line after a failure, `fail fw HT clock timeout`.
3. Note: the failure is intermittent (1/3 before); 3 green kexecs is weak evidence alone — T1 is the
   real proof. If time allows, force it: boot with `wifi.wrongkey_bss=1` once and confirm the old
   symptom can recur (optional).
