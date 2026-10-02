//! reboot(2) and the kernel's shutdown sequence (Linux kernel/reboot.c).
//!
//! `sys_reboot` checks CAP_SYS_BOOT (here: euid 0, which holds every
//! capability) and the magic numbers exactly as Linux does, then runs one of
//! `kernel_power_off`, `kernel_restart` or `kernel_halt`. Each mirrors Linux's
//! ordering: mark the system state (so nothing new starts), run the shutdown
//! notifiers, shut the devices down, and only then hand the machine to the
//! arch code — ACPI S5 / RESET_REG on x86_64 (arch_x86_64::power), PSCI
//! SYSTEM_OFF / SYSTEM_RESET on aarch64 (arch_aarch64::power).
//!
//! "Shut the devices down" is, for this kernel, the block layer: every f2fs
//! volume gets its block cache flushed, a checkpoint marked as a clean
//! unmount (CP_UMOUNT_FLAG) and a virtio-blk FLUSH, and is made read-only.
//! Linux's reboot(2) does not sync on its own (reboot(8) calls sync() first,
//! and the block drivers flush their write caches in `device_shutdown`); here
//! the write-back cache lives in the in-kernel f2fs server, so its flush is
//! this kernel's `device_shutdown`, and doing it unconditionally means even
//! `reboot -f` leaves a consistent volume. Init's orderly path (userland/init)
//! has already remounted / read-only by then, which makes this a no-op.

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use crate::serial_print_str;

// include/uapi/linux/reboot.h
const LINUX_REBOOT_MAGIC1:  u32 = 0xfee1_dead;
const LINUX_REBOOT_MAGIC2:  u32 = 672_274_793; // 0x28121969
const LINUX_REBOOT_MAGIC2A: u32 = 85_072_278;  // 0x05121996
const LINUX_REBOOT_MAGIC2B: u32 = 369_367_448; // 0x16041998
const LINUX_REBOOT_MAGIC2C: u32 = 537_993_216; // 0x20112000

const LINUX_REBOOT_CMD_RESTART:    u32 = 0x0123_4567;
const LINUX_REBOOT_CMD_HALT:       u32 = 0xCDEF_0123;
const LINUX_REBOOT_CMD_CAD_ON:     u32 = 0x89AB_CDEF;
const LINUX_REBOOT_CMD_CAD_OFF:    u32 = 0x0000_0000;
const LINUX_REBOOT_CMD_POWER_OFF:  u32 = 0x4321_FEDC;
const LINUX_REBOOT_CMD_RESTART2:   u32 = 0xA1B2_C3D4;

const EPERM:  isize = -1;
const EINVAL: isize = -22;

/// Ctrl-Alt-Del policy (`C_A_D`): true = the kernel reboots at once, false =
/// init gets SIGINT. Linux boots with it enabled; init normally turns it off.
/// Nothing generates the key event yet, so this is state only.
static CAD_ENABLED: AtomicBool = AtomicBool::new(true);

/// `system_state` for the shutdown paths: 0 running, else one of below.
const SYSTEM_RUNNING:   u8 = 0;
const SYSTEM_HALT:      u8 = 1;
const SYSTEM_POWER_OFF: u8 = 2;
const SYSTEM_RESTART:   u8 = 3;
static SYSTEM_STATE: AtomicU8 = AtomicU8::new(SYSTEM_RUNNING);

/// True once a halt/power-off/restart has begun.
#[allow(dead_code)]
pub fn shutting_down() -> bool {
    SYSTEM_STATE.load(Ordering::Acquire) != SYSTEM_RUNNING
}

/// Whether Ctrl-Alt-Del reboots directly (reboot(2) CAD_ON/CAD_OFF).
#[allow(dead_code)]
pub fn cad_enabled() -> bool {
    CAD_ENABLED.load(Ordering::Relaxed)
}

#[cfg(target_arch = "x86_64")]
static mut ACPI_POWER: Option<boot::acpi::AcpiPower> = None;

/// Read the FADT/DSDT power registers once at boot, while the tables are
/// known to be intact (x86_64; aarch64 uses PSCI and needs nothing).
#[cfg(target_arch = "x86_64")]
pub fn init_acpi(rsdp_phys: u64, hhdm_offset: u64) {
    let info = unsafe { boot::acpi::find_power_info(rsdp_phys, hhdm_offset) };
    match info {
        Some(p) => {
            serial_print_str("[ACPI] PM1a_CNT=");
            crate::serial_print_hex(p.pm1a_cnt as usize);
            serial_print_str(" PM1b_CNT=");
            crate::serial_print_hex(p.pm1b_cnt as usize);
            serial_print_str(if p.s5_found { " \\_S5_ SLP_TYP=" } else { " no \\_S5_, SLP_TYP=" });
            crate::serial_print_hex(p.slp_typ_a as usize);
            serial_print_str("/");
            crate::serial_print_hex(p.slp_typ_b as usize);
            serial_print_str(" RESET_REG=");
            crate::serial_print_hex(p.reset_port as usize);
            serial_print_str(" value ");
            crate::serial_print_hex(p.reset_value as usize);
            serial_print_str("\n");
            unsafe { ACPI_POWER = Some(p); }
        }
        None => serial_print_str("[ACPI] no FADT: power-off falls back to halt\n"),
    }
}

