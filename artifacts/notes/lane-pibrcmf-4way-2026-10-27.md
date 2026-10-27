# lane pibrcmf-4way (2026-10-27): Pi 5 host 4-way handshake ends with reason 15

Status: FIXED in code (host-tested); which of the three causes the AP objects to is decided on the next Pi boot,
which now shows the exact elements and EAPOL fields involved.

## Symptom (real Pi 5, main fe419b1d)
Firmware has no `idsup`, the host runs the handshake, msg1 arrives three times, we send msg2 three times, the AP
deauths with reason 15 (4-way handshake timeout). An AP drops a msg2 whose MIC does not verify and retransmits msg1,
so reason 15 after three msg2 points at the MIC (PTK/PMK), not at an IE mismatch (that would be reason 17).
`mfp=0` then gave assoc status 1; that may also be the AP backing off after failed handshakes, so retry it after a pause.

## What changed
- `wifi_crypto` (shared with iwlwifi): SHA-256, HMAC-SHA256, IEEE 802.11 KDF-SHA256, AES-128-CMAC;
  `derive_ptk_sha256`; the EAPOL MIC follows the key descriptor version (2 = HMAC-SHA1-128, 3 = AES-CMAC); the
  supplicant accepts v2 and v3, answers in msg1's version, and uses KDF-SHA256 for AKM 6 (`set_akm`) or v3; IGTK KDE
  (`00-0f-ac:9`) -> `HsEvent::InstallIgtk` from msg3 and group rekeys. `rsn_ie_psk_akm(group, akm, caps)`.
  The iwlwifi path is unchanged except that it ignores the new event.
- brcmf: the scan's BSS list is kept; at join time the AP's RSN element is decoded (group/pairwise/AKMs/caps/PMKID
  count/group mgmt cipher). Auto choice: AKM 6 if offered (MFPC=1, `wpa_auth` 0x8000), else AKM 2 with MFPC if the
  AP has MFPC and the firmware has `mfp`. MFPR stays 0. `mfp` iovar = 2/1/0 from MFPR/MFPC. TKIP group is echoed.
- Message 2 now carries the RSN IE of the association request the firmware actually sent ("assoc_info" +
  "assoc_req_ies", as Linux brcmf_get_assoc_ies and wpa_supplicant do); a difference from our IE is logged.
- IGTK installed through "wsec_key" at index 4/5 (like brcmfmac); a refusal is logged and not fatal.
- Knobs: `join akm=auto|2|6 mfpc=auto|0|1 mfpr=auto|0|1 msg2ie=auto|ours|fw` (with the earlier ones).
- /proc/net/nic: `rsn_ap`, `rsn_chosen`, `rsn_assoc_ours`, `rsn_assoc_fw`, `rsn_msg2_ie`, `eapol_msg1`, `eapol_last`
  (with the supplicant error, e.g. "message 3 MIC failure"), `secret` (kind, length, edge whitespace only). No key
  material: a test asserts no PMK bytes and no passphrase in these lines.

## Evidence
- iwlwifi-tests 173/173 (+7): SHA-256 FIPS 180 vectors, HMAC-SHA256 RFC 4231 cases 1 and 6, AES-CMAC RFC 4493
  examples 1-4 (also split input), KDF-SHA256-384 PTK against an independent Python hmac/hashlib reference,
  full v3 handshake with IGTK (CMAC of msg2 checked independently), AKM 6 forces KDF-SHA256, v1 refused.
  Existing crypto/tkip/wifisim-related tests unchanged and green.
- brcmf-tests 66/66 (+3): RSN element decode; auto/knob choice of AKM, MFPC, `wpa_auth`, `mfp`; AKM 6 host
  handshake where the firmware's assoc IE differs from ours (caps 0x008c) and msg2 must echo it, PTK/GTK/IGTK keys.
  The v3 test fails on the old code (it refused descriptor version 3).
- check-rpi5.sh rpi5 + raspi4b clean. Box full regress: see the merge commit.

## Next Pi boot
`cat /proc/net/nic` after `brcmfctl join`: `rsn_ap: group=4 pairwise=4 akm=... caps=0x.... mfpc=. mfpr=.`,
`rsn_chosen: akm=6|2 ...`, `rsn_assoc_fw: 30..` (compare with `rsn_assoc_ours`), `eapol_msg1: ... (ver=2|3 ...)`,
`secret: passphrase len=N edge_whitespace=0`. Success: `host supplicant: 4-way handshake complete`,
`ctl join 'LEANDRO2026': OK`, `eapol_rx 2 eapol_tx 2`, three wsec_key calls (pairwise, group, igtk 4) under PMF.
If `eapol_msg1` shows ver=2, `rsn_assoc_fw` equals what we sent, and it still fails with reason 15: check `secret`
(length and edge whitespace) against the provisioned passphrase. That is cause (c).
