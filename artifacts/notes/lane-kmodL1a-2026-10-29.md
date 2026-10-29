# lane/kmodL1a - Y/N gating for network drivers (2026-10-29)

Symbols (drivers/Kconfig): E1000E, R8169, MACB, VIRTIO_NET, IWLWIFI, WIFI_SIM, BRCMF, hidden WIFI_STACK.
Kernel features drv-e1000e, drv-r8169, drv-macb, drv-virtio-net, drv-iwlwifi, drv-wifi-sim, drv-brcmf, drv-wifi-stack
-> drivers features of the same name (no `default`; only the kernel crate enables them, so a bare cargo build is the all-N config).
Dependencies in drivers/Cargo.toml: iwlwifi, wifi-sim, brcmf imply wifi-stack; wifi-sim implies virtio-net
(Kconfig: WIFI_SIM now `depends on VIRTIO_NET`).

What is gated in drivers/src/lib.rs
- e1000e(+_hw), r8169(+_hw), macb(+_hw), iwlwifi, iwl_trans/fw/assoc/rs, wifi_sim, brcmf, brcmf_boot: compiled out.
- wifi-stack: wifi_mgr, wifi_frame, wifi_ht, wifi_crypto, wifi_wrongkey, wifi_pref, iwl_cmd, iwl_agg.
  wifi_api (types seam used by servers/net) and cmdline_secrets stay always on.
- Modules with outside callers are replaced by drivers/src/stubs/<name>.rs: e1000e, r8169 (shutdown, pm_*),
  iwlwifi (kexec_stop, scan_lines, still falls back to wifi_sim/brcmf), wifi_mgr (AUTO_ATTEMPT_BIT), virtio_net
  (reports no device; keeps find_msix/find_isr, which virtio-input uses, copied verbatim).
- Remaining call sites use cfg: drivers/src/netdev.rs (init, register, ops tables, info lines), rp1.rs attach_gem,
  kernel init.rs / power.rs (brcmf).
- NOT gated: bt (belongs with the USB/misc group in L1c), bcm2712_*, wifi_api.

Kconfig dependencies: E1000E/R8169/IWLWIFI depend on PCI only and MACB/BRCMF on nothing (were ARCH_X86_64 / BOARD_RPI5).
Reason: before this lane those drivers were compiled into every kernel (aarch64 QEMU can use a PCI e1000e; the
Pi-only drivers are inert elsewhere), so the byte-identical gate requires them ON in every default config.

## Gate results (kmodL1a2)

Gate 1 revised: byte-identity not required (declaring features changes the crate metadata hash and CGU layout). Accepted: hash-normalised symbol set equal apart from generic instances (drop_in_place, grow_one, fmt) moving between crates; section sizes equal; no string diffs beyond string-table packing; aarch64 runtests pass.

Gate 2: x86_64 all-N build OK, boots in QEMU, /bin/pstest 108 checks, 0 failed, no panic in dmesg.
