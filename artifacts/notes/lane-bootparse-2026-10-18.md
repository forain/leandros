# lane/bootparse (2026-10-18)

Direct (non-Limine) boots now deliver a command line and an initrd.

- `BootInfo` gained `cmdline_ptr/cmdline_len` (copy in a 4 KiB static inside the
  `boot` crate, so it outlives the MBI/DTB) and `efi_system_table`.
  `kernel::boot_cmdline()` falls back to it when Limine gave none.
- Parsers (`boot/src/multiboot2.rs`, `device_tree.rs`): MB2 tags 1/3/12/17
  (EFI map only if no tag 6), `parse_pvh` (hvm_start_info), `parse_multiboot1`,
  DTB `/chosen/bootargs`.
- Reservations: a loader-provided initrd (module / DTB) is reserved on both
  arches; with a real MB2 map the kernel image and MBI are reserved too. A
  module initrd wins over the fixed 0x10000000 scan (x86 fallback branch).
- **Entry-stub bugs found on the way (x86_64):** `_start_common` compared EAX
  after the BSS clear zeroed RAX, so MB2 was never detected; and `_start_pvh`
  saved the magic after the serial setup had overwritten AL
  (0x2BADB002 -> 0x2BADB050). Now saved first; the protocol lands in
  `BOOT_PROTO` for `kernel_main`. QEMU `-kernel` hands this ELF **multiboot1**
  (not PVH, not MB2): cmdline is prefixed with the kernel path.
- aarch64: QEMU 11.1 loads **no DTB at all** for a bare ELF `-kernel`
  (`info roms`, RAM scan: nothing), so `-append` cannot reach `kernel-direct`.
  Verified with `-dtb mini.dtb` (hand-made, /chosen/bootargs): `[early] DTB at
  0xFFFF800040000000`, `[early] cmdline: foo=bar zap=1`. `dumpdtb=` fails to
  write on this QEMU build.

## Tested
- `cd boot/bootparse-tests && cargo test --target aarch64-apple-darwin` 4/4
  (MB2 cmdline/module/EFI64, EFI map fallback, tag 6 precedence, PVH).
- x86_64 `--direct -append "foo=bar zap=1" -initrd`: cmdline logged, initrd taken
  from the module (0x1A5E000), init extracted.
- Limine boots reach login on both arches (x86_64 and aarch64, `--no-gpu`).

## Open
- aarch64 `--direct` crashes in virtio-gpu submit (`[EXC] ESR=0`) on this Mac;
  identical on main's prebuilt kernel (pre-existing, not this lane).
- Memory map for MB1/PVH direct boot is still the fixed fallback (HHDM 2 GiB).
- `capture_boot_identity` stays Limine-only; `root=` comes through
  `boot_cmdline()` now, `boot_disk` still absent on direct boot.
