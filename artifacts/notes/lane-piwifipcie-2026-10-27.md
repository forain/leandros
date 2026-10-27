# lane/piwifipcie 2026-10-27 - Pi 5 Wi-Fi handshake fails with pcie.rpi5=1

Status: PARTIAL (leading hypothesis REFUTED in code; diagnostics added for the next Pi boot).

## Refuted: EAPOL rides the wrong netif / MAC
- Host supplicant (`drivers/src/brcmf/cfg.rs` `host_eapol`) builds msg 2/4 itself and sends them with
  `fw.tx_data` straight to the CYW43455 (BDC header ifidx 0, `bcdc.rs`): no IP stack, no netctl ifindex,
  no netif TX queue. eth0's broken TX queue cannot hold them.
- Own MAC (SPA) = firmware `cur_etheraddr` read at init; frame SA and PTK derivation use the same value.
- PMK comes from the profile secret (wifi_mgr `pmk_for`), not keyed by ifindex. SNonce RNG is non-blocking.
- SDIO host is PIO-polled (no GIC line, so no MIP/MSI SPI overlap).
- New host test `host_handshake_ignores_the_netctl_ifindex_and_uses_the_firmware_mac` (wifi0 ifindex 2 vs 3:
  same M2/M4, SA = firmware MAC). brcmf-tests 77/77.

## What the failing log does say (pcieboot-a9a61b2b)
- Join 1 and 3: `handshake_evidence` => msg 2 WAS sent (eapol_sent > 0) and no msg 1 retry was seen after it;
  no "message 3 MIC failure / ANonce mismatch" error was logged, so msg 3 never reached `host_eapol`.
  i.e. after our first data TX no further EAPOL arrived (neither msg 3 nor msg 1 retries). Join 2: AP assoc reject
  status 1 (likely AP-side state after join 1).
- Open candidates: data RX over SDIO stalls/drops after the first data TX when PCIe/RP1 is up
  (CPU contention on PIO SDIO, rx queue drops), or msg 2 lost in the air. Needs the new counters.

## Added logging (next Pi boot should print)
- At msg 1: `[brcmf] host supplicant: message 1 from <AP>; EAPOL tx via firmware data path (BDC ifidx 0, not the
  IP stack), own MAC 2c:cf:67:d8:ca:1b (fw cur_etheraddr), wifi0 netctl ifindex 3, data frames rx N dropped D (since boot)`
- On failure: `[brcmf] host supplicant: join ended without keys: eapol rx R tx T (this join), message 1 after message 2: K, data frames rx N dropped D (since boot)`
  R=1,T=1 => nothing after msg 2 arrived; rising `dropped` => RX queue overflow; R>1 with K=0 => msg 3 seen but not answered.
