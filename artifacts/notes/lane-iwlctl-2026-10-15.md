# lane/iwlctl (L3): Wi-Fi connection manager + wifi_sim — 2026-10-15

Branch `lane/iwlctl`, which is main `8cf70b02` plus lane/ncabi `29db18a3` (the netctl-abi crate and the `wifi_api` seam). Spec: `netmgr-spec.md` decision 4, lane L3. WPA3/SAE/PMF stays deferred, so this lane does WPA2-PSK only. AKM selection is a single function, so SAE can be slotted in later.

## What landed

- **`drivers/src/wifi_mgr.rs` (new)**: the `WifiBackend` and its state machine. The real driver and the simulator share this code, so the QEMU tests exercise the same policy the laptop runs.
  - `Mgr` provides:
    - a command queue (`submit` never blocks, and queued scans are merged);
    - the published `NcWifiState`;
    - a BSS cache with aging;
    - the last scan;
    - autoconnect bookkeeping: the wrong-key block, back-off of 10/30/60 s, the disconnect block, and the last good profile;
    - a 256-entry event ring (`events_since`), used by the self-test and for debugging.
  - `Radio` is the trait a driver implements: `scan`, `connect`, `poll_connected`, the fast `leave`, the slow `settle`, `idle`, `set_radio`, `hw_rfkill` and `rfkill_released`.
  - `run()` is the loop. Rules:
    - **Disconnect order.** `leave` runs first (deauth, data path closed), then DISCONNECTED is emitted, and only after that the slow `settle` (firmware clean-up).
    - **Preemption.** Connect, Disconnect and radio-off preempt an attempt in progress through `ConnCtx::aborted()`, which is polled at every wait.
    - **Aging.** BSSs age only when a scan completes. A radio that cannot scan while associated therefore never empties its list.
    - **Coalescing.** A scan request within 10 s of the last scan is answered from the cache, and so is a FORCE request within 2 s.
    - **Every scan request ends in exactly one SCAN_DONE.** When the radio cannot scan while associated, it is a COALESCED one.
    - **Autoconnect order.** Highest priority wins, then the last-good profile, then the stored timestamp, then the signal. Profiles come from `wifi_api::with_profiles`. While netctl has never published (generation 0), the driver's fallback is used: the legacy wifi.conf / `wifi.ssid=` network.
    - **Directed probes.** Background scans probe for the SSIDs of the autoconnect profiles, as the old driver always did for its one network. A hidden home network needs this.
  - `select_akm(want, ext, rsn_flags, privacy, caps)`:
    - AUTO tries SAE, then PSK-SHA256, then PSK, each only when both sides can.
    - The caps-gated SAE row is already in place, so WPA3 later means adding `wifi_caps::SAE` and the radio's SAE path.
    - `bss_to_nc` fills in NM's flag bits exactly, plus `bss_ext` (SAE, MFPC/MFPR, TRANSITION, HIDDEN, JOINABLE…).
  - `handshake_failure(msg2_sent, deauth_reason)` is the wrong-key classifier, shared by both radios:
    - message 2 was sent, then reason 15/14/23/2/1 or silence → **WRONG_KEY**;
    - no message 1 at all → TIMEOUT;
    - any other deauth → DEAUTH_BY_AP.
- **`drivers/src/iwlwifi.rs`**, with `iwlwifi.mgr=1`:
  - **Mode switch.** After firmware bring-up, `driver_thread` hands over to `manager()` (`IwlRadio` + `wm::run`). `mgr=1` implies `start=1`.
  - **mgr=0 is the old fixed loop, unchanged** (wifi.conf / cmdline autoconnect, restart after every failure).
  - **`connect_steps`:**
    - returns `Fail` with real reasons (AUTH/ASSOC_REJECTED with `ieee_status`, TIMEOUT, WRONG_KEY, DEAUTH_BY_AP, FW_ERROR with `fw_assert`);
    - polls an abort closure, waiting for the 4-way in 100 ms slices;
    - reports ASSOCIATING/HANDSHAKE;
    - supports open networks (no RSN IE, no handshake; unverified on hardware);
    - records what it set up in `Prog`.
  - **`IwlRadio::leave`** clears the data path and sends a deauth (reason 3) if authenticated. It takes about 20 ms.
  - **`settle`** does an iwlmvm-style **soft teardown**: time event removed, TXPATH_FLUSH, MAC context unassociated, quota none, SCD queues disabled, REMOVE_STA, unbind. Any error falls back to `restart_rt`. A firmware assert means a restart, and 3 asserts in a row mean the device is given up. `iwlwifi.softdisc=0` forces a restart every time.
  - **Scanning while associated** is off by default (`iwlwifi.scanconn=1` enables it). With it off, requests while connected are answered from the cache.
  - **Signal** comes from the RSSI of the tracked BSS's beacons.
  - **Radio off** stops the device; radio on runs `restart_rt`. A hardware RF-kill is reported as RADIO_OFF/RFKILL, and its release restarts the firmware.
  - **/proc output.** /proc/net/nic gets `manager:` / `wifi_state:` lines. With the manager on, /proc/net/wifi shows the aged cache (with JOIN and AGE columns). With no Intel radio, it shows the sim's list.
