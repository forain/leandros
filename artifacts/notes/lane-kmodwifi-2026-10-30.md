# lane kmodwifi (W2-5a/5b): IWLWIFI as a loadable module, 2026-10-30

W2-5a (merged earlier): Wi-Fi radio class, kernel-owned manager, iwlwifi/brcmf/sim on the radio table.

W2-5b (lane/kmodwifi2): IWLWIFI is now a tristate.
- modules/iwlwifi builds the same driver source through shim modules (modules/iwlwifi/src/lib.rs): sched poll-key emulation over LinkMon, firmware, cmdline_secrets, pci/irq/netdev, wifi_class/wifi_mgr stubs.
- drivers/src/wifi_pure.rs holds the pure helpers (consts, handshake_failure*, pmk_for, connected_bg_scan_due) and the gk counters; wifi_mgr re-exports them.
- kapi_secret_peek also serves a module named iwlwifi; kapi_netdev_register attaches the netctl side for KNET_WIFI netifs; kapi_wifi_register applies the wifi.* knobs.
- No new exports (NEXPORTS 87 after merging main's input kapi).
- autoloadtest.sh case `iwlwifi` (KMOD_AUTOLOAD forces the load; no device => init -19, slot freed).

Behaviour notes
- With a device present the radio thread pins the module; unload is only clean with no device.
- gk counters: the module keeps a module-local copy; the kernel copy is used by wifi_sim. /proc/net/wifi shows the registry radio plus gk lines via netif info only.
- PCI id 24f4 is also aliased.
- llvm-nm on libkmod_iwlwifi.a: the only data statics outside the driver itself are kmod_kapi::OWNER, kmod_rt::OWNER, spin::held::HOOK (per-module runtime state); no copy of kernel-global state beyond the documented gk copy.

Evidence (merged tree): ktest=kmodkapi PASS on x86_64 and aarch64 (ksymtab 87); autoloadtest iwlwifi, wifisim and box aarch64 regress: see merge report. Laptop trial is left to the orchestrator.
