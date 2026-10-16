//! Machine power-off and reset — the arch half of `kernel_power_off` /
//! `kernel_restart` (kernel/src/power.rs), mirroring Linux's x86
//! `native_machine_power_off` (ACPI S5 through `pm_power_off`) and
//! `native_machine_emergency_restart` (ACPI reset register, then the
//! keyboard controller, then the PCI reset port 0xCF9, then a triple fault).
//!
//! On QEMU q35 the FADT gives PM1a_CNT = 0x604 and RESET_REG = I/O 0xCF9,
//! value 0x06; the DSDT's `\_S5_` is `{0, 0}`. Writing SLP_TYP|SLP_EN to
//! PM1a_CNT makes QEMU's ICH9 PM raise a guest shutdown request, so QEMU
//! exits by itself.

use boot::acpi::AcpiPower;

/// PM1_CNT bits.
const SCI_EN: u16 = 1 << 0;
const SLP_TYP_SHIFT: u16 = 10;
const SLP_TYP_MASK: u16 = 7 << SLP_TYP_SHIFT;
const SLP_EN: u16 = 1 << 13;

unsafe fn outb(port: u16, val: u8) {
    core::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack));
}
unsafe fn inb(port: u16) -> u8 {
    let v: u8;
    core::arch::asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack));
    v
}
unsafe fn outw(port: u16, val: u16) {
    core::arch::asm!("out dx, ax", in("dx") port, in("ax") val, options(nomem, nostack));
}
unsafe fn inw(port: u16) -> u16 {
    let v: u16;
    core::arch::asm!("in ax, dx", out("ax") v, in("dx") port, options(nomem, nostack));
    v
}

fn spin(n: u32) {
    for _ in 0..n { core::hint::spin_loop(); }
}

/// Enter ACPI S5 (soft-off). Returns only if the write did not take effect
/// (no PM1a block, or firmware that ignores it).
///
/// # Safety
/// Ring 0; the machine stops. Interrupts should already be disabled.
pub unsafe fn acpi_power_off(p: &AcpiPower) {
    if p.pm1a_cnt == 0 { return; }
    // Firmware that booted in legacy mode leaves SCI_EN clear; hand the
    // chipset to the OS first (Linux: acpi_enable()). Harmless when set.
    if inw(p.pm1a_cnt) & SCI_EN == 0 && p.smi_cmd != 0 && p.acpi_enable != 0 {
        outb(p.smi_cmd, p.acpi_enable);
        for _ in 0..300 {
            if inw(p.pm1a_cnt) & SCI_EN != 0 { break; }
            spin(100_000);
        }
    }
    // ACPI spec 7.4.2 / Linux acpi_hw_legacy_sleep: write SLP_TYP first, then
    // SLP_TYP|SLP_EN, to PM1a and (if present) PM1b.
    let a = (inw(p.pm1a_cnt) & !(SLP_TYP_MASK | SLP_EN)) | ((p.slp_typ_a as u16) << SLP_TYP_SHIFT);
    outw(p.pm1a_cnt, a);
    if p.pm1b_cnt != 0 {
        let b = (inw(p.pm1b_cnt) & !(SLP_TYP_MASK | SLP_EN)) | ((p.slp_typ_b as u16) << SLP_TYP_SHIFT);
        outw(p.pm1b_cnt, b);
        outw(p.pm1b_cnt, b | SLP_EN);
    }
    outw(p.pm1a_cnt, a | SLP_EN);
    // The machine should be gone; give a slow host a moment before the
    // caller falls back.
    spin(50_000_000);
}

/// Which reset to try first (`reboot.method=`); the rest of the chain
/// follows if it does not take.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Method { Auto, Acpi, Cf9, Kbd, Triple }

/// Busy-wait on the TSC clock (valid with interrupts off).
fn udelay(us: u64) {
    let t0 = crate::timer::monotonic_ns();
    while crate::timer::monotonic_ns().wrapping_sub(t0) < us * 1000 { core::hint::spin_loop(); }
}

