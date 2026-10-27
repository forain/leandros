# lane/piprofile 2026-10-27 — Pi Wi-Fi: profile autoconnect "wrong key" vs inline OK

Status: PARTIAL (secret-path hypothesis REFUTED by code reading + host test; misclassification FIXED).

## Secret path: identical (refuted)
netctl save -> Profile::new_wifi (userland/netctl/src/main.rs:364-370) -> slot encode/decode
(lib/netctl-abi/src/profiles.rs:259-310) -> servers/net/src/netctl/profiles.rs:107 load -> wifi_api::set_profiles
-> wifi_mgr pick_auto -> Secret::new(p.secret()) (drivers/src/wifi_mgr.rs ~1370) -> attempt -> brcmf netdev
connect -> Key::from_secret -> Key::psk(ssid). Inline: NcConnect.secret() -> Secret::new (servers/net netctl/mod.rs
connect_inner) -> same attempt/connect. Same bytes, same AKM (AUTO), same PBKDF2. Host test
`profile_and_inline_secret_give_the_same_pmk` (drivers/brcmf-tests) proves byte/PMK equality incl. 64-hex and spaces.

## Real differences between the paths
1. BSS choice: autoconnect picks the BSS by `auto_key` (5 GHz preferred, wifi.prefer5=1 default) and pins it
   (`bssid: Some(..)`); inline/ctl join take the strongest RSSI. On a dual-band "Anvara" these differ.
2. Timing: autoconnect joins immediately after a background scan.
3. Classification: brcmf `msg2 = link_seen && keyed` (cfg.rs join_inner), so any deauth after link-up was
   "msg2: true", and wm::handshake_failure maps reason 2 to WRONG_KEY -> profile put in wrong_key, autoconnect
   never retries it (the "fails every time").

## Fix (7127eb09)
Host supplicant counts EAPOL sent and message 1 retransmissions after message 2 per join
(RsnDiag.eapol_sent / msg1_after_msg2, Dev::handshake_evidence). netdev connect uses
wm::handshake_failure_ev: reason 1/2 with no msg1 re-send -> DEAUTH_BY_AP "AP rejected (reason 2) after
message 2 ..." (BSS backoff, profile stays eligible). Firmware supplicant and iwlwifi unchanged.

## Next Pi boot
Look for `connecting to 'Anvara' <bssid> ch N ... [auto]` and compare ch/bssid with the inline connect line.
If the auto line is a 5 GHz BSS and the inline one 2.4 GHz: boot with wifi.prefer5=0 to confirm.
