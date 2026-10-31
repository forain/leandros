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
- `--rpi5` default (BRCMF=y) builds. `--rpi5` with CONFIG_MODULES=y CONFIG_BRCMF=m: brcmf.kmod links (slot 0xfffffe0002000000+0x800000), kmodcheck + bidcheck pass on both arches, modules.alias has `of brcm,bcm4329-fmac brcmf`; target/kconfig-aarch64/.config has BOARD_RPI5=y, SDHCI=y, WIFI_STACK=y, BRCMF=m.
- Plain aarch64 virt, MODULES=y BRCMF=m, `kmodctl load /lib/modules/<bid>/brcmf.kmod`: `[kmod] brcmf init failed -19, slot freed` (no pin touched).
- ktest=kmodkapi aarch64 PASS (0 fail).
- Box regress lane/kmodbrcmfb aarch64 (efe8c2f1): overall PASS, 844 s; `--gc` run.
- target/ holds the `--rpi5` MODULES=y BRCMF=m build (root .config removed).

## Autoload fix (kmodbrcmfb2)
- Bug on the Pi: with BRCMF=m the `brcm,bcm4329-fmac` sdio2 `wifi@1` child was never announced (scan_dt did not see it), so no `[kmod] alias` line. Chosen fix (smaller than a generic SDIO-child scan): `drivers::platform::announce_if_in_dtb` (any-status DTB walk) called from kernel/src/init.rs just before `scan_dt`, only under `rpi5 && !drv-brcmf`; prints `[plat] announced brcm,bcm4329-fmac`. BRCMF=y and non-rpi5 builds are unchanged.
- Evidence: `--rpi5` default and `--rpi5` MODULES=y BRCMF=m both build; the =m kernel contains the `announced brcm,bcm4329-fmac` string and brcmf.kmod. Not run: ktest=kmodkapi (QEMU image was locked by a concurrent suite) and the final target/ rebuild of the =m kernel.

## Pi 5 trials 2026-10-31

- efe8c2f183 (BRCMF=m): module never autoloaded. scan_dt does not announce the nested sdio2 `wifi@1` node.
- 1b010a2d9e (announce from the boot DTB): still no autoload. After kexec the DTB is synthetic
  (`boot/src/fdt_build.rs` writes no Wi-Fi node; `[plat] DT compatibles 9`), so `fdt_has_compat` is false.
- bda9b38f1e (announce on every Pi 5 boot, `announce_board`): PASS. `[plat] announced brcm,bcm4329-fmac (board)`,
  `brcmf ... Live`, wifi0 DHCP ok, root f2fs (vdb).
- All-modules image bda9b38f1e-T1104Z (BRCMF=m MACB=m HDMI_AUDIO=m, built on lane/piallm = this branch): PASS.
  macb and brcmf Live; `of raspberrypi,rp1-gem -> macb`, PHY BCM54213PE, eth0 stack up (no cable, so no link);
  `alias of leandros,pi5-hdmi-audio hdmi_audio: Init(-19)` (no sink), slot freed; wifi0 DHCP ok; root f2fs.
  pi-boot good returned to 4ad839207a. Not promoted.
- One kexec trial→trial (B2 → all-m) hit the known aarch64 kexec straggler and reset to the card kernel; the retry
  from the card kernel worked (see lane-pikexec5-2026-10-31.md).
