# lane/lapreboot 2026-10-28: laptop reboot -> power-off (FIXED in design terms; root cause not reproduced)

Design: artifacts/notes/design-lapreboot-2026-10-28.md. Cause 1 (default cold 0xCF9=0x0E power cycle turned into S5) is the
leading hypothesis; the cf9full reproduction was deliberately NOT run (a power-off needs the user's button).

## Changes
- arch/x86_64/src/power.rs: chain is now Linux's (ACPI RESET_REG, 8042, ACPI, 8042, CF9 0x06) with CF9 0x0E last before the triple
  fault; cf9full waits 5 s; `quiet_acpi_events` clears PM1_EN/GPE0_EN and W1C's PM1_STS/GPE0_STS (SCI_EN untouched) before any reset.
- boot/src/acpi.rs: AcpiPower gains pm1a_evt, pm1b_evt, gpe0, gpe0_len.
- kernel/src/power.rs: default is warm; `reboot.method=auto|acpi|kbd|cf9|cf9full|triple`; `reboot.type=cold` = alias for cf9full;
  `[REBOOT] chain: ...` is logged before the final klog flush; `[PMC] reset cause` boot line (power::log_reset_cause).
- aarch64 / PSCI path untouched.

## [PMC] reset cause caveats
Offsets/bits are from memory (PCH 100/200 datasheet, coreboot skylake pmc.h), UNVERIFIED, raw values are printed next to the decode.
PWRMBASE is read from PMC 00:1f.2 BAR0 (0x10), not 0x48 as the design suggested. On the laptop ETR3 read 0xffffffff: that mapping is
NOT valid (CF9GR=1 in the decode is an artifact), ignore it. GEN_PMCON_A=0xd8a036f8 B=0x00073a49 PM1_STS=0 after a warm reboot.
GBLRST_CAUSE0/1 are not read (offsets unverified, risk of a bad MMIO read).

## Evidence
- x86_64 QEMU: reboot works with default and with reboot.method=acpi|kbd|cf9|cf9full (2 "Kernel starting" each, correct chain line).
- Box regress aarch64 (59f15441): PASS, 0 boot failures.
- Laptop (INTEL=1 trial 31d8c226937f): 15/15 reboot cycles returned (25-35 s) with the trial re-armed each cycle, each boot printed
  `[PMC] reset cause` and the previous boot's `[REBOOT] chain: acpi kbd acpi kbd cf9(0x06) cf9full(0x0E) triple`. 0 power-offs.
  Intermittent bug, so 15 clean cycles is supporting evidence only. Laptop left on `good`, not promoted.
- Open: confirm over more cycles / on battery; verify PMC bit decode before relying on it.