fn say(s: &str) {
    for &b in s.as_bytes() { unsafe { crate::putc(b) }; }
}

/// FADT RESET_REG (Linux `acpi_reboot`): only an I/O-port register is
/// supported (that is all `boot::acpi` records).
unsafe fn reset_acpi(p: Option<&AcpiPower>) {
    if let Some(p) = p {
        if p.reset_port != 0 {
            say("reboot: ACPI RESET_REG\n");
            outb(p.reset_port, p.reset_value);
            udelay(100_000);
        }
    }
}

/// Keyboard controller pulse (Linux BOOT_KBD): ten tries, each after the
/// input buffer drained.
unsafe fn reset_kbd() {
    say("reboot: 8042 pulse\n");
    for _ in 0..10 {
        for _ in 0..0x10000 {
            if inb(0x64) & 0x02 == 0 { break; }
            udelay(2);
        }
        udelay(50);
        outb(0x64, 0xFE);
        udelay(50);
    }
    udelay(100_000);
}

/// PCI reset control register 0xCF9 (Linux BOOT_CF9_FORCE): request a hard
/// reset, then trigger it; `cold` adds FULL_RST (bit 3), so the PCH power
/// cycles the platform (SLP_S3#/S4#/S5# asserted for 3-5 s): the same as a
/// power-button cold boot for every device on the S0 rails (codec, USB VBUS
/// on most boards, the stick).
unsafe fn reset_cf9(cold: bool) {
    say(if cold { "reboot: 0xCF9 full reset (0x0E)\n" } else { "reboot: 0xCF9 hard reset (0x06)\n" });
    let code: u8 = if cold { 0x0E } else { 0x06 };
    let cf9 = inb(0xCF9) & !code;
    outb(0xCF9, cf9 | 0x02);
    udelay(50);
    outb(0xCF9, cf9 | code);
    // A full reset holds the rails down for seconds before the CPU is gone
    // on some chipsets; give it time before falling back.
    udelay(if cold { 500_000 } else { 100_000 });
}

/// Triple fault (Linux BOOT_TRIPLE): an empty IDT and an exception.
unsafe fn reset_triple() -> ! {
    say("reboot: triple fault\n");
    #[repr(C, packed)]
    struct Idtr { limit: u16, base: u64 }
    let idtr = Idtr { limit: 0, base: 0 };
    core::arch::asm!("lidt [{}]", "int3", in(reg) &idtr, options(nostack));
    loop { core::arch::asm!("cli; hlt", options(nomem, nostack)); }
}

/// Reset the machine. Never returns.
///
/// Chains (the first entry is replaced by `method` when one is given, and
/// that one is not repeated later):
/// - warm (Linux's default on an ACPI machine, `native_machine_emergency_restart`
///   with reboot_type BOOT_ACPI; EFI and BIOS steps skipped, this kernel has
///   neither): ACPI RESET_REG, 8042, ACPI RESET_REG, 8042, 0xCF9 0x06,
///   triple fault;
/// - cold: 0xCF9 0x0E (full reset), ACPI RESET_REG, 8042, triple fault.
///
/// # Safety
/// Ring 0; the machine restarts. Interrupts should already be disabled.
pub unsafe fn restart(p: Option<&AcpiPower>, method: Method, cold: bool) -> ! {
    match method {
        Method::Acpi => reset_acpi(p),
        Method::Cf9 => reset_cf9(cold),
        Method::Kbd => reset_kbd(),
        Method::Triple => reset_triple(),
        Method::Auto => {}
    }
    if cold {
        if method != Method::Cf9 { reset_cf9(true); }
        if method != Method::Acpi { reset_acpi(p); }
        if method != Method::Kbd { reset_kbd(); }
    } else {
        if method != Method::Acpi { reset_acpi(p); }
        if method != Method::Kbd { reset_kbd(); }
        reset_acpi(p);
        reset_kbd();
        if method != Method::Cf9 { reset_cf9(false); }
    }
    reset_triple()
}

/// Stop this CPU for good (`machine_halt`).
pub fn halt_forever() -> ! {
    loop { unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)); } }
}