- **`drivers/src/wifi_sim.rs` (new)**: a simulated wifi0.
  - **Activation** is opt-in: a virtio-net NIC with MAC prefix `02:57:53`, or `wifisim=1`. `wifisim=0` forces it off.
  - **APs** come from `wifisim.aps=` or a default set of 7 (PSK, 5 GHz PSK, SAE-only, transition, open, hidden, refuses-association).
  - **Handshake.** The STA side is the real `wifi_crypto::Supplicant`. The AP runs the authenticator with its own PMK. With a wrong key, message 2's MIC fails, the AP retransmits message 1 three times at 300 ms intervals, and then deauths with reason 15, as on air.
  - **Data path.** Once associated, the data path is the second NIC (a SLIRP 10.0.3.0/24 user net). Without such a NIC it is a sink.
  - **Self-test.** `wifisim.selftest=1|2` runs the seam self-test at boot. Level 2 adds a raw DHCP DISCOVER→OFFER exchange over the data path.
- **`drivers/src/netdev.rs`** (owned by L1; minimal edit): calls `wifi_sim::init`, then attaches the seam with a provisional ifindex 3, named "wifi0" (sim first, else iwlwifi with mgr=1). L1 replaces this, as described in "For L1" below.
- **Test plumbing:**
  - `.claude/skills/run-leandros/wifisimtest.py` (host). It boots a throwaway image copy with `wifisim.selftest=2` and the sim NIC, waits for the verdict, then checks /proc/net/wifi from a root shell.
  - `runtests.py` runs it last in `regress`, and alone as `--suite wifisim`.
  - `driver.py` gained `LEANDROS_DISK_IMAGE`, which boots another copy of the boot image.
  - SKILL.md documents it.

## Results

See the table at the end, which is filled in from the runs.

## For L1 (multiif): wiring wifi0

1. In `netdev::init`, after `virtio_net::init` and `iwlwifi::init`, call `crate::wifi_sim::init(cmdline)`.
2. Skip the virtio devices that `wifi_sim::claims_virtio(i)` claims when registering `eth*`.
3. Then register:
   - `if wifi_sim::present() { register(Kind::Wifi, "wifi", &WIFISIM_OPS, 0); wifi_sim::attach(ifindex, name) }`
   - `if iwlwifi::present() { register(Kind::Wifi, "wifi", &IWL_OPS, 0); iwlwifi::attach(ifindex, name) }`

   `iwlwifi::attach` registers the seam only with `mgr=1`.
4. Ops for the sim:
   ```
   WIFISIM_OPS = { driver: "wifi_sim", send: wifi_sim::send_packet, recv: wifi_sim::poll_receive,
                   rx_rearm: wifi_sim::rx_rearm, rx_irq_driven: false, mac: wifi_sim::mac_address,
                   link: (wifi_sim::link_up(), 54, true), link_gen: wifi_sim::link_generation,
                   info: wifi_sim::info_lines }
   ```
5. Drop this lane's 4-line provisional block in `init`.
6. `LEANDROS_NIC2`: use MAC `02:57:53:xx:xx:xx` for the NIC that should become the sim's data path.

## For L2 (netctl)

