# lane/kexecfdt (2026-10-29) -- FIXED (aarch64): kexec FDT holes no longer accumulate

Root cause: design `pikexecsz-2026-10-29` (each kexec FDT block, 16 KiB = FDT + AP park page, is left out of
`/memory`; the next kernel never took it back, so a chain gained one hole per generation and `do_load`'s
`within(loadable)` rejected images reaching past one; same class as `prevlog ... not in usable RAM`).

## Change
- `kexec::return_boot_fdt()` (kexec/mod.rs), called in main.rs right after `arch_aarch64::init` (APs are past the
  park page): when `/chosen` carries `leandros,kexec-ap-mailbox`, copy the recorded DTB to the heap
  (`device_tree::set_recorded_dtb`; drivers read it at probe time), add `dtb & !0x3fff` +16 KiB to the map as
  Available via `sanitize_boot_info`, and free it (`buddy::add_free_range`). Logs
  `[KEXEC] boot FDT block A-B returned to the map (4 pages)`. No-op for firmware DTBs.
- `prevlog::reserve_early` accepts a region inside the union of adjacent Available regions and prints the
  overlapping map entries when it refuses.
- Debug knobs: `kexec.fdt_low=1` (FDT block = first free 16 KiB above the image segments, `Alloc::block_above`),
  `kexec.keep_fdt_hole=1` (skip the return; negative control).
- Not done: optional "treat own FDT as loadable in do_load" (unneeded once the block is returned).

## Evidence (QEMU aarch64, HVF)
`runtests aarch64 <tag> --suite kexecfdt` (`host:kexecfdttest.py`, leaves the VM kexec'd): gen0->gen1->gen2->gen3->gen4,
gen1's FDT X1=0x41000000 (image ends 0x40ecd000). In gen3 `kexec -l` of an image patched to end 0x41004000: rc=0,
then kexec'd into it. `returned to the map` printed 4/4. With `KEXECFDT_NOFIX=1` (keep_fdt_hole in gen1/gen2) the
same step fails: `destination 0x40080000-0x41004000 is not usable RAM`.
kexectest + kexec load/unload green. See PR commit for the aarch64 regress and x86_64 results.
x86_64: kexec gen0->gen1->gen2 chain PASS (kexecfdt suite x86 mode); aarch64 default regress PASS.
