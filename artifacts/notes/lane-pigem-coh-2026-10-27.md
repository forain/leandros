# lane/pigem-coh 2026-10-27: Pi 5 GEM descriptors were in CACHEABLE memory after kexec

Status: FIXED in code (root cause found by reading); needs a Pi boot to confirm.

## Root cause
The MAIR and PTE setup was correct. `WriteCombining` maps to AttrIndx 2, and `mmu::enable_identity` programs
that slot as 0x44 (Normal Inner/Outer NC, inner-shareable). The macb rings just never used that mapping.
- `desc_page` used the NC vmap only `if !mm::dma_cache::coherent()`, and buffer sync went through the same flag.
- `early_dma_cache` sets that flag to "non-coherent" only when the DTB contains `brcm,bcm2712|2711`.
- The Pi now boots through kexec. The kexec FDT (`boot::fdt_build`) is `compatible = "linux,dummy-virt"`, so
  the flag came up **coherent** on the Pi. The pcieboot-84df log shows the same kexec FDT (`[WDT] no PM node in
  the DTB (kexec FDT)`, `[bcm2712] ... dt=Unknown`, `macb ... dtb node missing`).
- As a result the rings sat in the WB linear map with no maintenance, and buffer sync was only `dsb`. The
  init-time contents reached DRAM by eviction, so 2 TX frames and RX slot 0 got through. After that the CPU's
  descriptor writes stayed in its cache (UBR on slot 0). It also kept reading the cached line of RX slots 0-3,
  so RX stuck at head 1 while the GEM had reached slot 115.
- The lane pigem-ring fixes were all inside the `!coherent()` branch, so on the Pi they never ran.
- All other non-coherent drivers on a kexec'd Pi were affected the same way (anything that uses `mm::dma_cache`).

## Fixes (lane/pigem-coh)
1. The kexec FDT carries `/chosen/leandros,dma-noncoherent` when the running kernel is non-coherent.
   `early_dma_cache` honors it and logs `, kexec fdt non-coherent`. Host test: fdt_roundtrip, which fails with the flag off.
2. The macb driver no longer trusts the platform flag. New knob `macb.dma=nc|dev|cmo|coh`, default **nc**:
   - nc: rings vmap'ed Normal-NC.
   - dev: rings mapped Device (kernel Uncached, AttrIndx 1 = 0x00).
   - cmo: rings in the linear map, with per-descriptor `dc ivac` before every read and before and after every
     write. The pre-write invalidate keeps a stale neighbour from being cleaned over a GEM write.
   - coh: the old behaviour.
   In nc, dev and cmo the buffers always get `dc cvac` before TX and `dc ivac` before an RX read.
3. New boot lines:
   `[macb] dma nc (platform ...): ring va .. pte .. attrindx 2 = mair byte 0x44 sh 3 | MAIR_EL1 ..`
   (TTBR1 walk) and `[macb] dma selfcheck: ring->linear .. linear->ring ..: agree|DISAGREE`
   (a round trip through both aliases on spare slot 4 of the dummy descriptor page).

## Evidence
macb-tests 15/15; bootparse-tests 12/12; build-all aarch64 release OK; `--rpi5` release OK;
QEMU aarch64 (HVF) /bin/nettest 1/1 PASS.

## Next Pi boot: what success looks like
`dma nc (platform non-coherent)` once the booting kernel already carries this change (the first kexec from an
older kernel prints `platform coherent`, and macb is still correct), `attrindx 2 = mair byte 0x44`,
`selfcheck ... agree`, then `diag +10s: hw tx N>2 ... stalls 0`, rxring head tracking the hw slot, and a DHCP lease.
If selfcheck says DISAGREE or the stall persists: boot with `macb.dma=dev`, then `macb.dma=cmo`.
