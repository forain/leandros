# design piwifiap 2026-10-28: "BSS 84:78:48:d8:7e:79 rejects with reason 2 on every boot"

## Verdict

This is not a property of the AP. **The first join after firmware init fails, whichever AP it goes to.** The first
join always goes to 84:78 only because that AP has the strongest signal.

Most likely cause: on that first join, the firmware's association-request RSN IE differs from the RSN IE we
echo in message 2. We cannot see the firmware's IE because our `assoc_info` read fails on every join, including
successful ones (`rsn_assoc_fw: unavailable`). As a result, `msg2ie=auto` falls back to "ours" on every join.

## Evidence (piirq/boot.log, 77 firmware inits)

| attempt # in boot | BSS | outcome |
|---|---|---|
| 1 | 84:78 | **76x Handshake reason 2 after msg2**, 1x assoc status 1 |
| 2-4 | 84:78 | **31 ok**, 1 assoc reject |
| 2 | 74:83 | 44 ok, 21 assoc status 1 (after the 84:78 deauth) |

- 84:78 never shows "failure 2 in a row". In every boot, the next join to 84:78 succeeds, often within seconds.
  For example, at log line 4583 a manual connect succeeded in 129 ms, right after the autoconnect at line 4573
  had failed.
- The AP verified the MIC. It deauthed straight after msg2 and did not retransmit msg1. On every failure the
  driver logged: `eapol rx 1 tx 1, message 1 after message 2: 0`. Compare LEANDRO2026 on 10-27: there msg1 came
  3x, then reason 15 (a MIC failure).
- In hostapd's PTKCALCNEGOTIATING state, MIC OK followed by a mismatch between the assoc-request IE and the
  msg2 IE leads to `wpa_sta_disconnect`. Older hostapd versions (2.6 and earlier, still common in vendor APs) send
  that disconnect with **WLAN_REASON_PREV_AUTH_NOT_VALID = 2**. Newer versions send 17. A newer AP would
  therefore show reason 17 for the same bug.
- The AP's RSN IE (`rsn_ap`, 84:78 / Anvara):
  `30140100000fac040100000fac040100000fac020c00`
  - group CCMP, pairwise CCMP, **AKM 2 (PSK) only**.
  - caps 0x000c: 16 PTKSA replay counters; **MFPC=0, MFPR=0**; no PMKIDs; no group-management cipher.
  - Our IE: `...020000` (caps 0).
  - The msg1 key_info is 0x008a, version 2 (HMAC-SHA1 + AES), which is correct for AKM 2.
- Ruled out:
  - PMF / 802.11w: the AP has MFPC=0.
  - SAE or WPA3-transition: AKM 2 only.
  - FT-PSK (AKM 4): not offered.
  - Wrong key-descriptor version or a missing MIC: the MIC is accepted, and 84:78 works on joins 2-4.
  - Band or DFS: channel 1, 2.4 GHz.
  - `sup_wpa`: it is refused (-23, no `idsup`) on every join, the successful ones included.
- Why only the first join: `configure_security` sets `wpaie` **before** `wpa_auth` and `wsec`.
  - On a freshly initialised firmware, `wpa_auth` is 0 (WPA disabled) at that point. The firmware drops or
    rebuilds its stored IE when `wpa_auth` and `wsec` change, so the association goes out with the firmware's
    own RSN IE.
  - On every later join, `wpa_auth` already equals PSK, so `wpaie` sticks.
  - Linux uses the same order, but it gets away with it: wpa_supplicant takes the msg2 IE from the reported
    association-request IEs (`brcmf_get_assoc_ies`), which is exactly the path that is broken here.
- Why `assoc_info` is always unavailable: `assoc_req_rsn()` queries `assoc_info` with an **8-byte buffer**.
  - The firmware's `wl_assoc_info_t` is about 32 bytes: req_len, resp_len, flags, and the fixed request and
    response fields. A buffer too short for it returns BCME_BUFTOOSHORT.
  - Linux queries with `WL_ASSOC_INFO_MAX` = 512 bytes.

Alternative still open: H2, stale state for our MAC on the AP or its controller. It is weakened by two facts. Cold
boots fail too, and the previous session ended with a clean disassoc from 74:83. Test step 1 below separates H1
from H2.

## 1. Fix (drivers/src/brcmf/cfg.rs)

a. `assoc_req_rsn()`: read `assoc_info` into a 512-byte buffer, as Linux does.
   - Read `req_len = le32(buf, 0)`.
   - Then read `assoc_req_ies` into a 512-byte buffer and keep `[..req_len]`. The offset-0/4/10 scan stays.
   - On error, store the reason in `self.rsn.assoc_fw` instead of "unavailable", for example
     `"assoc_info err Status(-14)"` or `"req_len 0"`.

   ```rust
   let mut li = alloc::vec![0u8; 512];
   let n = match self.fw.iovar_get("assoc_info", &mut li) { Ok(n) => n, Err(e) => { self.rsn.assoc_fw_err = format!("assoc_info {:?}", e); return None } };
   ```