- **Attempt ids.** Autoconnect attempts get kernel-assigned ids with **bit 31 set** (`wifi_mgr::AUTO_ATTEMPT_BIT`). Keep CONNECT ids below 2^31.
- **Profile generation.** Publish profiles at least once at init (`gen >= 1`), with the legacy profile merged. Generation 0 means "never published", and the driver then uses its own legacy fallback.
- **`BLOCKED_WRONG_KEY`.** It is not exposed by the trait. Derive it from WIFI_STATE FAILED(WRONG_KEY)+profile_uuid, and clear it on CONNECT of that uuid or on RELOAD. Alternatively, see the ABI request below.
- **ENETDOWN.** `submit` answers ENETDOWN for Scan/Connect while UNAVAILABLE or radio-off. The NM shim must still return success for RequestScan.
- **Disconnect before firmware is up.** A Disconnect is accepted before the firmware is up and blocks autoconnect once the firmware is ready.

## ABI change requests (L0)

1. *(optional)* `WifiBackend::profile_flags(&self, uuid) -> u8` (BLOCKED_WRONG_KEY), so netctl does not have to track it.
2. None blocking. Note that SAE_H2E / H2E_ONLY in `bss_ext` are never set: `wifi_frame::Bss` does not keep the RSNXE. Fixing that belongs to the SAE lane in wifi_frame.rs, not to the ABI. The WPA1 IE suites are not parsed either, so `wpa_flags` is approximated as PSK/TKIP.

## Firmware-dependent unknowns (8265, API 36; laptop only)

- **Soft teardown.** It depends on the TXPATH_FLUSH v1 layout and its response, SCD_QUEUE_CFG disable, REMOVE_STA and BINDING remove with all-invalid MACs, and on a reconnect working afterwards without a restart. The fallback is an automatic restart, and `iwlwifi.softdisc=0` forces it.
- **Deauth TX** on the management queue just before teardown.
- **Scan while associated** (`iwlwifi.scanconn=1`): UMAC scan v7 on the associated MAC, and data-path behaviour during it.
- **Open (no-RSN) networks**: association without the RSN IE, and the RX status of unprotected data.
- **Radio off/on** through `stop_device` + `restart_rt`, and firmware restart after an RF-kill toggle at runtime.
- **Beacon RSSI** of the tracked BSS as the reported signal.

## Laptop checklist (EliteBook 840 G5, stick; `laptop-deploy-kernel`, then `laptop-remote-debug`)

Use a profile-free stick with wifi.conf, as today. Never put the passphrase in logs or notes.

**mgr=0 (default cmdline), must be unchanged**

1. Boot → `dmesg | grep iwlwifi` shows "credentials from /etc/leandros/wifi.conf", then "4-way handshake complete", then "wlan: link up". SSH is reachable before login.
2. `/proc/net/nic` has no `manager:` line.

**mgr=1 (`iwlwifi.mgr=1` added through laptop-deploy-kernel's cmdline A/B)**

3. Boot. Expect "manager running", "autoconnect: '<home>'" and "connected to '<home>' in N ms". **SSH is reachable before login.** Record N.
4. `cat /proc/net/wifi`: the AP list with JOIN/AGE columns; `/proc/net/nic`: `wifi_state: connected`.
5. When L2 is merged, run `netctl scan` while connected. Expect a COALESCED SCAN_DONE and no link drop.
6. With `netctl disconnect`, expect "disconnected … on request in N ms", with N < 2000 (target < 100). Then expect "left the BSS without a firmware restart (M ms)". If you see "teardown failed … restarting" instead, record the error.
7. `netctl connect <home>` with the right key: connected. Time it. This is the reconnect after a soft teardown, which is the key unknown.
8. `netctl connect <home>` with a **wrong** key: FAILED wrong-key within about 6 s. The log shows "the AP ended the handshake (reason 15)" or "timed out after message 2". The link then comes back through autoconnect only if another profile exists. Run `netctl connect` with the right key again.
9. Radio: `netctl radio off` gives RADIO_OFF, and the AP list is empty. `netctl radio on` gives DISCONNECTED, then a scan, then autoconnect back. Check SSH.
10. Autoconnect after reboot: reboot and repeat step 3, with profiles in `/etc/leandros/network` (L2), then without (legacy fallback).
11. Optionally, `iwlwifi.scanconn=1`: `netctl scan` while connected gives a fresh scan and a live link. Watch for firmware asserts.
12. Optionally, `iwlwifi.softdisc=0`: a disconnect is still < 2 s, but the next connect is slower (firmware restart).
13. Always: `dmesg | grep -i 'firmware failure\|assert'` is empty.
