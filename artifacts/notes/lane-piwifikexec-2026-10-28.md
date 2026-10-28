# lane piwifikexec 2026-10-28: wifi0 lost after kexec

Design: design-piwifikexec-2026-10-28.md (same dir). Not a kexec/SDIO bug: a single 4-way
handshake timeout on a weak BSS (-81 dBm) was labelled WRONG_KEY and blocked the profile for the boot.

FIXED. New drivers/src/wifi_wrongkey.rs (pure policy, host-tested), wired into
wifi_mgr.rs `note_failure` / `note_success`; knob `wifi.wrongkey_bss=N` (default 2, 1 = old) in wifi_pref.rs.
- WRONG_KEY backs off the BSS and the profile; the profile is blocked only after verdicts on >= N distinct
  BSSes within 10 min, never if it connected this boot or has a stored timestamp. NO_SECRET still blocks at once.
- Host tests T1-T5 (brcmf-tests net_tests::wrongkey). With the legacy always-block policy 4 of 5 fail
  (T1 included); with the fix 5/5 pass. wifi_mgr itself is kernel-coupled, so the policy lives in the pure module.
- Pi (good 902a07c4, trial a743f6b2): pi-deploy + trial->trial kexec x3, wifi0 DHCP 172.16.162.184 each time;
  the weak-BSS failure did not recur (intermittent), so T1 is the proof. Left on good.
- Builds: aarch64 --rpi5, aarch64, x86_64 INTEL=1: 0 errors.
- Open: BSS 84:78:48:d8:7e:79 rejects with reason 2 on every boot; stamp timestamp_s on success.
