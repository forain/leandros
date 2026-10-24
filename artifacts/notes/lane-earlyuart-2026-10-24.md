# lane/earlyuart — 2026-10-24 (openbugs L8.12)

**FIXED** — aarch64 early boot no longer faults on the physical PL011 under TCG, and
faults before `arch_aarch64::init` are reported instead of spinning at VBAR 0 + 0x200.

## Change (commit c7bdac3e)
- `arch/aarch64/src/early.rs` (new): `setup(hhdm, kphys, kvirt)` runs first in
  `kernel_main` (Limine path). It calls `exception::init_early()` (VBAR_EL1 = kernel table,
  early flag set), then makes the UART reachable:
  - `AT S1E1W` probe of the physical UART translates (HVF case) -> use it as is;
  - else, if TTBR0 is 4K/48-bit/EPD0=0 and L0[0] is empty, hook a static L1 table with one
    1 GiB Device block (PA 0..1 GiB, MAIR device index found by scan, PXN|UXN, EL1 only) -> use phys;
  - else early UART output is dropped (`EARLY_SILENT`) rather than faulting.
  `teardown()` clears the hook + TLBI when `uart::set_base`/`reinit` switch to the HHDM alias
  (in `arch_aarch64::init`), so no process inherits a lower-half device map.
- `uart.rs`: `rd`/`wr` go through `reg_base()` honouring the early mode; legacy behaviour
  (direct boot / kexec, after arch init) is unchanged.
- `exception.rs`: while the early flag is set (cleared by `exception::init`), the EL1 sync
  handler prints `[EXC] early EL1 fault ESR= ELR= FAR=` and halts (wfe) without touching sched.
- Test-only knob `early.fault=1`: deliberate read of VA 0x7000_0000_0000 right after
  `capture_boot_identity` (all arches; on x86_64 the existing early_idt reports it).
- Why identity instead of HHDM: the HHDM alias of the UART is not mapped until arch init
  (`map_4k` needs the buddy), and pre-installing a block in TTBR1 would collide with that
  `map_4k` walk; the TTBR0 hook is private to the early window.

## Evidence
- TCG `--mode uefi-tcg`, `/bin/uname -a`: boots to shell, 1/1 PASS (was: ESR 0x96000004, FAR 0x9000018, hang).
- `early.fault=1` HVF and TCG both print
  `[EXC] early EL1 fault ESR=0000000096000004 ELR=FFFFFFFFC01E7698 FAR=0000700000000000`.
- HVF full regress: 45 suites, PASS (eu-reg2).
- TCG full regress (eu-treg2): 44/45 suites; only red = polltest poll_timeout_wake_latency (known-red latency case, TCG timing; unrelated to boot).
- x86_64 release build OK.

## Notes / open
- Two harness boots failed with an empty serial tail ("prompt not seen in 120s") when run
  concurrently with an x86_64 build or a second QEMU; both passed when re-run alone. The 120 s
  prompt wait does not appear to be scaled for TCG (scale 3.0) — likely harness timing, not this change.
