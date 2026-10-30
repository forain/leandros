# lane/kmodmacba (M1, kmodpi) 2026-10-30 - READY FOR PI TRIAL (=y refactor)

macb stays built in; the GEM now reaches the driver through the platform registry and the kenv seam.

- rp1.rs: `attach_gem` pulses the PHY reset (unchanged, gpio32), then announces `raspberrypi,rp1-gem`:
  reg0 = BAR1 phys + 0x10_0000 (0x4000), reg1 = eth_cfg (0x20, 0 if BAR1 too small), dma_offset 0x10_0000_0000,
  irq0 = RP1 source 6, flags bit0 = `rp1.irq=1`. It calls `macb::register()` first (built-in claim).
- macb.rs: `register()` -> `platform::register(["raspberrypi,rp1-gem"], probe)`; `probe` maps reg0/reg1 (kenv::ioremap_uc),
  requests the RP1 irq (kenv::rp1_irq_request), then the old `rp1_attach`. `macb::init` (from netdev::init) is unchanged.
  MAC from `kenv::board_eth_mac`, DTB from `kenv::dtb_blob`, kexec stop via `kenv::dma_quiesce_register("gem", ..)`
  (the dma_shutdown table the kapi_dma_quiesce_register export also fills; not KPmOps). `MACB_OPS` moved from netdev.rs into macb.rs.
  mm/sched now come from `crate::kenv`.
- kenv.rs: ioremap_uc, board_eth_mac, dtb_blob, rp1_irq_request, dma_quiesce_register.
- New exports `kapi_rp1_irq_request(owner,n,level,h,ctx)` / `kapi_rp1_irq_free(owner,n)` (exports/rp1.rs; -19 off Pi 5, -22 bad args),
  kmod-abi `Rp1IrqRequestFn/Rp1IrqFreeFn`, kmod-kapi `plat::rp1_irq_request/free`. NEXPORTS 132 -> 134 (docs updated).
  If lane kmodhdmia merges first: `git merge main`, recount NEXPORTS (ksymtab length is checked by ktest).

Evidence: builds ok aarch64 and `--rpi5` (kernel has macb::probe); ktest=kmodkapi PASS on aarch64 HVF (134 exports, 4 new rp1 cases);
box aarch64 regress: see report. Not done: Pi hardware trial (orchestrator), ioremap of reg0/reg1 is a second Uncached mapping of the
BAR1 sub-range (rp1's own BAR1 map stays).
