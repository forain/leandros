//! PL011 UART driver for AArch64.

// ── Board-specific constants ──────────────────────────────────────────────────

/// MMIO base address of the PL011.
#[cfg(not(feature = "rpi5"))]
pub const BASE: usize = 0x0900_0000;       // QEMU virt

#[cfg(feature = "rpi5")]
pub const BASE: usize = 0x107D_0010_00;    // RPi 5 RP1 UART0

/// Integer baud-rate divisor.
#[cfg(not(feature = "rpi5"))]
const IBRD_VAL: u32 = 13;

#[cfg(feature = "rpi5")]
const IBRD_VAL: u32 = 26;

/// Fractional baud-rate divisor.
#[cfg(not(feature = "rpi5"))]
const FBRD_VAL: u32 = 1;

#[cfg(feature = "rpi5")]
const FBRD_VAL: u32 = 3;

// ── Register offsets ──────────────────────────────────────────────────────────
const DR:   usize = 0x000;
const FR:   usize = 0x018;
const IBRD: usize = 0x024;
const FBRD: usize = 0x028;
const LCRH: usize = 0x02C;
const CR:   usize = 0x030;

// ── Flag register bits ────────────────────────────────────────────────────────
const FR_RXFE: u32 = 1 << 4;
const FR_TXFF: u32 = 1 << 5;

// ── Runtime UART base ─────────────────────────────────────────────────────────
static mut UART_BASE_ADDR: usize = BASE;

// ── Register helpers ──────────────────────────────────────────────────────────

#[inline(always)]
unsafe fn rd(off: usize) -> u32 {
    let base = UART_BASE_ADDR;
    ((base + off) as *const u32).read_volatile()
}

#[inline(always)]
unsafe fn wr(off: usize, val: u32) {
    let base = UART_BASE_ADDR;
    ((base + off) as *mut u32).write_volatile(val);
}

// ── Initialise ────────────────────────────────────────────────────────────────

pub unsafe fn init() {
    // Default initialization — doesn't touch registers if MMU might be on
}

/// Force set the UART base (e.g. to a virtual address).
pub unsafe fn set_base(base: usize) {
    UART_BASE_ADDR = base;
}

pub unsafe fn reinit(base: usize) {
    UART_BASE_ADDR = base;
    wr(CR,   0);
    wr(IBRD, IBRD_VAL);
    wr(FBRD, FBRD_VAL);
    wr(LCRH, (0b11 << 5) | (1 << 4));
    wr(CR,   (1 << 0) | (1 << 8) | (1 << 9));
}

pub unsafe fn putc(c: u8) {
    // Basic check: if UART_BASE_ADDR is physical and MMU is on, we might fault.
    // However, in early boot we just have to be careful.
    if UART_BASE_ADDR == 0 { return; }
    
    while rd(FR) & FR_TXFF != 0 {
        core::hint::spin_loop();
    }
    wr(DR, c as u32);
}

pub unsafe fn getc() -> Option<u8> {
    if UART_BASE_ADDR == 0 { return None; }
    if rd(FR) & FR_RXFE != 0 {
        None
    } else {
        Some((rd(DR) & 0xFF) as u8)
    }
}

pub unsafe fn has_data() -> bool {
    if UART_BASE_ADDR == 0 { return false; }
    rd(FR) & FR_RXFE == 0
}
