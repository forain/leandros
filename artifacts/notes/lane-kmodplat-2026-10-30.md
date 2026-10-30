# lane/kmodplat (P0, kmodpi) 2026-10-30 - DELIVERED

Platform registry for modules plus board services. No driver became =m; default behaviour unchanged.

- drivers/src/platform.rs: PlatDev grows to the KPlatDev shape (regs[4], irqs[4], nirq, dt_node); module drivers via
  `register_ext(owner, compats, probe, ctx)`; `unregister_owner` unclaims what the owner claimed; every device records
  `hooked`. `set_autoload` now calls `replay_unclaimed()`, so board announcements made before the hook (rp1, hdmi_probe)
  reach the autoloader once. `swap_autoload`/`is_claimed` are ktest helpers.
- drivers/src/dma_shutdown.rs: `register_ext` (ctx word, owner), `unregister_owner`, `has`.
- kmod-abi `plat.rs` (KPlatDev, KPlatReg, probe/quiesce types, sys_plat externs, RegKind PLAT=15 DMAQ=16 (KMS=14 on main)); kmod-kapi `plat` wrappers.
- kernel exports/plat.rs: kapi_plat_register/unregister, kapi_dtb_blob, kapi_board_mac (-19 off Pi 5), kapi_soc_stepping
  (0 C0, 1 D0, 2 unknown, -19 off Pi 5), kapi_serror_checkpoint (0 clean), kapi_vmap/kapi_vunmap, kapi_dma_quiesce_register
  (-17 duplicate name). `kapi_dma_sync` dirs 3 (raw clean) and 4 (raw invalidate), valid on HHDM and kva-window addresses
  (no-op off aarch64). registry release_one drops PLAT and DMAQ records on failed init.
- NEXPORTS 123 (main) -> 132 (ktest literal updated, docs/kernel/modules.md).
- Not done: no board code announces through the registry yet (no caller changed); the replay is exercised by ktest.
  There is no module unload, so "unload" is tested through registry::release_all (the failed-init path).

Evidence: ktest=kmodkapi PASS aarch64 (HVF); builds ok: aarch64, --rpi5, x86_64. Owner ids 60020-60022 used by the new
cases (60010 is counted by kmodkapi_w2; a first run failed on that collision).
