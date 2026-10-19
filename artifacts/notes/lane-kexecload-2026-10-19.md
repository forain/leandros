# lane/kexecload (2026-10-19)

Lane 2 of `artifacts/notes/kexec/PLAN.md`: `kexec_file_load` stages a
`kernel-direct` image. No jump yet (`reboot(KEXEC)` still EINVAL).

## What it is
- `kernel/src/kexec/` : `mod.rs` (syscall, state, `loaded()`), `elf.rs`
  (PT_LOAD by p_paddr), `stage.rs` (safe allocator, indirection list,
  segment stager), `info.rs` (MBI / FDT through the `boot` builders).
- Syscall x86_64 320 / aarch64 294, euid 0 only (EPERM). Flags UNLOAD (1) and
  NO_INITRAMFS (4); anything else EINVAL. Re-load replaces; unload frees.
  `kexec_load` (246/104) stays ENOSYS for root.
- `kexec::loaded() -> Option<Loaded>`: `entry`, `bootinfo` (MBI/FDT phys),
  `head` (first indirection page), `regs` (x86 `[0x36d76289, MBI]` for
  EAX/EBX; aarch64 `[FDT, 0]` for x0), `footprint` (every destination range
  plus the FDT, for cache clean).
- `boot/src/limine.rs` keeps the raw Limine entry types (`raw_regions()`).
  kexec uses them on a Limine boot; on a direct-booted kernel it translates
  `BOOT_INFO.memory_regions()`.

## Facts found
- `kernel-direct` is **ELF64 on both arches** (x86 `OUTPUT_FORMAT(elf64-x86-64)`;
  `kernel-direct-32.elf` is a separate objcopy product). Only ELF64 is
  accepted. aarch64 is an ELF too (plus a raw `.bin`); the ELF is what loads.
- aarch64 segments are not page aligned and neighbours share a page: PT_LOAD 2
  ends at 0x402D0030 and PT_LOAD 3 starts there. The stager carries the open
  page across segments (gap stays zero). The pure-BSS segment is not staged
  (the entry code zeroes BSS) but is part of the footprint.
- aarch64 `e_entry` is `_start` (image + 0x40); the arm64 protocol enters at
  the image start (`b _start`), so `entry` = lowest paddr after checking the
  `ARM\x64` magic at +0x38.
- aarch64 image occupies 0x40080000-0x4197A000; initrd dest 0x48000000
  (x86 0x10000000) matches `INITRD_PHYS` in `main.rs`.

## Load rules
Every destination page (segments incl. BSS, initrd) must lie in Limine
types 0/5/6 (usable, bootloader reclaimable, executable+modules) else
EADDRNOTAVAIL. No source, indirection or FDT page may lie in a destination
range (x86: nor below 1 MiB): rejected buddy pages are held until the load
finishes, then freed. x86 MBI goes below 1 MiB via `pick_mbi_phys`, checked to
be usable; the aarch64 FDT is 16 KiB from the buddy (<8 GiB) and removed from
its own `/memory`. x86 refuses images above 2 GiB (direct HHDM).
x86 framebuffer tag only when the FB lies above all usable RAM.

## Test
`userland/kexectest/kexectest.c` (zig cc, in `build-userland.sh` and the f2fs
`bins`), the image carries `/boot/kexec` (= `target/final-$arch/kernel-direct`,
9 MB, `mkfs-f2fs-populated.py`). Cases: load, re-load, load+initrd, unload x2,
bad flags, bad fd, unterminated cmdline, non-ELF (ENOEXEC), non-root (EPERM).
aarch64 (HVF, `--no-gpu`-equivalent driver boot): ALL PASS; log lines:

    [KEXEC] loaded: 1 segments, entry=0x40080000 bootinfo=0x44868000 head=0x13a510000 pages=1981 (src=1973 ind=4) cmdline=41B
    [KEXEC]   dest seg: 0x40080000-0x4197a000

## Open for the exec lane
- The FDT / MBI content is only host-tested (lane kexecbootinfo); the new
  kernel has never parsed the in-guest one.
- Nothing validates the staged data after load (the old kernel keeps running);
  exec must copy with the list while all other CPUs are stopped.
- x86_64 builds and runs (QEMU/OVMF, TCG): all error-path cases pass, but the
  load is refused with EADDRNOTAVAIL, correctly: OVMF's map has ACPI NVS at
  0x800000-0x900000 and reserved/executable at 0x900000-0x1780000, inside the
  fixed 0x100000-0x1a67000 destination (design B#8). kexectest reports that
  as SKIP on x86. Making x86 load needs a relocatable/low-NVS-tolerant policy
  (or a QEMU config whose firmware leaves that range free); decide in the exec
  lane. The MB2 path of `kernel-direct` is also still untested at runtime.
- Source pages come from anywhere in RAM (aarch64 head=0x13a510000 above 4 GiB):
  the purgatory needs an identity map covering them.
