# lane/kmodbrcmfa (B1, kmodpi) 2026-10-30 - READY FOR PI TRIAL

brcmf (=y) moved onto the kenv seam; default behaviour unchanged.

- drivers/src/kenv.rs: `kenv::pi5` (rpi5 only: `sdio2_pinmux()` log text, `wl_reg_on`, `wl_reg_on_ready`, `wifi_mac`) now carries
  every direct `bcm2712_gpio` / `bcm2712_id` / `bcm2712_otp` use of brcmf (sdio_host.rs, brcmf_boot.rs). `kenv::pm`: drivers-side
  shutdown-hook table (`register_shutdown(name, phase, fn)`, replace-by-name, `unregister`, `shutdown_hooks(phase)`).
- brcmf registers its own shutdown hook (phase WIFI) at the top of `brcmf_boot::start`; pm2 no longer has a built-in brcmf Dev,
  it walks `kenv::pm::shutdown_hooks(p)` before the module hooks of each phase (compile-time assert PHASE_WIFI == pm_phase::WIFI).
  The module build (B2) registers through the existing per-owner `kapi_pm_register2` (dropped on failed init by the registry).
- New export `kapi_pi5_pins(owner, op, arg, out)` (exports/pi5pins.rs; ops MUX_SDIO2, WL_ON_SET, WL_ON_READY), allowed only for the
  module named `brcmf` (-1 otherwise), -19 off the Pi 5. kmod-abi `sys_plat` decl + `pi5_pins` consts; kmod-kapi wrappers
  `plat::pi5_sdio2_mux/pi5_wl_on/pi5_wl_on_ready`. `kapi_secret_peek` now also allows the module named `brcmf`.
- NEXPORTS 139 -> 140 (recounted from the table: 140 sym! lines).
- ktest kmodkapi: export present, pi5_pins refused for other owners, pm hook table (replace, phase filter, unregister).

Evidence: builds ok aarch64 and --rpi5; ktest=kmodkapi aarch64 PASS (fails=0).
Pi trial (=y, over eth0 192.168.2.4): wifi0 associates WPA2 + DHCP; `[brcmf] ...` quiesce line (`brcmf: disassoc+down ... WL_REG_ON low`)
on reboot/kexec; `[brcmf]` chip/fw/ver lines at boot as before.
Not done (B2): the module crate, sched::wedge / boot::device_tree uses inside brcmf, init.rs cfg(drv-brcmf) split.
