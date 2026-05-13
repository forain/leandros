//! PS/2 keyboard driver for x86-64.

use evdev_server;

static mut EXTENDED: bool = false;

/// Called from the keyboard IRQ handler (vector 33).
pub fn on_irq() {
    unsafe {
        let scancode = inb(0x60);
        handle_scancode(scancode);
    }
}

#[cfg(target_arch = "x86_64")]
unsafe fn inb(port: u16) -> u8 {
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
unsafe fn inb(_port: u16) -> u8 { 0 }

fn handle_scancode(scancode: u8) {
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
    }
}
