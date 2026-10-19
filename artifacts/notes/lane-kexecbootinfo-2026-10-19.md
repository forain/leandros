# lane/kexecbootinfo (2026-10-19)

Lane 1 of `artifacts/notes/kexec/PLAN.md`: pure builders that describe the
machine to the next kernel. Nothing is wired to a syscall yet.

## `boot/src/mb2_build.rs` (x86_64 multiboot2 MBI)
- `build_mbi(&Mb2Input, &mut Mb2Scratch, &mut [u8]) -> Result<len, Mb2Error>`.
  Tags 1 cmdline, 2 loader name, 3 module "initrd", 6 memory map, 8
  framebuffer (RGB, XRGB layout), 14/15 RSDP copy (by revision byte), 12 EFI64,
  0 end. No self-pointers: fill anywhere, copy to the final page.
  `Mb2Scratch` (~40 KiB) is caller-provided; keep it off small stacks.
- Raw Limine types: 0/5/6 Available, 2 ACPI reclaim, 3 NVS, 4 bad, 1/7/other
  Reserved. Available RAM straddling 2 GiB is split; the part at/above becomes
  Reserved. `[0,1 MiB)`, the initrd (page rounded) and caller `reserve` ranges
  are forced Reserved (most restrictive wins, via `memmap::sanitize`). More than
  128 entries: smallest Available ranges are dropped (absent = not RAM).
- `pick_mbi_phys(size, extra_avoid)`: first 4 KiB-aligned slot below 0x9F000,
  searching from 0x28000, clear of `MB2_LOW_USED`:
  0-0xFFF (IVT/BDA), 0x7000-0x7FFF (AP trampoline, GDT/params at 0x7E00-0x7F1F,
  `smp.rs`), 0x10000-0x10FFF (`_start_pvh` GDT), 0x20000-0x27FFF (early page
  tables; `rep stosd` clears 32 KiB), 0x8F000-0x90FFF (early stack, ESP=0x90000),
  0x9F000-0xFFFFF (EBDA/video/ROM).

## `boot/src/fdt_build.rs` (aarch64 DTB)
- `build_fdt(&FdtInput, &mut [u8]) -> Result<len, FdtError>`; v17, empty
  rsvmap, strings table deduplicated (512-byte cap), size padded to 8. Buffer
  and final DTB address must be 8-byte aligned.
- Emits exactly what `device_tree::parse` reads: root cells 2/2, `/chosen`
  (bootargs, linux,initrd-start/end u64, rng-seed, stdout-path),
  `memory@<first>` with one `reg` (<= 60), `pl011@`, `pcie@` (ecam reg),
  optional `framebuffer@`, `/cpus/cpu@<mpidr>` (cells 1, or 2 if any MPIDR >
  32 bits), `psci`. No `/reserved-memory`, no memreserve.
- `select_memory(&[MemoryRegion], &mut [(u64,u64);60])`: Available only,
  clipped to 0-16 GiB, smallest dropped beyond 60.
- `device_tree::for_each_cpu_mpidr_at(dtb, f)` split out of
  `for_each_cpu_mpidr` so host tests can walk `/cpus` (the original only
  accepts a higher-half address).

## Tests
`cd boot/bootparse-tests && cargo test --target aarch64-apple-darwin`: 11/11
(7 new: MBI round trip incl. 2 GiB/1 MiB clamping, ACPI v1/errors, 128 cap,
placement; FDT round trip with cmdline/initrd/memory/uart/ECAM/fb/cpus/seed,
wide MPIDR, 60-region cap). The parsers use process-wide statics, so all host
tests now take a shared mutex.

## Open
- Wiring (`kernel/src/kexec/`): needs the raw Limine types kept per entry
  (`limine.rs` currently collapses them), the new kernel's image/DTB ranges in
  `reserve`/outside `memory`, and the RSDP bytes read via HHDM.
- The MBI must be written after the old kernel stopped using low memory it
  shares with `MB2_LOW_USED`; the picker only avoids the *new* kernel's users.
