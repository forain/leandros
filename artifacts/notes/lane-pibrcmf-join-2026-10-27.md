# lane pibrcmf-join (2026-10-27): Pi 5 join fails with BCME -23; SSID case from card prep

Status: FIXED in code (host-tested); the real cause of -23 is narrowed to one command and is now logged
by name. The next Pi boot confirms which one.

## Bug 1: card prep derived the SSID from the .psk file name (FIXED)
`leandro2026.psk` -> `ssid=leandro2026`, but the AP is `LEANDRO2026` (SSIDs are case-sensitive).
`scripts/prepare-rpi5-sdcard.sh` now takes `--wifi-ssid NAME` (or `$RPI5_WIFI_SSID`) or an `ssid=` line in
the file (with `psk=`); without either it stops. The file name is never used. The PSK is never printed.
Checked the three cases (option + bare passphrase file, ssid=/psk= file with CRLF, neither -> error).

## Bug 2: join failed with Fw(Status(-23)) = BCME_UNSUPPORTED (FIXED, cause to be confirmed)
Old sequence: INFRA, SET_AUTH, SET_WSEC, SET_WPA_AUTH, `sup_wpa`=1 (fatal), SET_WSEC_PMK (passphrase), `join`
iovar (SET_SSID fallback, which logs its own line). The Pi log shows no "join iovar refused" line, so the -23
came from the security setup. The only commands there that a firmware build may lack are `sup_wpa` (exists
only with the in-firmware supplicant, cap `idsup`) and WLC_SET_WSEC_PMK (same). Linux brcmfmac enables the
firmware supplicant only when "cap" lists `idsup`; otherwise wpa_supplicant runs the 4-way handshake on the
host and installs keys with "wsec_key". So the likely cause is a 7.45.265 build without `idsup`.

What changed (drivers/src/brcmf/cfg.rs, netdev.rs):
- init reads "cap" and logs `caps: ...`; /proc/net/nic shows `caps:` and `join_opts:`.
- Linux connect order: `wpaie` (our RSN IE), wpa_auth, auth, wsec, `mfp` (only if caps has `mfp`, or a knob),
  `sup_wpa` 1, PMK, join.
- `sup_wpa` refused -> the host runs the 4-way handshake (`wifi_crypto::Supplicant`, already used by
  iwlwifi): EAPOL frames are taken off the RX queue, M2/M4 go out through tx_data, and PTK and GTK go in
  through "wsec_key" (`brcmf_wsec_key_le`, 164 bytes; pairwise ea=AP, group BRCMF_PRIMARY_KEY). Group rekeys
  while up are also handled (poll_link).
- PMK default = Linux `brcmf_set_pmk` (32-byte PSK, flags 0), then passphrase, then hex+PASSPHRASE
  (Linux 4.13 form). Each refusal is logged.
- Every refused command is logged as `join: <cmd> refused: <err>`. The ctl FAILED line names it:
  `FAILED Fw(Status(-23)) at 'iovar sup_wpa=1 -> Status(-23)' [method=.. supwpa=.. ...]`.
- Live knobs (persist; later autoconnects use them): `join [reset] [method=auto|joiniovar|setssid]
  [supwpa=auto|1|0] [pmk=auto|raw|hex|passphrase] [sec=auto|ioctl|iovar] [mfp=auto|0|1|2] [wpaie=1|0]
  [ssid=<rest of line>]`.

## Evidence
- brcmf-tests 63/63 (+5): a mock firmware that answers -23 to `sup_wpa` runs a full WPA2 4-way handshake
  against an in-test authenticator (M2 MIC checked, PTK/GTK in wsec_key match); forcing `supwpa=1` on the same
  mock fails exactly as the Pi did, with `fail_cmd` = `iovar sup_wpa=1 -> Status(-23)`; PMK form order; the
  method and sec knobs; wsec_key layout; ctl parse.
- check-rpi5.sh rpi5 + raspi4b clean (no brcmf warnings). aarch64 Mac nettest PASS; box aarch64 full regress 53 suites, 0 bad (incl. wifisimtest).

## Open
- Unproven on hardware: whether the -23 is `sup_wpa` (expected) or WLC_SET_WSEC_PMK, and the wsec_key layout
  on this firmware. If the host handshake gets to M3 but no data flows, try `wpaie=0` and `mfp=0`.
- WPA3/SAE still out of scope.

## Next Pi boot (`cat /proc/net/nic`, `dmesg brcmf`)
`caps: ... ` (shows whether `idsup` is listed); after `join`: `join: iovar sup_wpa=1 refused: Status(-23)`,
`no firmware supplicant ...; running the 4-way handshake on the host`, `host supplicant: 4-way handshake
complete`, `ctl join 'LEANDRO2026': OK`; counters `eapol_rx 2 eapol_tx 2 handshake host`.
