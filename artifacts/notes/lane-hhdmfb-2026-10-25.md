# lane/hhdmfb — 2026-10-25 — CLOSED as permanent design (openbugs L8.3, x86 Limine HHDM gap > 4 GiB)

Question: why did Limine's HHDM on the HP EliteBook (x86_64, Fast Boot OFF) leave all RAM above 4 GiB
unmapped, and is the kernel's in-place fill (`1d011eb2`, `early_hhdm_fill`) a workaround or the design?

## Answer
- **Not our memmap-type assumption.** Limine 11.4.1 source (`common/protos/limine.c`, `build_pagemap`,
  `get_hhdm_span_top`): for base revision >= 4 (we request 6) every memmap entry except RESERVED and
  BAD_MEMORY is mapped into the HHDM (rev 3 is the only revision with a type allow-list; only revs < 3 also
  map 0-4 GiB unconditionally). The pagemap is built after ExitBootServices from the same `memmap` array the
  memmap response is made from (`acpi_map_tables`/`smbios_map_tables`/`efi_map_runtime_entries` only add
  RESERVED_MAPPED entries, `pmm_sanitise_entries` only trims usable entries against overlaps, and the later
  `pmm_reclaim_uefi_mem` only retypes EFI_RECLAIMABLE, which rev >= 4 maps anyway). Entry
  `0x1_0000_0000 +0x3_3F80_0000 usable` therefore must be mapped; the source has no path that skips it.
- **Not a firmware memmap difference either.** Fast Boot OFF (56/58 Limine entries) and ON (50) have the
  same RAM extents and the same > 4 GiB entry (`0x100000000 +0x33F800000 usable`, today's
  `hw/snap1.txt` vs `hw/fb_on.txt`); they differ only in how the 0xA0000000-0xB5400000 band is split
  (Limine's allocations, kernel/initrd placement, boot-services memory).
- **The gap is not a property of Fast Boot OFF.** Per-boot table over every stick klog in
  `artifacts/remote-logs/*/klog-all.txt` (boot id, Limine entry count, "[early] HHDM fill: bootloader HHDM had gaps"):
  - gap on boots 13-18 only (2026-10-16 09:47 to 2026-10-17 13:20; fbmm-ab9a64af, main-a3f226cc, -175e8640,
    -e93d2049, -75de361a, -3f165a43), always exactly the whole block: `12 x 1G, 508 x 2M, 1 new table`
    (PDPTE[4..16] zero, PML4E present: never written, not corrupted);
  - no gap on every later boot (20-146: 60+ boots, all Fast Boot OFF with 56/58 entries, same Limine 11.4.1,
    same cmdline `iwlwifi.mgr=…`), incl. boots 20-23 whose kernels have a boot path byte-identical to the gap
    kernels (`git diff 3f165a43 cd15e251 -- boot kernel/src/main.rs arch/x86_64` is empty, klog.rs unchanged),
    so "no fill line" there is real evidence, not lost logging. Today's Fast-Boot-OFF boot (`snap1`, 56 entries)
    also has no fill line. The fill prints unconditionally when it runs; a complete HHDM is silent.
- Conclusion: a transient firmware/bootloader state on 10-16/10-17 (boot 19 is missing from the klog; the
  likeliest trigger is a firmware-setup visit / power cycle between 13:20 and 16:51 on 10-17), with a cause
  outside both our code and the Limine source we can read. It cannot be reproduced on demand, so one more
  Fast-Boot-OFF boot would show nothing (today's boot already is that boot).

## Decision: the fill is the permanent design
The Limine contract (rev >= 4: all non-reserved/bad entries in the HHDM) was demonstrably broken once by real
firmware; the kernel checks it (one walk per mapped range, silent when complete) and repairs it in place, then
re-verifies (`early_hhdm_holes`, which excludes anything still unmapped). Same design on aarch64
(`early_hhdm_a64`, lane hhdmfill). No Limine change, revision 6 stays.

## Change (this lane)
- `kernel/src/main.rs` `early_hhdm_fill`: doc comment states the contract, the evidence and why this is the
  design; before writing anything, the fill now logs the bootloader's own first hole and its raw walk
  (`[early] HHDM fill: first bootloader hole A walk: PML4E PDPTE ...`), so a recurrence tells "never mapped"
  (zero entry) from corrupted tables (garbage) in the stick klog without a photo.

## Evidence for the change
- x86_64 TCG, 8 GiB, `earlyfb.test=unmap4g` (clears the HHDM PDPTEs above 4 GiB, the HP's shape):
  `[early] HHDM fill: first bootloader hole 0x0000000100000000 walk: 0x000000007BFA9027 0x0000000000000000`
  (PML4E present, PDPTE zero: the same signature as the HP), then `mapped 6 x 1G ... (1G pages on)`,
  memtest 1/1 PASS (tests-hhdmfb-x86).
- aarch64 HVF release build + memtest 1/1 PASS, no new line (code is x86_64-only).

## Open
- If the fill line ever reappears on real hardware, the new walk line plus `earlyfb.stop=1` (Limine's
  verbose `limine: Top of HHDM`, `paging: 1GiB pages` lines stay on screen) is the next diagnostic.