b. In `configure_security`, set `wpaie` once more after `wpa_auth`, `auth`, `wsec` and `mfp`. This is the "late"
   set. Keep the early set as well. Add a knob `wpaie=1|0|late` with default `late`, which does both sets.
   Belt and braces: (a) alone fixes the problem if the firmware reports its IE; (b) makes the firmware send
   ours in the first place.

c. In the join-failure log line, include the deauth event type (`DEAUTH_IND` from the AP vs a local `DEAUTH`)
   from `Down::Deauth{by_ap}`. Today "AP rejected" is assumed.

d. Host test in brcmf-tests:
   - Use a FakeFw whose `assoc_info` returns BUFTOOSHORT for buffers under 32 bytes, and whose `wpaie` is
     discarded while `wpa_auth` is 0.
   - Assert that the first join sends msg2 with the firmware's IE and that this IE is the one in the
     association request.
   - Assert that the new order issues `wpaie` after `wpa_auth`.

## 2. Stamping timestamp_s (userland/netctl + lib/netctl-abi; the kernel cannot write slots)

Why it is never stamped: only `ports/sysbus/src/nm/engine.rs::stamp_used` writes `timestamp_s`. The Pi boots
serial-only, and the log contains no sysbus or NM line at all.

- Move the policy into `lib/netctl-abi`, as `pub fn stamp(p: &mut Profile, now_s: u64) -> bool`:
  - If `now_s` is below 1_577_836_800 (no RTC or NTP yet), write 1 when the old value is 0. The value 1 means
    "proven, time unknown".
  - Otherwise write `now_s` if the old value is 0, or if `now_s - old > 6 h`.
  - Return false when nothing changes, so no slot write happens. This keeps flash writes low and stops sysbus
    and netctl from writing back and forth.
- sysbus `stamp_used` calls the shared function and drops its own `now == 0` early return.
- New `netctl watch --stamp`: reuse the existing `watch()` loop.
  - On `WifiState` CONNECTED with a non-nil `profile_uuid` and a new `attempt`, run `backend::edit`-equivalent
    code under the slot lock with `stamp`.
  - Start it from init next to the existing wifi.conf and profile load. It is quiet: one line per write.
- Kernel (`wifi_mgr.rs` `note_success`): no persistence. `st.used` already covers this boot. The guard in
  `note_failure` (`timestamp_s != 0`) works from the next boot, or after the reload that the stamper may trigger
  (no reload is needed).

## 3. Preferring working BSSes (drivers/src/wifi_mgr.rs, wifi_pref.rs)

- New `st.bss_rejected: Vec<([u8;6], u64 last_ms)>`, bounded by MAX_BSS.
  - `note_bss_failure` adds or refreshes the entry. This covers Handshake, Assoc and flap failures; NoNetwork
    is excluded.
  - `note_success(bssid)` removes the entry.
  - The entry decays after 10 min.
  - `ProfilesChanged` clears it, the same as `bss_backoff`.
- `auto_key` gains `clean: bool` after `boot_ms`:
  `(priority, boot_ms, clean, five, stored_s, legacy, signal)`.
  A BSS that rejected us this boot then loses to any clean BSS of the same SSID, whatever its signal. It is still
  chosen when it is the only candidate, after the existing 15/60/120/300 s skip window.
- The autoconnect log adds `(rejected this boot, demoted)`.
- Host test in brcmf-tests `net_tests::bss_pref`:
  - Two BSSes at -55 and -66 dBm. The stronger fails once: after its 15 s skip window, the weaker one is still
    chosen.
  - When the weaker one disappears, the stronger one is chosen again.
  - A success clears the demotion.

## Pi test plan (aarch64 --rpi5, pi-remote kexec trial; "good" stays on the card)

0. Baseline, current good kernel: after kexec, run `cat /proc/net/nic | grep rsn_`. Expect
   `rsn_assoc_fw: unavailable` and the first autoconnect to fail with reason 2.
1. H1/H2 split, no code needed. Right after boot, and before autoconnect, run
   `netctl connect Anvara --bssid 74:83:c2:b7:ec:19` as the first join.
   - Failure (reason 2 or 17, after msg2) means H1 is confirmed.
   - Success, followed by a later 84:78 success, means H2 (the AP side).
2. Trial kernel with 1a only (`wpaie=1` knob, so not late). On the first join, expect
   `rsn_assoc_fw: 30..` instead of "unavailable", and the log line
   `join: firmware association RSN IE differs from ours (fw ... ours ...)`. That line is the proof of H1. The
   first join to 84:78 should be ok.
3. Default trial (1a + 1b): first join ok on 3 of 3 kexec boots and 1 cold power cycle, with `rsn_assoc_fw` equal
   to `rsn_assoc_ours`. Then run `grep -c 'reason 2'` on the serial log: it must be 0.
4. Stamp:
   - Run `netctl watch --stamp` (from init), then `netctl profiles`: Anvara shows a nonzero timestamp after the
     first connect.
   - After a kexec, the autoconnect line reads `stored timestamp N`, not `not connected before`.
5. Penalty: block the fix with the `wpaie=1 msg2ie=ours` knobs to reproduce the reject. The next autoconnect
   should go to 74:83 with `demoted`, not back to 84:78 after 15 s.
6. Regression: brcmf-tests all green, then box aarch64 regress including wifisimtest.
