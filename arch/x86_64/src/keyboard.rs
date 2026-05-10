//! PS/2 keyboard driver for x86-64.

use evdev_server;

static mut SHIFT: bool = false;

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
    match scancode {
        0x2A | 0x36 => unsafe { SHIFT = true; },
        0xAA | 0xB6 => unsafe { SHIFT = false; },
        _ => {
            if scancode < 0x80 {
                let ascii = if unsafe { SHIFT } {
                    SHIFT_MAP[scancode as usize]
                } else {
                    MAP[scancode as usize]
                };
                if ascii != 0 {
                    evdev_server::push_event(0, 1 /* EV_KEY */, ascii as u16, 1); // Down
                    evdev_server::push_event(0, 0 /* EV_SYN */, 0, 0);
                    // For now, we don't push Up events because serial doesn't either,
                    // and doomgeneric_leandros handles timed expiration.
                }
            }
        }
    }
}

const MAP: [u8; 128] = [
    0,  27, b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8',	/* 9 */
  b'9', b'0', b'-', b'=', b'\x08',	/* Backspace */
  b'\t',			/* Tab */
  b'q', b'w', b'e', b'r',	/* 19 */
  b't', b'y', b'u', b'i', b'o', b'p', b'[', b']', b'\n',	/* Enter key */
    0,			/* 29   - Control */
  b'a', b's', b'd', b'f', b'g', b'h', b'j', b'k', b'l', b';',	/* 39 */
 b'\'', b'`',   0,		/* Left shift */
 b'\\', b'z', b'x', b'c', b'v', b'b', b'n',			/* 49 */
  b'm', b',', b'.', b'/',   0,				/* Right shift */
  b'*',
    0,	/* Alt */
  b' ',	/* Space bar */
    0,	/* Caps lock */
    0,	/* 59 - F1 key ... > */
    0,   0,   0,   0,   0,   0,   0,   0,
    0,	/* < ... F10 */
    0,	/* 69 - Num lock*/
    0,	/* Scroll Lock */
    0,	/* Home key */
    0,	/* Up Arrow */
    0,	/* Page Up */
  b'-',
    0,	/* Left Arrow */
    0,
    0,	/* Right Arrow */
  b'+',
    0,	/* 79 - End key*/
    0,	/* Down Arrow */
    0,	/* Page Down */
    0,	/* Insert Key */
    0,	/* Delete Key */
    0,   0,   0,
    0,	/* F11 Key */
    0,	/* F12 Key */
    0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,
    0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,
    0,   0,   0,   0,   0,   0,   0,
];

const SHIFT_MAP: [u8; 128] = [
    0,  27, b'!', b'@', b'#', b'$', b'%', b'^', b'&', b'*',	/* 9 */
  b'(', b')', b'_', b'+', b'\x08',	/* Backspace */
  b'\t',			/* Tab */
  b'Q', b'W', b'E', b'R',	/* 19 */
  b'T', b'Y', b'U', b'I', b'O', b'P', b'{', b'}', b'\n',	/* Enter key */
    0,			/* 29   - Control */
  b'A', b'S', b'D', b'F', b'G', b'H', b'J', b'K', b'L', b':',	/* 39 */
 b'"', b'~',   0,		/* Left shift */
 b'|', b'Z', b'X', b'C', b'V', b'B', b'N',			/* 49 */
  b'M', b'<', b'>', b'?',   0,				/* Right shift */
  b'*',
    0,	/* Alt */
  b' ',	/* Space bar */
    0,	/* Caps lock */
    0,	/* 59 - F1 key ... > */
    0,   0,   0,   0,   0,   0,   0,   0,
    0,	/* < ... F10 */
    0,	/* 69 - Num lock*/
    0,	/* Scroll Lock */
    0,	/* Home key */
    0,	/* Up Arrow */
    0,	/* Page Up */
  b'-',
    0,	/* Left Arrow */
    0,
    0,	/* Right Arrow */
  b'+',
    0,	/* 79 - End key*/
    0,	/* Down Arrow */
    0,	/* Page Down */
    0,	/* Insert Key */
    0,	/* Delete Key */
    0,   0,   0,
    0,	/* F11 Key */
    0,	/* F12 Key */
    0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,
    0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,
    0,   0,   0,   0,   0,   0,   0,
];
