# lane/dtblate — RAM-found DTB reservations applied before the allocator (2026-10-23)

Status: FIXED (follow-up to lane/dtbres "Open": a DTB found only by the RAM search was parsed too late for reservations).

## Change
- The Limine-path RAM DTB scan (first 32 MiB of RAM, 4 KiB steps) moved from after
  `arch_aarch64::init` to `early_dtb_ram_scan()` in kernel/src/main.rs, run right before
  `early_dtb_reserve()` and `mm::init_with_map`. `parse()` stashes the blob's
  `/memreserve/` + `/reserved-memory` ranges, so `early_dtb_reserve` now reserves them.
- Pre-mm safety: Limine's HHDM maps only memory-map regions, so each page is probed with
  `AT S1E1R`/PAR_EL1 before it is read (unmapped pages skipped and counted, never faulted on).
  The old "parse needs SIMD" deferral comment was stale (kernel target is `-neon,-fp-armv8`).
- uart/ecam from that DTB are still applied at the old late point (from the saved early
  result), so arch init sees the same BOOT_INFO as before.
- New log line: `[early] dtb ram scan: found at <phys>|none (unmapped pages skipped: N)`.
- `boot::device_tree::scan_for_dtb(start, pages, mapped)` + host unit test
  (unmapped page holding the magic is skipped; blob found; its memreserve is walkable).
  `cargo test -p boot --lib --target aarch64-apple-darwin`: 4/4.
- pcibridgetest.py: when the log says `DTB not found via Limine`, requires
  `[early] dtb ram scan:` < `[early] dtb reserved:` < that line. Negative control on an
  old-code log: FAIL (scan line absent).

## Evidence (aarch64 QEMU, UEFI+ACPI)
- Before: `[MAIN] DTB search failed.` — the late scan found nothing, because by then the
  buddy had reused 0x40000000 (QEMU's DTB lives in usable RAM). ECAM came from ACPI MCFG.
- After: `[early] dtb ram scan: found at 0x40000000 (unmapped pages skipped: 0)`,
  `[early] dtb reserved: none` (QEMU virt has no reserved ranges), `[MAIN] Found DTB in RAM`.
  UART 0x09000000 / ECAM 0x4010000000 identical to before; pcibridge PASS (rng on bus 2).
- Regress aarch64: 44/44 RC=0, overall PASS. x86_64 untouched (aarch64-only cfg).

## Open
- Not hardware-verified (Pi 5 hands over a DTB via Limine, so it takes the dtbres path).
- A DTB left in usable RAM by firmware is trusted as the platform DTB (same as before).
