# lane/kexechigh (2026-10-19)

x86_64 `kernel-direct` (multiboot2 from kexec, multiboot1/PVH from QEMU
`-kernel`) now uses all RAM in the memory map, not just 0-2 GiB.

## Design
- `_start_pvh` is unchanged: one PDPT (0x21000) shared by PML4[0] identity,
  PML4[256] HHDM and PML4[511] kernel, two PDs of 2 MiB pages over 0-2 GiB.
- New `kernel/src/direct_hhdm.rs`, called from `kernel_main` on a non-Limine
  x86 boot right after `sanitize_boot_info` and before `mm::init_with_map`:
  gives PML4[256] its own PDPT (copying entries 0-1, so 0-2 GiB is
  unchanged; RAM no longer aliases the identity/kernel PDPT) and maps every
  Available / ACPI-reclaimable / ACPI-NVS run above 2 GiB, 1 GiB pages where
  a whole aligned GiB is RAM and CPUID 80000001h:EDX[26] (pdpe1gb) is set,
  2 MiB pages elsewhere. HHDM offset stays 0xffff_8000_0000_0000; PML4 slots
  257+ are used beyond 512 GiB (stops before mm::kva's slot 480).
- Page tables come from a 64-page (256 KiB) static pool in the kernel BSS:
  inside the reserved image, so never on the MBI, initrd, 0x7000 trampoline
  or the 0x20000 boot tables. Pool exhaustion (only with 2 MiB pages and
  > ~60 GiB above 2 GiB) re-sanitizes the map with RAM above the limit
  Reserved instead of handing the buddy unmapped memory.
- Reserved ranges and holes stay unmapped, as with Limine rev 6: LAPIC,
  IOAPIC and the framebuffer still get 4 KiB `map_4k` entries (new PDs under
  the HHDM PDPT), PCI BARs/ECAM go through `kva::ioremap`. No huge page can
  cover them (a 2 MiB page only where RAM is; 1 GiB only when all RAM).
- `direct.gbpages=0` on the command line forces 2 MiB pages.
- `boot/src/mb2_build.rs`: the >= 2 GiB -> Reserved clamp
  (`MB2_DIRECT_LIMIT`) is gone; RAM passes through. The kexec segment
  check (`kexec/mod.rs`, image must end below 2 GiB) stays: the kernel image
  is reached through PML4[511] -> PDPT[510..511] (0-2 GiB).
- `boot/src/multiboot2.rs`: `parse_multiboot1` reads the mmap (flag bit 6)
  and `parse_pvh` the start_info v1 memmap, so plain `--direct` gets the
  real map instead of the fixed 1 MiB-2 GiB fallback; `main.rs` reserves the
  fixed-address `-device loader` initrd (0x10000000) on that path too.
- Host tests (`cd boot/bootparse-tests && cargo test --target
  aarch64-apple-darwin`): 11/11; `mb2_roundtrip` now expects RAM across
  2 GiB and up to 8 GiB Available; `pvh_start_info` carries a memmap.

## Test (x86_64 TCG, q35, 4 CPUs, 6 GiB: RAM 0-2 GiB + 4-8 GiB)
Images: APFS clones of main's images (same commit 7558dd77, disk was full
at the time), new Limine kernel copied into the ESP clone, new kernel-direct
written to the guest's /boot/kexec from a raw extra virtio disk (md5 match).
- gen 0 (Limine): MemTotal 6204908 kB, nproc 4.
- gen 1 (`kexec ... --append=kexec.gen=1`): `[early] direct HHDM: RAM to
  0x200000000, 1G pages 4, 2M pages 0, tables 1 (gbpages)`, `[SMP] 4 CPUs
  online`, login, MemTotal 6257640 kB, nproc 4. 3.2 GB written to tmpfs
  (fills it; low RAM is only 2 GiB, so pages above 4 GiB are in use) fine.
- gen 2 (chained, MBI 0x29000): same HHDM line, 4 CPUs, login, MemTotal
  6257640 kB, cmdline `kexec.gen=1 kexec.gen=2`.
- gen 3 with `direct.gbpages=0`: `1G pages 0, 2M pages 0x800, tables 5`,
  4 CPUs, MemTotal 6257640 kB, 2.9 GB tmpfs write OK.
- plain `--direct` 6 GiB (QEMU multiboot1, 0x2BADB002): HHDM to 8 GiB,
  pcache cap 3058 MiB (half of ~6 GiB; was bounded by the 2 GiB fallback).
  Login still fails (no root identity on direct boot, known) and it runs on
  1 CPU: multiboot1 hands over no RSDP, so `[CPUS] no ACPI MADT`
  (pre-existing, not this lane).
- aarch64 regression (HVF, Limine, 4 GiB): gen 0 4 CPUs, MemTotal
  4119932 kB; new kernel-direct to /boot/kexec (md5 match), `kexec -l` +
  `-e` -> gen 1 login, cmdline `kexec.gen=1`, MemTotal 4162756 kB, 1 CPU
  (the known HVF CPU_ON issue, lane kexecexec).

## Open
- multiboot1 / PVH direct boot: no RSDP from multiboot1 (PVH has one) ->
  1 CPU; an EBDA/BIOS-area RSDP scan would fix it.
- Not tried under KVM / on the laptop.
