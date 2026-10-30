# lane/kmodwifi (kernel-module wave 2, W2-5) - 2026-10-30

Status: PARTIAL. W2-5a (Y-only radio class for iwlwifi, brcmf, SimRadio) DONE; W2-5b (IWLWIFI as a tristate / `modules/iwlwifi`) NOT started, handoff in orch/handoff/kmodwifi.md.

## What changed
- `drivers/src/wifi_radio.rs` (pure, module-compilable): `Radio` trait, `ScanHit`, `Fail`, `ConnCtx` moved out of wifi_mgr (re-exported there, drivers unchanged); `repr(C)` `KWifiRadioOps`, `KScanHit` (every `Bss` field, incl. RSN and rates), `KWifiFail`, `KConnCtx`, `KSsid`, `KLegacy`; driver-side adapter `register(Box<dyn Radio>, name, prefix, mac, kick, legacy) -> Result<Arc<AtomicBool> /* manager ended */, errno>`. It reaches the kernel through `kenv::wifi_register` (direct call in Y, `kapi_wifi_register` in a module).
- `drivers/src/wifi_class.rs` (kernel): `KRadio: Radio` over the table, registry, `register_raw`, `attach` (pending netctl attach by driver name, so the netif/radio order does not matter), `lookup`, `scan_lines`, `info_lines`. The kernel leaks one `Mgr` per radio and runs `wifi_mgr::run` on a `wifi-<prefix>` kthread. `Mgr` gained `set_mac`, `set_waker_raw` (kick callback+ctx).
- Drivers: `iwlwifi::manager`, `brcmf::netdev::run_ctl` (radio behind `SharedRadio` so the driver thread gets it back after the loop ends for the retry/bus hand-back path) and `wifi_sim` (`mgr()` resolves the kernel Mgr for its selftests) no longer own a `static MGR`. Legacy wifi.conf credentials are passed as (ssid, psk) bytes through `KLegacy`; never logged.
- `kernel/src/kmod/exports/wifi.rs`: `kapi_wifi_register(owner, ops, ctx, legacy) -> i32` (-22 bad table, -17 name taken, -12 thread, -19 no wifi stack). NEXPORTS 83 -> 84.
- ktest kmodkapi: ksymtab 84, null table -> -22, KScanHit round trip (RSN, rates, IEs), KWifiFail round trip.
- IWLWIFI=n stub: `scan_lines` walks the class registry.

## Deviations from the plan text
- Register signature is `(owner, ops, ctx, legacy)`; name/prefix/caps/MAC live in the table (`size` gates the layout).
- `rfkill_released`, `kick`, `stopped` were added to the ops table (the trait has rfkill_released; kick replaces `Mgr::set_waker`; stopped lets brcmf wait for the loop to end).
- `iwlwifi::scan_lines` stays the entry for /proc/net/wifi (mgr=0 table + TX lines); it defers to the registry when no Intel radio is present. procnet is unchanged.

## Evidence (aarch64, release)
- ktest=kmodkapi: all ok, 0 fail. First wifisim run FAILED (Bss rebuilt from IEs lost RSN: "privacy bit, no RSN element") - fixed by carrying every field; wifisimtest then passes (RC=0, 32 s).
- `--rpi5` build (brcmf) compiles.
- x86_64 build (IWLWIFI=y default) + regress: see merge report.

## Not done
- W2-5b: `config IWLWIFI tristate`, `modules/iwlwifi`, modules.alias pci entries 8086:24fd / 8086:24f3, slot 96/32, no-device load test, llvm-nm check. Blockers found: iwl_trans/iwlwifi use `crate::pci/irq/firmware/netdev::kick/cmdline_secrets/wifi_pref/wifi_api` directly (no kenv), and `wm::gk` (group-key statics), `wm::handshake_failure*`, `fmt_mac`, `pmk_for`, `connected_bg_scan_due`, `set_knobs` live in kernel-only wifi_mgr.rs and are used by the driver. See handoff.
- Module-owner cleanup of a registered radio (unregister on unload) - the radio thread pins the module for now.
- Laptop WPA2 connect + throughput trial (orchestrator).
