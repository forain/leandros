//! PL011 UART driver for AArch64.

// ── Board-specific constants ──────────────────────────────────────────────────

/// MMIO base address of the PL011.
#[cfg(not(any(feature = "rpi5", feature = "raspi4b")))]
pub const BASE: usize = 0x0900_0000;       // QEMU virt

#[cfg(feature = "rpi5")]
pub const BASE: usize = 0x107D_0010_00;    // RPi 5 RP1 UART0

/// QEMU -M raspi4b PL011 (BCM2711 peripheral base 0xFE000000 + UART0 offset
/// 0x201000). Verified live via QMP `info mtree` — see drivers/src/sdhci.rs's
/// top-of-file comment for the rest of the raspi4b address set.
#[cfg(feature = "raspi4b")]
pub const BASE: usize = 0xFE20_1000;

/// Integer baud-rate divisor.
#[cfg(not(any(feature = "rpi5", feature = "raspi4b")))]
const IBRD_VAL: u32 = 13;

// rpi5 and raspi4b's QEMU pl011 model both clock UART0 at 48MHz, giving the
// same 115200-baud divisor (48_000_000 / (16 * 115200) = 26 + 1/24).
#[cfg(any(feature = "rpi5", feature = "raspi4b"))]
const IBRD_VAL: u32 = 26;

/// Fractional baud-rate divisor.
#[cfg(not(any(feature = "rpi5", feature = "raspi4b")))]
const FBRD_VAL: u32 = 1;

#[cfg(any(feature = "rpi5", feature = "raspi4b"))]
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

/// How long `putc` will wait for the PL011 TX FIFO before giving the byte up,
/// in CNTVCT ticks. Derived from CNTFRQ_EL0 so it is a real ~10 ms regardless of
/// the counter's rate, with a fixed fallback if firmware left CNTFRQ zero.
#[inline(always)]
unsafe fn tx_wait_ticks() -> u64 {
    let f: u64;
    core::arch::asm!("mrs {}, cntfrq_el0", out(reg) f, options(nomem, nostack));
    if f == 0 { 100_000 } else { f / 100 }
}

#[inline(always)]
unsafe fn cntvct_raw() -> u64 {
    let v: u64;
    core::arch::asm!("mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack));
    v
}

/// Latched when a `putc` wait expires; cleared by the first later probe that
/// finds the FIFO with room. See `arch_x86_64::putc` for why both exist.
static TX_WEDGED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Console bytes dropped because the PL011 TX FIFO never drained in time. The
/// x86_64 counterpart is `arch_x86_64::UART_TX_DROPPED`.
pub static UART_TX_DROPPED: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

pub unsafe fn putc(c: u8) {
    use core::sync::atomic::Ordering::Relaxed;
    // Basic check: if UART_BASE_ADDR is physical and MMU is on, we might fault.
    if UART_BASE_ADDR == 0 { return; }

    // Deadline + wedge latch, for the same reason as the x86_64 16550 path (see
    // arch/x86_64/src/lib.rs::putc): `putc` runs in IRQ context, and a host
    // chardev back end that stops accepting bytes leaves TXFF asserted
    // indefinitely. Waiting for it without a deadline freezes the tick, the
    // scheduler and the virtio-input drain on this CPU; dropping the byte does
    // not. Console output may be lost, an interrupt handler may not be stalled.
    if TX_WEDGED.load(Relaxed) {
        if rd(FR) & FR_TXFF != 0 {
            UART_TX_DROPPED.fetch_add(1, Relaxed);
            return;
        }
        TX_WEDGED.store(false, Relaxed);
    } else {
        let deadline = cntvct_raw().wrapping_add(tx_wait_ticks());
        while rd(FR) & FR_TXFF != 0 {
            if cntvct_raw().wrapping_sub(deadline) < (1u64 << 63) {
                TX_WEDGED.store(true, Relaxed);
                UART_TX_DROPPED.fetch_add(1, Relaxed);
                return;
            }
            core::hint::spin_loop();
        }
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
