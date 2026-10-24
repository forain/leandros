# lane/hhdmfill — 2026-10-24 — FIXED (openbugs L8.3: aarch64 HHDM fill proven, now the default)

## Root cause of the "silent unmap4g wedge" (it was never the fill)
- On HVF the early UART at phys 0x0900_0000 "translated" (`AT S1E1W` in `arch_aarch64::early::setup`)
  only through a **stale TLB entry** left over from the firmware's identity map; Limine's TTBR0
  tables do not map it. The fill's (and the test knob's) `tlbi vmalle1is` dropped that entry, so the
  next early print took a translation fault (ESR 0x96000004, FAR 0x0900_0018 = PL011 FR,
  ELR in `uart::putc_within`), and the early fault report faulted again on the same UART: endless
  recursion, SP walking down through .bss/.text, no output. TCG has no such stale entry, which is
  why TCG (after earlyuart) boots the knob fine.
- Found by recording the first fault + a stage marker in statics and reading them with the QEMU
  monitor (`x/5gx`) while the guest was wedged: stage = after the fill's TLBI, first fault as above.
- Fix (`arch/aarch64/src/early.rs`): `tlbi vmalle1` before the probe, so the probe sees the real
  tables and falls back to the private TTBR0 Device hook (as on TCG).
- Second bug found on the way: the fill's page-table pool was the *bottom* of the first usable
  region = phys 0x4000_0000 on QEMU virt, where the DTB lives (Limine reports it usable); the `2m`
  fill zeroed it (`dtb ram scan: none`). Pool now comes from the top of the highest mapped usable region.
- Also removed the late `exception::init()` in the test path (it cleared the early-fault flag).

## Evidence (6 GiB, `earlyfb.test=unmap4g1` clears the L1 HHDM entry for GiB 4)
- TCG `mm.hhdmfill=1`: `wrote 1 x 1G ... patched 1`, memtest PASS. TCG `2m`: `512 x 2M, 1 new table page`, memtest PASS.
- HVF `mm.hhdmfill=1`: `wrote 1 x 1G, patched 1`, memtest 1/1 PASS (35 s); HVF `2m`: `512 x 2M, 1 table`, DTB found, memtest PASS.
- HVF default boot: `mapped 1 ranges, patched 0`, memtest PASS (unchanged).
- After making fill the default: see "Final" below.
- Host walker tests 11/11.

## Change of default
- Fill now runs by default on aarch64 (as on x86_64); `mm.hhdmfill=0` = report-only, `2m` = no 1 GiB blocks.
