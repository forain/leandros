# lane/pimem — openbugs L8.8 (software part), 2026-10-26

Status: PARTIAL-by-design — all three items FIXED and guarded in QEMU aarch64; the
non-coherent behaviour itself can only be proven on a Pi 5.

## 1. no-map ranges left out of the HHDM — FIXED
- `boot::hhdm_a64::unmap` zeroes leaves wholly inside the range and splits partial
  blocks (1G->2M->4K, attributes kept) through a caller break-before-make hook.
- `kernel/src/main.rs early_nomap_a64` (after mm init; split tables from buddy) runs it
  for every DTB `/reserved-memory` `no-map` child (rounded out to pages) plus the test
  knob `mm.nomaptest=S-E`; `a64_bbm` refuses a split when the block holds this code,
  the stack or the parent table (reported as `left`), masks DAIF across the window.
  Verification by `AT S1E1R` per range: `-> unmapped` / `STILL MAPPED`.
- Guard: host tests (3, boot/memmap-tests) + QEMU boot line. A/B on HVF aarch64,
  `mm.nomaptest=0x50003000-0x50400000`:
  `mm.nomap=0` -> `STILL MAPPED (1022 probes)`; default -> `cleared 510 split 2 left 0 -> unmapped`.
- Open (Pi): on a direct boot the kernel lives in the 1 GiB block of RAM GiB 0, so a
  no-map range in GiB 0 gets `left != 0` (split refused, still cacheable) unless the
  early tables map that GiB at 2 MiB. Check the `[early] nomap:` line on the Pi.

## 2. DMA sync does cache maintenance on non-coherent platforms — FIXED (unproven on HW)
- `mm::dma_cache`: `dc cvac` clean (to-device), invalidate with `dc civac` on partial
  edge lines and `dc ivac` inside (from-device), line = `CTR_EL0.DminLine`, `dsb sy`.
  `DmaBuf::sync_for_device/cpu` (xHCI) call it over the whole buffer.
- Mode: non-coherent iff the DTB is BCM2711/BCM2712 and has no `dma-coherent`;
  `dma.coherent=0|1` overrides. QEMU virt's UEFI-provided DTB has no `dma-coherent`
  at all, so that property alone could not be the switch. x86/QEMU keep the barrier.
- Guards: host `plan_edges` (every line covered once, partial = civac) + boot selftest
  `[early] dma: coherent, ..., line 64, selftest 48/48 ok` (line counts, dirty bytes
  beside an unaligned invalidate survive).
- Open (Pi): only xHCI's DmaBuf uses sync points; other DMA drivers that would run on
  the Pi (SDHCI, GENET/RP1 eth) have none yet.

## 3. Direct-boot park elects by full affinity — FIXED
- entry_aarch64.s gate: Aff3:Aff2:Aff1:Aff0 == 0 (was Aff0 only, which let every
  Cortex-A76 core through: MT=1, core in Aff1). `boot/src/mpidr.rs` holds the predicate;
  host tests check multi-cluster MPIDRs and that the asm text matches. With the old
  Aff0 mask the test fails.

Regress aarch64 (with the nomaptest knob): PASS. x86_64 builds.
