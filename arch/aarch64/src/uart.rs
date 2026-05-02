//! PL011 UART driver for AArch64.
//!
//! **QEMU -machine virt** (default):
//!   Base: 0x0900_0000  UARTCLK: 24 MHz → IBRD=13, FBRD=1  (115200 baud)
//!
//! **Raspberry Pi 5 (BCM2712 / RP1)** — enabled by the `rpi5` cargo feature:
//!   Base: 0x107D_0010_00  UARTCLK: 48 MHz → IBRD=26, FBRD=3  (115200 baud)
//!
//! Baud divisor formula:  BAUDDIV = UARTCLK / (16 × baud)
//!   QEMU:  24_000_000 / (16 × 115_200) ≈ 13.0208  → IBRD=13, FBRD=round(0.0208×64)=1
//!   RPi5:  48_000_000 / (16 × 115_200) ≈ 26.0417  → IBRD=26, FBRD=round(0.0417×64)=3

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
const DR:   usize = 0x000; // Data register (write = TX, read = RX)
const FR:   usize = 0x018; // Flag register
const IBRD: usize = 0x024; // Integer baud-rate divisor
const FBRD: usize = 0x028; // Fractional baud-rate divisor
const LCRH: usize = 0x02C; // Line control register (high)
const CR:   usize = 0x030; // Control register

// ── Flag register bits ────────────────────────────────────────────────────────
const FR_RXFE: u32 = 1 << 4; // RX FIFO empty
const FR_TXFF: u32 = 1 << 5; // TX FIFO full — spin until clear before writing

// ── Runtime UART base ─────────────────────────────────────────────────────────
//
// Initialised to the compile-time BASE constant; updated by `reinit()` when
// the DTB reports a different address.  Using a static mut ensures we don't
// depend on atomic operations in early boot.

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

// ── Initialise the PL011 for 115 200 8N1 with FIFO enabled ───────────────────

/// Initialise the PL011 UART at the compile-time `BASE` address.
pub unsafe fn init() {
    UART_BASE_ADDR = BASE;

    // Disable UART while programming line-control registers.
    wr(CR, 0);

    wr(IBRD, IBRD_VAL);
    wr(FBRD, FBRD_VAL);

    // LCRH: WLEN = 0b11 (8-bit), FEN = 1 (FIFO enable).
    wr(LCRH, (0b11 << 5) | (1 << 4));

    // CR: UARTEN (bit 0) | TXE (bit 8) | RXE (bit 9).
    wr(CR, (1 << 0) | (1 << 8) | (1 << 9));
}

/// Re-initialise the PL011 at a runtime-discovered base address.
pub unsafe fn reinit(base: usize) {
    // Update the runtime base BEFORE any register access so wr() uses it.
    UART_BASE_ADDR = base;

    wr(CR,   0);
    wr(IBRD, IBRD_VAL);
    wr(FBRD, FBRD_VAL);
    wr(LCRH, (0b11 << 5) | (1 << 4));
    wr(CR,   (1 << 0) | (1 << 8) | (1 << 9));
}

/// Write one byte to the TX FIFO, spinning until space is available.
pub unsafe fn putc(c: u8) {
    while rd(FR) & FR_TXFF != 0 {
        core::hint::spin_loop();
    }
    wr(DR, c as u32);
}

/// Read one byte from the RX FIFO, or return `None` if empty.
pub unsafe fn getc() -> Option<u8> {
    if rd(FR) & FR_RXFE != 0 {
        None
    } else {
        Some((rd(DR) & 0xFF) as u8)
    }
}

/// Check if the RX FIFO has data.
pub unsafe fn has_data() -> bool {
    rd(FR) & FR_RXFE == 0
}
