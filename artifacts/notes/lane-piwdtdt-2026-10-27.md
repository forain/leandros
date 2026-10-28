# lane piwdtdt — 2026-10-27: Pi 5 watchdog off after kexec

Status: FIXED (code + host tests + aarch64 regress; not yet run on the real Pi).

## Root cause (candidate a)
`scripts/remote.py pi-deploy` kexecs the new kernel. On aarch64 kexec hands over a
fresh FDT built by `kernel/src/kexec/info.rs` + `boot/src/fdt_build.rs`: root
`compatible = "linux,dummy-virt"`, model "LeandrOS kexec", only memory/cpus/psci/uart/
pci-ecam/chosen. It has no PM node, so `wdt::dtb_has_pm` on `recorded_dtb()` was false.
DTB_VIRT itself was fine (b refuted: the DTB is recorded, it is just ours). The firmware
DTB names the node `brcm,bcm2712-pm` (c refuted; cold boot from the card would arm).
Other rpi5 drivers work after kexec because they use fixed addresses / hardware ids
(`bcm2712_id::stepping()` reads SUN_TOP_CTRL), not the DT.

## Fix
`rpi_misc::wdt_init`: DT node first; otherwise, if `bcm2712_id::stepping().source == Hw`
(chip id decodes as 0x2712), map the fixed PM block 0x10_7d200000 and require
`wdt::pm_regs_plausible(RSTC, RSTS, WDOG)` (password byte reads 0, WDOG only its 20-bit
counter; all-ones/junk fails) before arming. New lines:
`[WDT] no PM node in the DTB (kexec FDT): BCM2712 chip id, PM regs sane, using 0x107d200000`
or `[WDT] BCM2712 chip id but PM regs implausible ...: watchdog off`.
`wdt.off=1` unchanged. Code only built into rpi5 (`rpi_misc::boot` is cfg rpi5), so QEMU
virt/raspi4b are untouched.

## Evidence
- host: `drivers/rpi-misc-tests` 15/15 (dtb/magic test extended with pm_regs_plausible + Buf::x).
- check-rpi5.sh clean for rpi5 and raspi4b; `build-all.sh --arch aarch64 --rpi5` builds.
- aarch64 regress (QEMU virt, HVF) overall PASS, boots failed 0/1, no `[WDT]` line in any log.

## Open
- Confirm on the Pi after pi-deploy: expect the "kexec FDT" line followed by the armed line.
- Alternative not done: carry the PM node in the kexec FDT (would also need reg/ranges).
