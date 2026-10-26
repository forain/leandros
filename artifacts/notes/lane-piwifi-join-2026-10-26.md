# lane/piwifi-join — 2026-10-26 — DELIVERED (untested on hardware)

Joins brcmf_boot (firmware to "ver") with brcmf::netdev (wifi0 + wifi_mgr). Commit 680ac752.

- Hand-off: `brcmf_boot::run` calls `brcmf::netdev::run(dev)` on its own `brcmf` kthread after "ver"
  (Brcmf<SdioHost> implements Fwil). Device is parked for `take_device()` only when wifi0 was not registered.
- Ordering: NOT a hazard. `brcmf::netdev::init` sets present() unconditionally in rpi5 builds (unless
  brcmf.disable=1) inside `netdev::init` (kernel init.rs:249), before registration; root is mounted by
  userspace init later, and brcmf_boot waits for root_ready, so present() is final before the hand-off.
  wifi0 exists from boot with the radio down (mac None) until `init ok`, same as iwlwifi pre-firmware.
  No change made (least invasive). Gating present() on sdio PROBE_STATE==1 was considered and skipped.
- Autoconnect: same path as iwlwifi.mgr=1: netdev::run loads /etc/leandros/wifi.conf via
  netctl_abi::profiles::{parse_legacy_wifi_conf, legacy_profile} into MGR.set_fallback_profiles (buffer wiped);
  the net server's netctl also merges wifi.conf into published profiles (servers/net/src/netctl/profiles.rs).
- Tests: check-rpi5.sh rpi5 + raspi4b OK; brcmf-tests 44/44 (wifi.conf parsing already covered by
  netctl-abi legacy_profile_from_wifi_conf; the ordering lives in kernel init and is not host-testable);
  aarch64 virt /bin/nettest PASS, only `[NET] netif eth0 = virtio-net` (no wifi0).

## Next Pi boot should show
`[NET] netif wifi0 = brcmfmac` → `[brcmf] chip 4345 ...`/`fw ...`/`clm ...`/`mac ...`/`ver ...` →
`[brcmf] init ok: mac d8:3a:dd:.. country PT` → `[brcmf] credentials from /etc/leandros/wifi.conf: SSID '..'` →
`[brcmf] associated with xx:..` → `[NET] wifi0 link up: DHCP discover restarted` → `[NET] DHCP configured, address: ...`.
Open: late-MAC handling in the net server relies on LINK_GEN bump (as pibrcmf-net designed); unverified on HW.
