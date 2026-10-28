# lane/piwifiap 2026-10-28: first join after firmware init got reason 2 after msg2

Design: artifacts/notes/design-piwifiap-2026-10-28.md. Status: FIXED on the Pi (5/5), see evidence.

## Changes
- drivers/src/brcmf/cfg.rs: `assoc_req_rsn()` reads `assoc_info` into 512 bytes (was 8: BUFTOOSHORT on every join);
  `rsn_assoc_fw` now reports the error instead of a bare "unavailable"; `wpaie` is set again after
  wpa_auth/auth/wsec/mfp (knob `wpaie=late|early|0`, default late; `1` = early only); the join log names
  DEAUTH vs DEAUTH_IND (AP) vs DISASSOC.
- lib/netctl-abi: shared `profiles::stamp(p, now_s)` (1 = proven/time unknown; refresh only if older than 6 h).
  sysbus `stamp_used` uses it; `netctl watch --stamp` (started by init when /dev/netctl exists) stamps on CONNECTED.
- drivers/src/wifi_mgr.rs, wifi_pref.rs: per-boot rejected-BSS list (10 min decay, cleared by success and
  ProfilesChanged); `auto_key` gains `clean` after `boot_ms`; autoconnect log says "rejected this boot, demoted".
- Tests: brcmf-tests (84 pass: late wpaie order, 512-byte assoc_info with BUFTOOSHORT mock), iwlwifi-tests pref
  (demotion), netctl-abi stamp_policy.

## Evidence (Pi 5, 5 trial kexecs of a8b3e2ec)
- Baseline (good kernel): `rsn_assoc_fw: unavailable`.
- All 5 boots: first join associated, `rsn_assoc_fw: ...020c00` (caps 0x000c, 16 PTKSA counters) vs ours `...020000`,
  `rsn_msg2_ie: fw`, log "firmware association RSN IE differs from ours". 4 of 5 went to 84:78 (the old failing AP).
  H1 confirmed: the firmware sets the PTKSA-counter bits itself; the late wpaie did not change the IE it sends, the
  fix is the echo of the firmware IE in msg2 (that needed the 512-byte buffer).
- Not run: design step 1 (no-code H1/H2 split) and the stamp check on the Pi (needs the new /bin/netctl in the root,
  i.e. pi-sync, which would change the Pi root); stamp is covered by the netctl-abi unit test only.
- Pi state: was running slot trial (== good, 1f82dbdc); left on `good` (same kernel, uname 1f82dbdcbd).
