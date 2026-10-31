# lane/kmodmacbb (M2, kmodpi) 2026-10-30 - READY FOR PI TRIAL (=m)

MACB is tristate (default y, unchanged). `=m` builds `modules/macb` from the same `drivers/src/macb.rs` + `macb_hw.rs`.

- Kconfig `MACB` tristate; kernel/Cargo.toml `mod-macb`; workspace member `modules/macb`; alias `of: raspberrypi,rp1-gem`; slot macb 288/8 (existing).
- With =m the kernel has no `drivers/macb` feature: rp1.rs already only announces `raspberrypi,rp1-gem` (cfg feature macb).
- modules/macb/src/lib.rs shims `kenv`, `mm` (dma_cache -> kapi_dma_sync/coherent, kva::vmap -> kapi_vmap), `irq`, `pci`, `netdev`, `platform` (KPlatDev -> PlatDev trampoline).
  `init`: `macb::register()` (replays the announcement into probe -> rp1_attach), `macb::init(cmdline)`, -19 if not `present()`, then `kapi_netdev_register("eth")`.
- kexec stop: `kenv::dma_quiesce_register` -> `kapi_dma_quiesce_register` (device stage), not KPmOps. No module pm hooks.
- rp1 irq: transmuted `macb::rp1_isr` through `kapi_rp1_irq_request`.

Evidence: `--rpi5` with CONFIG_MODULES=y CONFIG_MACB=m builds; kmodcheck ok (slot 0xfffffe0004800000+0x200000), bidcheck ok, kmodpost guards pass, macb alias in modules.alias.
Not done: Pi trial (link/DHCP over the module, kexec out), aarch64 virt `modules.load=macb` -> -19 case (needs the box/virt run).
Module is never unloaded (no exit); the irq is not freed.

## Pi 5 trial (2026-10-31, orchestrator)

Trial #1 a04106babb (rpi5 defconfig + MODULES=y MACB=m, built after d4c8a459):
- root f2fs mounted, wifi0 reachable over SSH.
- `[kmod] of raspberrypi,rp1-gem -> macb`, `macb loaded id 1`, `/proc/modules`: macb Live.
- dma selfcheck agree; PHY BCM54213PE id 0x600d84a2 at 1, rgmii-id; `eth0: stack up (macb)`;
  no link (no cable on the Pi).
- kexec trial -> good: `[kexec] quiesced: dma40 xhci0 xhci1 gem` (module's DMA-quiesce hook ran),
  good 4ad839207a back after 32 s. Not promoted (=m trial).