/// reboot(magic1, magic2, cmd, arg) — Linux `SYSCALL_DEFINE4(reboot, ...)`.
pub fn sys_reboot(magic1: usize, magic2: usize, cmd: usize, _arg: usize) -> isize {
    // CAP_SYS_BOOT first, as Linux does: an unprivileged caller learns
    // nothing about the magic numbers.
    if sched::current_euid() != 0 { return EPERM; }
    // The magics and the command are C `int`s; compare the low 32 bits so a
    // sign-extending caller matches too.
    let (m1, m2, cmd) = (magic1 as u32, magic2 as u32, cmd as u32);
    if m1 != LINUX_REBOOT_MAGIC1
        || (m2 != LINUX_REBOOT_MAGIC2 && m2 != LINUX_REBOOT_MAGIC2A
            && m2 != LINUX_REBOOT_MAGIC2B && m2 != LINUX_REBOOT_MAGIC2C) {
        return EINVAL;
    }
    match cmd {
        LINUX_REBOOT_CMD_RESTART | LINUX_REBOOT_CMD_RESTART2 => kernel_restart(),
        LINUX_REBOOT_CMD_CAD_ON  => { CAD_ENABLED.store(true, Ordering::Relaxed); 0 }
        LINUX_REBOOT_CMD_CAD_OFF => { CAD_ENABLED.store(false, Ordering::Relaxed); 0 }
        LINUX_REBOOT_CMD_HALT    => kernel_halt(),
        LINUX_REBOOT_CMD_POWER_OFF => kernel_power_off(),
        // KEXEC, SW_SUSPEND and anything else: not configured, as on a Linux
        // built without CONFIG_KEXEC_CORE / CONFIG_HIBERNATION.
        _ => EINVAL,
    }
}

/// `kernel_shutdown_prepare` + `device_shutdown`. Exactly one caller gets
/// past the state transition; a concurrent second reboot(2) parks here (the
/// first one is about to take the machine down, as Linux's
/// `system_transition_mutex` would make it wait).
fn shutdown_prepare(state: u8) {
    if SYSTEM_STATE.compare_exchange(SYSTEM_RUNNING, state, Ordering::AcqRel, Ordering::Acquire).is_err() {
        loop { core::hint::spin_loop(); }
    }
    // Shutdown notifiers: none are registered in this kernel.
    // Device shutdown: the block layer (see the module comment).
    crate::syscall::sync_all();
    let n = f2fs_server::shutdown_all();
    serial_print_str("[POWER] filesystems synced; ");
    crate::print_number(n as u32);
    serial_print_str(" f2fs volume(s) committed with a clean-unmount checkpoint (the rest were already read-only)\n");
}

/// Last step of every path: no more interrupts on this CPU, every queued
/// console line written out.
fn machine_prepare() {
    sched::mark_system_down();
    crate::console_drain_outbox();
    crate::console_staging_disable_and_drain();
    unsafe {
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!("cli", options(nomem, nostack));
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!("msr daifset, #0xf", options(nomem, nostack));
    }
}

/// `kernel_power_off`. Falls back to `kernel_halt`'s end state when the
/// platform cannot power off (no FADT / no PSCI), as Linux does without a
/// `pm_power_off` handler.
fn kernel_power_off() -> ! {
    shutdown_prepare(SYSTEM_POWER_OFF);
    serial_print_str("reboot: Power down\n");
    machine_prepare();
    #[cfg(target_arch = "x86_64")]
    unsafe {
        if let Some(p) = (*core::ptr::addr_of!(ACPI_POWER)).as_ref() {
            arch_x86_64::power::acpi_power_off(p);
        }
        serial_print_str("reboot: ACPI power-off failed; System halted\n");
        arch_x86_64::power::halt_forever();
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        arch_aarch64::power::system_off();
        serial_print_str("reboot: PSCI SYSTEM_OFF failed; System halted\n");
        arch_aarch64::power::halt_forever();
    }
}

/// `kernel_restart`.
fn kernel_restart() -> ! {
    shutdown_prepare(SYSTEM_RESTART);
    serial_print_str("reboot: Restarting system\n");
    machine_prepare();
    #[cfg(target_arch = "x86_64")]
    unsafe {
        arch_x86_64::power::restart((*core::ptr::addr_of!(ACPI_POWER)).as_ref());
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        arch_aarch64::power::system_reset();
        serial_print_str("reboot: PSCI SYSTEM_RESET failed; System halted\n");
        arch_aarch64::power::halt_forever();
    }
}

/// `kernel_halt`: everything stopped and synced, the machine left on.
fn kernel_halt() -> ! {
    shutdown_prepare(SYSTEM_HALT);
    serial_print_str("reboot: System halted\n");
    machine_prepare();
    #[cfg(target_arch = "x86_64")]
    arch_x86_64::power::halt_forever();
    #[cfg(target_arch = "aarch64")]
    arch_aarch64::power::halt_forever();
}
