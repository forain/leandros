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

/// Reset the machine. Never returns.
///
/// # Safety
/// Ring 0; the machine restarts. Interrupts should already be disabled.
pub unsafe fn restart(p: Option<&AcpiPower>) -> ! {
    // 1. ACPI RESET_REG (Linux's default reboot= method when ACPI is up).
    if let Some(p) = p {
        if p.reset_port != 0 {
            outb(p.reset_port, p.reset_value);
            spin(10_000_000);
        }
    }
    // 2. Keyboard controller pulse (reboot=k): wait for the input buffer to
    //    drain, then command 0xFE pulses the CPU reset line.
    for _ in 0..10 {
        for _ in 0..0x10000 {
            if inb(0x64) & 0x02 == 0 { break; }
            spin(10);
        }
        outb(0x64, 0xFE);
        spin(1_000_000);
    }
    // 3. PCI reset control register (reboot=p): hard reset.
    outb(0xCF9, 0x02);
    spin(1000);
    outb(0xCF9, 0x06);
    spin(10_000_000);
    // 4. Triple fault (reboot=t): an empty IDT and an exception.
    #[repr(C, packed)]
    struct Idtr { limit: u16, base: u64 }
    let idtr = Idtr { limit: 0, base: 0 };
    core::arch::asm!("lidt [{}]", "int3", in(reg) &idtr, options(nostack));
    loop { core::arch::asm!("cli; hlt", options(nomem, nostack)); }
}

/// Stop this CPU for good (`machine_halt`).
pub fn halt_forever() -> ! {
    loop { unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)); } }
}
