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
const IMSC: usize = 0x038;
const ICR:  usize = 0x044;

// ── Flag register bits ────────────────────────────────────────────────────────
const FR_RXFE: u32 = 1 << 4;
const FR_TXFF: u32 = 1 << 5;

// ── Runtime UART base ─────────────────────────────────────────────────────────
static mut UART_BASE_ADDR: usize = BASE;

// ── Register helpers ──────────────────────────────────────────────────────────

#[inline(always)]
unsafe fn rd(off: usize) -> u32 {
    let mut base = UART_BASE_ADDR;
    // If MMU is enabled (TTBR1_EL1 is not 0), use the HHDM address
    let ttbr1: u64;
    core::arch::asm!("mrs {}, ttbr1_el1", out(reg) ttbr1);
    if ttbr1 != 0 && base < 0xFFFF_0000_0000_0000 {
        extern "C" { fn mm_get_hhdm_offset() -> u64; }
        let hhdm = mm_get_hhdm_offset() as usize;
        if hhdm != 0 {
            base += hhdm;
        }
    }
    ((base + off) as *const u32).read_volatile()
}

#[inline(always)]
unsafe fn wr(off: usize, val: u32) {
    let mut base = UART_BASE_ADDR;
    let ttbr1: u64;
    core::arch::asm!("mrs {}, ttbr1_el1", out(reg) ttbr1);
    if ttbr1 != 0 && base < 0xFFFF_0000_0000_0000 {
        extern "C" { fn mm_get_hhdm_offset() -> u64; }
        let hhdm = mm_get_hhdm_offset() as usize;
        if hhdm != 0 {
            base += hhdm;
        }
    }
    ((base + off) as *mut u32).write_volatile(val)
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
    
    // Enable RX interrupt (bit 4) and RT interrupt (bit 6)
    wr(IMSC, (1 << 4) | (1 << 6));
}

pub unsafe fn clear_irq() {
    if UART_BASE_ADDR == 0 { return; }
    wr(ICR, (1 << 4) | (1 << 6));
}

pub unsafe fn putc(c: u8) {
    // Basic check: if UART_BASE_ADDR is physical and MMU is on, we might fault.
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

pub fn serial_print_str(s: &str) {
    for &b in s.as_bytes() {
        unsafe { putc(b); }
    }
}

pub fn print_hex(mut n: usize) {
    let mut buf = [0u8; 16];
    let mut i = 16;
    if n == 0 {
        serial_print_str("0000000000000000");
        return;
    }
    while n > 0 {
        i -= 1;
        buf[i] = b"0123456789abcdef"[(n & 0xF) as usize];
        n >>= 4;
    }
    // Pad with zeros to 16 chars
    while i > 0 {
        i -= 1;
        buf[i] = b'0';
    }
    for &c in &buf {
        unsafe { putc(c); }
    }
}
