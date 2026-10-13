//! PS/2 keyboard driver for x86-64 (i8042 controller, scancode set 1 via
//! the controller's translation).
//!
//! # Presence
//!
//! Nothing is read from the controller until [`init`] has decided there is
//! one, the way Linux's i8042 driver decides:
//!
//! 1. ACPI FADT `IAPC_BOOT_ARCH` bit 1 ("8042") clear on a revision >= 2
//!    FADT: no controller; the ports are not touched at all.
//! 2. Status port reads `0xFF`: nothing decodes the ports.
//! 3. Flush: at most 16 stale bytes; a controller whose output buffer never
//!    empties is broken or absent.
//! 4. Read the command byte (0x20) and run the controller self-test (0xAA,
//!    expecting 0x55), each with a timeout; then restore the command byte
//!    with the keyboard IRQ and translation on and the AUX IRQ off (there is
//!    no PS/2 mouse driver), and enable the keyboard port.
//!
//! Real PCs without a PS/2 controller often *emulate* one in SMM for USB
//! keyboards ("USB legacy support") until the OS takes the xHCI controller
//! over, after which the emulation is gone and the ports float. [`recheck`]
//! runs after the xHCI BIOS handoff and drops the controller (and masks
//! IRQ 1) if it vanished. The IRQ handler independently ignores everything
//! it cannot trust: a status of `0xFF`, an empty output buffer, bytes flagged with a parity/timeout error, and the controller's
//! protocol bytes (ACK, resend, overrun, echo, self-test codes).
//!
//! This is the only code that turns i8042 bytes into input, so with no
//! controller no PS/2 key event can ever be produced.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use evdev_server;

static mut EXTENDED: bool = false;

/// The controller passed [`init`]'s checks and IRQ 1 bytes are trusted.
static PRESENT: AtomicBool = AtomicBool::new(false);
/// IRQs that found status 0xFF (controller vanished).
static BAD_STATUS: AtomicU32 = AtomicU32::new(0);
/// Bytes still to swallow from a Pause/Break sequence (E1 1D 45 E1 9D C5).
static SKIP: AtomicU8 = AtomicU8::new(0);
/// The PS/2 mouse probe owns the data port (polled, task context): the IRQ
/// handler must not read it. Keyboard bytes arriving meanwhile are dropped.
pub(crate) static AUX_BUSY: AtomicBool = AtomicBool::new(false);

pub(crate) const DATA: u16 = 0x60;
pub(crate) const STATUS: u16 = 0x64;
pub(crate) const ST_OBF: u8 = 0x01;
const ST_IBF: u8 = 0x02;
pub(crate) const ST_AUX: u8 = 0x20;
pub(crate) const ST_TIMEOUT: u8 = 0x40;
pub(crate) const ST_PARITY: u8 = 0x80;

/// Polls of the status port before a controller wait gives up (about
/// 100 ms on hardware, where each port read is ~1 us).
pub(crate) const WAIT_POLLS: u32 = 100_000;

extern "C" {
    fn serial_print(s: *const u8, len: usize);
}

fn log(s: &str) {
    unsafe { serial_print(s.as_ptr(), s.len()) };
}

/// Whether the PS/2 controller is in use.
pub fn present() -> bool { PRESENT.load(Ordering::Acquire) }

#[cfg(target_arch = "x86_64")]
pub(crate) unsafe fn outb(port: u16, val: u8) {
    core::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack));
}

#[cfg(not(target_arch = "x86_64"))]
pub(crate) unsafe fn outb(_port: u16, _val: u8) {}

#[cfg(target_arch = "x86_64")]
pub(crate) unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    core::arch::asm!(
        "in al, dx",
        out("al") val,
        in("dx") port,
        options(nomem, nostack)
    );
    val
}

#[cfg(not(target_arch = "x86_64"))]
pub(crate) unsafe fn inb(_port: u16) -> u8 { 0xFF }

pub(crate) unsafe fn wait_write() -> bool {
    for _ in 0..WAIT_POLLS {
        let st = inb(STATUS);
        if st == 0xFF { return false; }
        if st & ST_IBF == 0 { return true; }
        core::hint::spin_loop();
    }
    false
}

pub(crate) unsafe fn wait_read() -> Option<u8> {
    for _ in 0..WAIT_POLLS {
        let st = inb(STATUS);
        if st == 0xFF { return None; }
        if st & ST_OBF != 0 { return Some(inb(DATA)); }
        core::hint::spin_loop();
    }
    None
}

pub(crate) unsafe fn command(cmd: u8) -> bool {
    if !wait_write() { return false; }
    outb(STATUS, cmd);
    true
}

/// Drain stale output. False if it never empties (a stuck/absent device).
pub(crate) unsafe fn flush() -> bool {
    for _ in 0..16 {
        let st = inb(STATUS);
        if st == 0xFF { return false; }
        if st & ST_OBF == 0 { return true; }
        let _ = inb(DATA);
    }
    inb(STATUS) & ST_OBF == 0
}

