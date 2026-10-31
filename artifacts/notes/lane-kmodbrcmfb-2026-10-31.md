# lane/kmodbrcmfb (B2): brcmfmac as a loadable module

Status: READY FOR PI TRIAL (PARTIAL until the Pi trial confirms the `[brcmf]` chip/fw/clm/ver lines).

- Kconfig `BRCMF` is a tristate (still `select WIFI_STACK`, which rounds to y; default y). kernel/Cargo.toml `mod-brcmf`; workspace member `modules/brcmf`; alias `of: brcm,bcm4329-fmac`; slot brcmf 128/32 (existing, verified).
- modules/brcmf path-includes `drivers/src/brcmf/mod.rs` and `brcmf_boot.rs` (plus wifi_pure/radio/frame/ht/crypto, as iwlwifi does) over a module-side `kenv`: SDIO2 mux and WL_REG_ON through `kapi_pi5_pins`, board MAC through `kapi_board_mac`, DTB through `kapi_dtb_blob`, firmware through `kapi_fw_*`, quiesce through `kapi_pm_register2` (phase WIFI). The module crate carries the driver's `rpi5` feature by default.
- Driver seam edits (built-in behaviour unchanged): brcmf/netdev.rs, sdio_host.rs, brcmf_boot.rs import `sched`/`mm` through `crate::kenv`, log through `kenv::serial_raw`, and read the DTB through `kenv::dtb_blob`.
- init: `kapi_soc_stepping()` unavailable (-19, not a BCM2712) returns -19 before any pin is touched. Otherwise: `brcmf::netdev::init`, register netif `wifi`, register the WIFI-phase shutdown hook, spawn kthread `brcmf-probe` that runs `sdio_host::boot_probe()` then `brcmf_boot::start()`. init returns 0 at once (a missing chip shows as `[brcmf] fail ...`, not -19; the module stays loaded).
- init.rs `drv-brcmf` sites (:154/:349) need no change: `drv-brcmf` is off for =m, so the kernel neither probes nor starts.
- Pi trial build: `printf 'CONFIG_MODULES=y\nCONFIG_BRCMF=m\n' > .config; ./scripts/build-all.sh --rpi5; rm .config`.
- Pi expectations: `[kmod] alias of brcm,bcm4329-fmac brcmf` then `[sdio]` and `[brcmf]` chip/fw/clm/ver lines; wifi0 joins about 350 ms later than =y (WL_ON cycle after BT start). Do not run the trial over wifi0 (use eth0 192.168.2.4, macb=y).

## Evidence
(filled in below)
