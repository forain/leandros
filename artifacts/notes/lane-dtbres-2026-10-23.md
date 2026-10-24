# lane/dtbres — DTB reserved memory + aarch64 PCI bridge recursion (2026-10-23)

Status: FIXED (openbugs L8.8 reserved-memory, L8.5 PCI bus-0-only). Pi-only parts open (below).

## (1) `/memreserve/` + `/reserved-memory`
- `boot/src/device_tree.rs`: `for_each_reserved_at(dtb, f)` walks the header's
  `/memreserve/` block (u64 pairs to the 0,0 terminator) and every enabled
  `/reserved-memory` child's `reg` (node's own `#address-cells`/`#size-cells`,
  multi-entry `reg`, `no-map` flag, `status != okay` skipped, dynamic `size`-only
  children skipped, grandchildren ignored). `parse()` stashes the ranges (up to 32)
  while the blob is surely mapped; `reserved_ranges()` hands them out.
- `kernel/src/main.rs` `early_dtb_reserve()` (right after `mm.reserve=`, before
  `mm::init_with_map`): `mm::buddy::reserve_range` per range + one line each:
  `[early] dtb reserved: <start>-<end> memreserve|reserved-memory/<name> [no-map]`
  (or `... none`). `MAX_RESERVED` 32 -> 64 so Pi firmware ranges can't overflow it.
- Guard: host unit tests (`cargo test -p boot --lib --target aarch64-apple-darwin`,
  boot crate is now `cfg_attr(not(test), no_std)`) on a crafted FDT: memreserve
  entry, no-map child, 2-entry reg with 2/1 cells, disabled child, dynamic cma
  with nested child, `bus-range`. 3/3 pass. (The parser did not exist before, so
  "fails on old code" = does not compile.)
- QEMU virt via Limine/UEFI hands us no DTB (ACPI only) — boot log shows
  `[early] dtb reserved: none`; the ranges path itself is only exercised on a DTB boot.

## (2) aarch64 PCI bridge recursion
- `drivers/src/pci.rs` `scan()` split into `scan_bus`/`probe_function`/`scan_bridge`.
  x86_64 keeps the flat 0..=255 scan (behaviour unchanged). aarch64 walks from bus 0
  through type-1 headers: firmware-numbered bridges (sec > bus, sub >= sec,
  sec <= max) are followed; an unnumbered bridge (sec == 0, bare `-kernel` boot) gets
  primary/secondary/subordinate assigned and, on that path, a 1 MiB-aligned memory
  window over the BARs assigned behind it (+ mem/bus-master enable; pref/IO windows off).
- BAR sizing now only touches 6 BARs on type-0, 2 on type-1, 0 on type-2 headers
  (previously a bridge's 0x18 bus-number register was treated as BAR2).
- ECAM mapping covers buses 0..max (DTB `bus-range`, else ACPI MCFG end bus, else 0),
  `[MAIN] PCI ECAM buses 0-0xFF` on QEMU (256 MiB device mapping).
- Guard: `.claude/skills/run-leandros/pcibridgetest.py` (suite `pcibridge`): QEMU
  `pcie-root-port -> pci-bridge -> virtio-rng-pci`. New code: bridges -> buses [1, 2],
  virtio-rng on bus 2, PASS (standalone and via `runtests.py --suite pcibridge`). Regress suite aarch64: 25/25 RC=0, overall PASS. x86_64: kernel type-checks (cargo check). Negative control (max bus forced to 0 = old behaviour):
  `virtio-rng on buses []`, FAIL.

## Open / out of scope
- Pi 5 only: non-coherent DMA sync, MPIDR park; `no-map` ranges are reserved but
  still inside the cacheable HHDM linear map (not unmapped) — needs a Pi check.
- Bare-boot bridge windows: 32-bit non-prefetchable only; no I/O window.
- DTB found only by the post-mm RAM search (`DTB not found via Limine`) is parsed
  too late for reservations (only uart/ecam are taken from it).