/// Probe the controller. `acpi_8042`: the FADT's IAPC_BOOT_ARCH "8042" bit,
/// when the FADT is revision 2 or later (None = no usable FADT).
pub fn init(acpi_8042: Option<bool>) {
    // A clear FADT bit is only a note: vendors (HP) clear it on machines
    // that have an i8042. Like Linux, probe anyway; the status port reading
    // 0xFF means no controller.
    let fadt_absent = acpi_8042 == Some(false);
    if fadt_absent {
        log("[I8042] FADT says no 8042 (ignored, as Linux); ");
    }
    match unsafe { probe() } {
        Ok(()) => {
            PRESENT.store(true, Ordering::Release);
            if fadt_absent { log("probe: present, self-test 0x55, kbd port ok\n"); }
            else { log("[I8042] PS/2 controller present (self-test 0x55, kbd port ok)\n"); }
        }
        Err(why) => {
            if !fadt_absent { log("[I8042] "); }
            log("no PS/2 controller: ");
            log(why);
            log("\n");
            mask_irq();
        }
    }
}

unsafe fn probe() -> Result<(), &'static str> {
    if inb(STATUS) == 0xFF { return Err("status port reads 0xFF"); }
    if !flush() { return Err("output buffer never empties"); }
    if !command(0x20) { return Err("timeout writing a command"); }
    let ctr = wait_read().ok_or("no reply to READ COMMAND BYTE")?;
    if !command(0xAA) { return Err("timeout writing the self-test command"); }
    match wait_read() {
        Some(0x55) => {}
        Some(_) => return Err("self-test failed"),
        None => return Err("no reply to the self-test"),
    }
    // Keyboard IRQ + translation on, keyboard clock enabled; AUX IRQ off
    // and AUX clock OFF until `psmouse::init` has set the pointing device
    // up: with the clock on and nobody reading, a touchpad's bytes sit in the
    // shared output buffer and block every keystroke behind them.
    let new_ctr = (ctr | 0x01 | 0x40 | 0x20) & !(0x02 | 0x10);
    if !command(0x60) || !wait_write() { return Err("timeout writing the command byte"); }
    outb(DATA, new_ctr);
    if !command(0xA7) { return Err("timeout disabling the AUX port"); }
    if !command(0xAE) { return Err("timeout enabling the keyboard port"); }
    let _ = flush();
    Ok(())
}

/// Re-validate after something that can make an emulated controller go away
/// (the xHCI BIOS handoff). Logs and disables the driver if it is gone.
pub fn recheck(when: &str) {
    if !present() {
        // Not found earlier (floating ports, or a failed probe): the real
        // controller may only be reachable now that USB legacy emulation is
        // off. Probing is bounded and safe on absent hardware.
        if unsafe { probe() }.is_ok() {
            PRESENT.store(true, Ordering::Release);
            unsafe { super::ioapic::set_irq(1, super::cpus::bsp_apic_id().min(0xFF) as u8, 33) };
            log("[I8042] PS/2 controller found ");
            log(when);
            log(" (self-test 0x55, kbd port ok)\n");
        }
        return;
    }
    let st = unsafe { inb(STATUS) };
    if st == 0xFF {
        PRESENT.store(false, Ordering::Release);
        mask_irq();
        log("[I8042] PS/2 controller gone ");
        log(when);
        log(" (it was the firmware's USB legacy emulation); PS/2 input disabled\n");
    }
}

fn mask_irq() {
    unsafe { super::ioapic::mask_irq(1, 33) };
}

/// Called from the i8042 IRQ handler (vector 33, which serves both IRQ 1
/// and, once the PS/2 mouse is up, IRQ 12): drains the output buffer and
/// routes each byte by the status register's AUX bit.
pub fn on_irq() {
    if !present() || AUX_BUSY.load(Ordering::Acquire) { return; }
    unsafe {
        for _ in 0..16 {
            let st = inb(STATUS);
            if st == 0xFF {
                // The ports float: whatever raised the IRQ, no byte here is real.
                if BAD_STATUS.fetch_add(1, Ordering::Relaxed) >= 8 {
                    PRESENT.store(false, Ordering::Release);
                    mask_irq();
                }
                return;
            }
            if st & ST_OBF == 0 { return; }
            let byte = inb(DATA);
            let err = st & (ST_TIMEOUT | ST_PARITY) != 0;
            if st & ST_AUX != 0 {
                // Mouse/touchpad bytes are not keystrokes.
                drivers::psmouse::on_byte(byte, err);
                continue;
            }
            // Error-flagged bytes are garbage.
            if err { continue; }
            handle_scancode(byte);
        }
    }
}

