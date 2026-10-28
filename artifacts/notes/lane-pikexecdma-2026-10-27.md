# lane pikexecdma — 2026-10-27: no bus master survives a kexec

**FIXED (code; real-Pi verification pending — this lane does not touch the Pi).**

## Bug
kexec from a `pcie.rpi5=1` kernel left RP1 mastering. The next kernel's buddy
allocator handed out pages the GEM RX ring still pointed at; a live IPv6 mDNS
frame (`33 33 00 00 00 fb ...`) landed on a free-list link:
`[BUDDY] CORRUPT LINK: node=0x10040000 ... next=0xfc98fb0000003333` → panic.
The earlier EL1 null write in epoll `EpCache` (lane piepollnull/piepollsrc) was
on a kernel kexec'd from a PCIe boot: same class. Config-space BME clearing in
`quiesce_all_pci` was already there and did not stop it, so the fix stops each
engine at its own registers and then cuts the bus.

## Old kernel (before the jump), `kernel/src/power.rs::kernel_kexec`
- New generic registry `drivers/src/dma_shutdown.rs`: `register(name, Stage, fn() -> bool)`;
  `Stage::Device` hooks run after xHCI halt + kexec_hooks, then `quiesce_all_pci`,
  then `Stage::Bus` hooks. Runs even with `reboot.quiesce=0`.
- `gem` (macb.rs, sequence in `macb_hw::quiesce`, host-tested): IDR all, THALT +
  wait TSR.TGO, NCR TE/RE off + CLRSTAT, TSR/RSR cleared, TBQP/RBQP + Q1.. + TBQPH/RBQPH = 0.
- `xhci0 xhci1`: the RP1 DWC3s were already halted + HCRST by `usb_hcd::quiesce_all`
  (registered via `register_platform_controller`); now counted in the line.
- `dma40`: every channel this kernel claimed is aborted (firmware channels untouched).
- `pcie-perst` (brcmstb_pcie.rs, Stage::Bus, last): BME off on RP1 and the root
  port, PERST# asserted, bridge reset asserted; `REGS` cleared so later config
  accesses return all-ones instead of stalling.
- brcmf (SDIO Wi-Fi) was already powered off with WL_REG_ON low in phase 1; SD
  host DMA is per-command and idle after the final disk flush.
- Log: `[kexec] quiesced: gem xhci0 xhci1 dma40 pcie-perst` (`FAILED: <names>` appended on failure).

## New kernel (defense in depth), `kernel/src/main.rs` rpi5 direct-boot path
Before `reserve_range`/`init_with_map` (buddy), regardless of `pcie.rpi5`:
`brcmstb_pcie::early_reset_identity()` through the entry stub's identity map (PUD 64, Device):
if SW_INIT shows the bridge released → assert PERST# (MISC_PCIE_CTRL), drain, assert
bridge reset (bit 44). Already in reset → nothing to do (no RC access: it would stall).
Log: `[pcie] early reset: RC held in reset before mm init (...)`. Escape: `pcie.early_reset=0`.
`brcmstb_pcie::init` stage 4 no longer reads RC registers while the bridge is held
in reset (that read stalls the CPU); `pcie.reuse=1` cannot reuse a link after the early reset.

## x86_64 kexec (checked, no change needed)
Already quiesced: e1000e `shutdown`, iwlwifi `kexec_stop`, xHCI halt + HCRST,
i915 `kexec_quiesce` (rings stopped), r8169, NVMe CC.EN off, HDA CRST, NVIDIA
GSP unload, virtio reset, BME + MSI/MSI-X off on every endpoint, then
`kexec::devreset` FLR/bus resets on the last CPU. Future drivers can use dma_shutdown too.

## Tests
- macb-tests host: 17/17 (2 new: queue pointers cleared after RE off; TX-never-idle still clears TE/RE).
- aarch64 full regress on QEMU virt (HVF): PASS. `--rpi5` release build: PASS.
- host:serialxfertest.py (kexec) ended "1 unknown" in this lane (needs its pi-serial setup; run it alone) — the kexec path itself was not exercised in QEMU here.
- Open: real Pi 5 kexec from a `pcie.rpi5=1` kernel with Ethernet traffic (expect both lines,
  no CORRUPT LINK); confirm the early RC read is safe on a cold firmware boot without pcie.rpi5.
