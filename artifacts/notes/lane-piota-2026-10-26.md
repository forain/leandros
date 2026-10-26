# lane/piota 2026-10-26: Pi 5 kernel updates over the network (kexec), no SD swaps

Status: PARTIAL (code + tooling done; nothing hardware-verified; QEMU raspi4b cannot run kexec: no PSCI/EL3 there).

## Audit of the aarch64 kexec path on BCM2712
| item | finding | action |
|---|---|---|
| DTB hand-off | kexec builds a fresh FDT (memory, bootargs, initrd, cpus, psci=smc on rpi5). The firmware DTB's `/reserved-memory` (`atf@0` 0..0x80000 no-map = BL31) was reserved in the buddy only, so the next kernel's `/memory` called BL31 free RAM | FIXED: `early_dtb_reserve` records ranges (`DTB_RESERVED`), `kexec::info::build` carves them out of `/memory`, `do_load` rejects destinations on them |
| initrd | rpi5 kernel probes 0x1000_0000 for a cpio and overrides the DTB value; kexec put the new initrd at 0x4800_0000, so gen2 would run the OLD initrd still at 0x1000_0000 | FIXED (rpi5-gated): kexec INITRD_PHYS = 0x1000_0000 on rpi5 |
| GIC-400 | GICv2 ICPENDR0 does not clear SGIs; distributor (incl. per-CPU banks) survives CPU_OFF | FIXED: GICD_CPENDSGIR clear in `kexec_quiesce`; APs clear their own bank in `kexec_cpu_off` (GICv2 path only) |
| PSCI CPU_OFF/ON | already rpi5-gated smc conduit (park page + AFFINITY_INFO wait) | unchanged; hardware-unverified (TF-A rpi5 pwr_domain_off) |
| uart10 | compile-time base + early asm; state preserved across the jump | nothing needed |
| load address | kexec loads the rpi5 `kernel-direct` ELF (PT_LOAD 0x200000, entry = image start) | nothing needed; remote.py refuses non-0x200000 ELFs |
| SD DMA | sdhci.rs is PIO-only (no SDMA/ADMA); gen2 does SWRST_ALL | nothing needed. Wi-Fi lanes: gen2 brcmf probe must pulse WL_REG_ON low->high (the CYW43455 keeps running across kexec) |

## Tooling
- `scripts/remote.py`: `LEANDROS_REMOTE=pi` (+ `LEANDROS_PI_HOST`) or `pi:<ip>`; `pi-deploy`, `pi-boot <slot>`, `pi-promote`, `pi-status`.
  Boot FAT is not reachable from LeandrOS (only the f2fs partition is a block device), so slots live in `/boot/pi/{trial,good,prev}`;
  kernel.img is never written (power cycle = factory recovery).
- `.claude/skills/pi-remote/SKILL.md`.

## Open
- Expose the FAT partition (raw sdhci access in blkdev_sdhci raw_read/raw_write + espctl-style FAT tool) so `pi-promote` can write kernel.img.
- Optional boot-time auto-kexec into `good` (needs a one-shot guard).