fn handle_scancode(scancode: u8) {
    // Controller/keyboard protocol bytes, never key codes in set 1: overrun
    // (00/FF), self-test failure (FC/FD), echo (EE), ACK (FA), resend (FE).
    // (AA is also LShift-up and is kept.)
    if matches!(scancode, 0x00 | 0xFF | 0xEE | 0xFA | 0xFC | 0xFD | 0xFE) { return; }
    let skip = SKIP.load(Ordering::Relaxed);
    if skip > 0 { SKIP.store(skip - 1, Ordering::Relaxed); return; }
    if scancode == 0xE1 {
        // Pause/Break: E1 1D 45 E1 9D C5, no release; not mapped.
        SKIP.store(5, Ordering::Relaxed);
        return;
    }
    if scancode == 0xE0 {
        unsafe { EXTENDED = true; }
        return;
    }

    let is_up = (scancode & 0x80) != 0;
    let code = scancode & 0x7F;
    let is_ext = unsafe { EXTENDED };
    unsafe { EXTENDED = false; }

    let ev_code = if is_ext {
        match code {
            0x1C => 96,  // KEY_KPENTER
            0x1D => 97,  // KEY_RIGHTCTRL
            0x35 => 98,  // KEY_KPSLASH
            0x38 => 100, // KEY_RIGHTALT
            0x47 => 102, // KEY_HOME
            0x48 => 103, // KEY_UP
            0x49 => 104, // KEY_PAGEUP
            0x4B => 105, // KEY_LEFT
            0x4D => 106, // KEY_RIGHT
            0x4F => 107, // KEY_END
            0x50 => 108, // KEY_DOWN
            0x51 => 109, // KEY_PAGEDOWN
            0x52 => 110, // KEY_INSERT
            0x53 => 111, // KEY_DELETE
            _ => 0,
        }
    } else {
        match code {
            0x01 => 1,   // KEY_ESC
            0x02 => 2,   // KEY_1
            0x03 => 3,   // KEY_2
            0x04 => 4,   // KEY_3
            0x05 => 5,   // KEY_4
            0x06 => 6,   // KEY_5
            0x07 => 7,   // KEY_6
            0x08 => 8,   // KEY_7
            0x09 => 9,   // KEY_8
            0x0A => 10,  // KEY_9
            0x0B => 11,  // KEY_0
            0x0C => 12,  // KEY_MINUS
            0x0D => 13,  // KEY_EQUAL
            0x0E => 14,  // KEY_BACKSPACE
            0x0F => 15,  // KEY_TAB
            0x10 => 16,  // KEY_Q
            0x11 => 17,  // KEY_W
            0x12 => 18,  // KEY_E
            0x13 => 19,  // KEY_R
            0x14 => 20,  // KEY_T
            0x15 => 21,  // KEY_Y
            0x16 => 22,  // KEY_U
            0x17 => 23,  // KEY_I
            0x18 => 24,  // KEY_O
            0x19 => 25,  // KEY_P
            0x1A => 26,  // KEY_LEFTBRACE
            0x1B => 27,  // KEY_RIGHTBRACE
            0x1C => 28,  // KEY_ENTER
            0x1D => 29,  // KEY_LEFTCTRL
            0x1E => 30,  // KEY_A
            0x1F => 31,  // KEY_S
            0x20 => 32,  // KEY_D
            0x21 => 33,  // KEY_F
            0x22 => 34,  // KEY_G
            0x23 => 35,  // KEY_H
            0x24 => 36,  // KEY_J
            0x25 => 37,  // KEY_K
            0x26 => 38,  // KEY_L
            0x27 => 39,  // KEY_SEMICOLON
            0x28 => 40,  // KEY_APOSTROPHE
            0x29 => 41,  // KEY_GRAVE
            0x2A => 42,  // KEY_LEFTSHIFT
            0x2B => 43,  // KEY_BACKSLASH
            0x2C => 44,  // KEY_Z
            0x2D => 45,  // KEY_X
            0x2E => 46,  // KEY_C
            0x2F => 47,  // KEY_V
            0x30 => 48,  // KEY_B
            0x31 => 49,  // KEY_N
            0x32 => 50,  // KEY_M
            0x33 => 51,  // KEY_COMMA
            0x34 => 52,  // KEY_DOT
            0x35 => 53,  // KEY_SLASH
            0x36 => 54,  // KEY_RIGHTSHIFT
            0x37 => 55,  // KEY_KPASTERISK
            0x38 => 56,  // KEY_LEFTALT
            0x39 => 57,  // KEY_SPACE
            0x3A => 58,  // KEY_CAPSLOCK
            0x3B => 59,  // KEY_F1
            0x3C => 60,  // KEY_F2
            0x3D => 61,  // KEY_F3
            0x3E => 62,  // KEY_F4
            0x3F => 63,  // KEY_F5
            0x40 => 64,  // KEY_F6
            0x41 => 65,  // KEY_F7
            0x42 => 66,  // KEY_F8
            0x43 => 67,  // KEY_F9
            0x44 => 68,  // KEY_F10
            0x57 => 87,  // KEY_F11
            0x58 => 88,  // KEY_F12
            _ => 0,
        }
    };

    if ev_code != 0 {
        evdev_server::push_event(0, 1 /* EV_KEY */, ev_code, if is_up { 0 } else { 1 });
        evdev_server::push_event(0, 0 /* EV_SYN */, 0, 0);
        // PS/2 IRQ, not the tick — flush here so coalescing costs this path no
        // latency either. No-op unless the burst mode is compiled in.
        evdev_server::flush_pending_wake();
    }
}
