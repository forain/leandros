//! PL011 UART driver for AArch64.

#[allow(unused_imports)]
use leandros_lib::mmio::{MmioRd, MmioWr};

// ── Board-specific constants ──────────────────────────────────────────────────

/// MMIO base address of the PL011.
#[cfg(not(any(feature = "rpi5", feature = "raspi4b")))]
pub const BASE: usize = 0x0900_0000;       // QEMU virt

/// RPi 5 / BCM2712 `uart10` — the PL011 behind the board's dedicated 3-pin
/// JST-SH debug connector, and the node the vendored
/// `target/rpi5-uefi/bcm2712-rpi-5-b.dtb` aliases as `console`. The DTB spells
/// it `serial@7d001000` under the `soc` bus, whose
/// `ranges = <0x7c000000 0x10 0x7c000000 0x4000000>` translates that to CPU
/// physical 0x10_7D001000.
///
/// Explicitly *not* the UART on GPIO 14/15: on the Pi 5 that one lives inside
/// the RP1 southbridge (`/axi/pcie@120000/rp1/serial@30000`, the DTB's
/// `serial0`) and is unreachable without PCIe enumeration and RP1 bring-up. A
/// serial cable on the 40-pin header sees nothing from this driver.
#[cfg(feature = "rpi5")]
pub const BASE: usize = 0x107D_0010_00;

/// QEMU -M raspi4b PL011 (BCM2711 peripheral base 0xFE000000 + UART0 offset
/// 0x201000). Verified live via QMP `info mtree` — see drivers/src/sdhci.rs's
/// top-of-file comment for the rest of the raspi4b address set.
#[cfg(feature = "raspi4b")]
pub const BASE: usize = 0xFE20_1000;

/// Integer baud-rate divisor.
#[cfg(not(any(feature = "rpi5", feature = "raspi4b")))]
const IBRD_VAL: u32 = 13;

// QEMU's raspi4b pl011 model clocks UART0 at 48MHz, giving a 115200-baud
// divisor of 48_000_000 / (16 * 115200) = 26 + 1/24.
#[cfg(feature = "raspi4b")]
const IBRD_VAL: u32 = 26;

// There is deliberately no rpi5 arm here: that path inherits whatever the
// firmware programmed rather than computing its own divisor. See `reinit`.
//
// The DTB is not a usable source for it. `uart10`'s "uartclk" input is the
// `clk_uart` fixed-clock, `clock-frequency = <0x8ca000>` = 9,216,000 Hz, which
// would put the 115200 divisor at exactly 5 — but programming 5 produced
// garbage on real hardware (markers arriving as 0x9E 0xC1 instead of "AB..."),
// while the firmware's own output through the same UART is clean. So the DTB
// node describes what Linux is told to assume, not what the firmware actually
// clocks the port at, and the honest thing is to not second-guess a working
// configuration we can simply keep.

/// Fractional baud-rate divisor.
#[cfg(not(any(feature = "rpi5", feature = "raspi4b")))]
const FBRD_VAL: u32 = 1;

#[cfg(feature = "raspi4b")]
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
const RSR:  usize = 0x004; // RSR on read, ECR on write
const IFLS: usize = 0x034;
/// LCRH.FEN: enable the 32-entry FIFOs. Without it the PL011 holds ONE byte.
const LCRH_FEN: u32 = 1 << 4;
/// DR error bits that travel with each received byte.
const DR_OE: u32 = 1 << 11;
const DR_ERR: u32 = 0xF << 8; // OE | BE | PE | FE

// ── Flag register bits ────────────────────────────────────────────────────────
const FR_RXFE: u32 = 1 << 4;
const FR_TXFF: u32 = 1 << 5;

// ── Runtime UART base ─────────────────────────────────────────────────────────
static mut UART_BASE_ADDR: usize = BASE;

// ── Register helpers ──────────────────────────────────────────────────────────

/// Early-boot addressing (set by `early::setup` on the Limine path, reset by
/// `set_base`/`reinit` once `arch_aarch64::init` maps the HHDM alias).
pub const EARLY_LEGACY: u8 = 0;
/// The physical address translates (bootloader map or `early` hook): use it.
pub const EARLY_IDENT: u8 = 1;
/// Nothing maps the UART yet: drop output instead of faulting.
pub const EARLY_SILENT: u8 = 2;
static EARLY_MODE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(EARLY_LEGACY);

pub fn set_early_mode(m: u8) { EARLY_MODE.store(m, core::sync::atomic::Ordering::Relaxed); }

/// Effective register base, or None when the UART is not reachable yet.
#[inline(always)]
unsafe fn reg_base() -> Option<usize> {
    let mut base = UART_BASE_ADDR;
    match EARLY_MODE.load(core::sync::atomic::Ordering::Relaxed) {
        EARLY_IDENT => return Some(base),
        EARLY_SILENT => return None,
        _ => {}
    }
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
    Some(base)
}

#[inline(always)]
unsafe fn rd(off: usize) -> u32 {
    match reg_base() { Some(b) => ((b + off) as *const u32).mmio_read(), None => 0 }
}

#[inline(always)]
unsafe fn wr(off: usize, val: u32) {
    if let Some(b) = reg_base() { ((b + off) as *mut u32).mmio_write(val) }
}


unsafe fn leave_early() {
    if EARLY_MODE.swap(EARLY_LEGACY, core::sync::atomic::Ordering::Relaxed) != EARLY_LEGACY {
        crate::early::teardown();
    }
}

// ── Initialise ────────────────────────────────────────────────────────────────

pub unsafe fn init() {
    // Default initialization — doesn't touch registers if MMU might be on
}

/// Force set the UART base (e.g. to a virtual address).
pub unsafe fn set_base(base: usize) {
    UART_BASE_ADDR = base;
    leave_early();
}

/// The baud programming `reinit` ended up installing, recorded so `arch::init`
/// can report it. On rpi5 these are read back from the live port rather than
/// chosen, and are the only direct evidence of what the firmware clocks
/// `uart10` at.
pub static mut UART_IBRD_SEEN: u32 = 0;
pub static mut UART_FBRD_SEEN: u32 = 0;

pub unsafe fn reinit(base: usize) {
    UART_BASE_ADDR = base;
    leave_early();

    // Keep the firmware's divisor and line control on real hardware. It has
    // demonstrably got 115200 right — its whole boot log arrives clean through
    // this port — and we have no trustworthy figure for BCM2712's UARTCLK to
    // recompute one from. Overwriting it with a divisor derived from the DTB's
    // clk_uart turned every marker into noise.
    #[cfg(feature = "rpi5")]
    let (ibrd, fbrd, lcrh) = (rd(IBRD), rd(FBRD), rd(LCRH) | LCRH_FEN);
    #[cfg(not(feature = "rpi5"))]
    let (ibrd, fbrd, lcrh) = (IBRD_VAL, FBRD_VAL, (0b11 << 5) | (1 << 4));

    UART_IBRD_SEEN = ibrd;
    UART_FBRD_SEEN = fbrd;

    wr(CR,   0);
    wr(IBRD, ibrd);
    wr(FBRD, fbrd);
    wr(LCRH, lcrh);
    // RX interrupt at 1/8 full (4 bytes), TX at 1/8; leftovers below the
    // trigger raise the receive-timeout interrupt (RTIM) instead. A low RX
    // trigger leaves 28 of the 32 FIFO slots as headroom for IRQ latency
    // (~2.4 ms at 115200; the kernel runs syscalls with IRQs masked).
    // Explicit, because on the Pi the firmware owns the port before us.
    wr(IFLS, (0b000 << 3) | 0b000);
    wr(RSR, 0); // clear latched errors
    wr(CR,   (1 << 0) | (1 << 8) | (1 << 9));

    // Enable RX interrupt (bit 4) and RT interrupt (bit 6)
    wr(IMSC, (1 << 4) | (1 << 6));
}

/// Unmask (`true`) or mask the RX + RX-timeout interrupts — serial input flow
/// control, see `evdev_server::serial_rx_drain`. Touches the device only on a
/// change, so the per-tick call costs no MMIO exit in the common case.
pub unsafe fn set_rx_irq(on: bool) {
    use core::sync::atomic::{AtomicBool, Ordering::Relaxed};
    static RX_MASKED: AtomicBool = AtomicBool::new(false);
    if UART_BASE_ADDR == 0 || RX_MASKED.load(Relaxed) != on { return; }
    RX_MASKED.store(!on, Relaxed);
    wr(IMSC, if on { (1 << 4) | (1 << 6) } else { 0 });
}

/// Diagnostic snapshot of the console PL011's interrupt state:
/// `(IMSC, RIS, MIS, FR, CR)`; zeros before the port is mapped.
pub fn irq_regs() -> (u32, u32, u32, u32, u32) {
    unsafe {
        if UART_BASE_ADDR == 0 { return (0, 0, 0, 0, 0); }
        (rd(IMSC), rd(0x03C), rd(0x040), rd(FR), rd(CR))
    }
}

/// Raw `UARTIMSC` write, for `gic::probe_uart_line` only (it restores the
/// value it read from `irq_regs`, so the RX-mask bookkeeping stays valid).
pub unsafe fn write_imsc(v: u32) {
    if UART_BASE_ADDR == 0 { return; }
    wr(IMSC, v);
}

/// True when the RX FIFO holds at least one byte (FR.RXFE clear).
pub fn rx_pending() -> bool {
    unsafe { UART_BASE_ADDR != 0 && rd(FR) & FR_RXFE == 0 }
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

/// `putc_patient`'s own latch (see `arch_x86_64::USER_TX_WEDGED`).
static USER_TX_WEDGED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Console bytes dropped because the PL011 TX FIFO never drained in time. The
/// x86_64 counterpart is `arch_x86_64::UART_TX_DROPPED`.
pub static UART_TX_DROPPED: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

pub unsafe fn putc(c: u8) {
    putc_within(c, tx_wait_ticks(), &TX_WEDGED);
}

/// `putc` for process-context console output (a user `write(2)` to the
/// console): 20x the wait (200 ms) and its own latch. A slow host reader is
/// back-pressure a tty writer should wait out, not output to discard; see
/// `arch_x86_64::putc_patient` for the measured loss this prevents (there the
/// socket chardev buffers only ~278 one-byte writes on a Linux host).
pub unsafe fn putc_patient(c: u8) {
    putc_within(c, tx_wait_ticks().saturating_mul(20), &USER_TX_WEDGED);
}

unsafe fn putc_within(c: u8, wait_ticks: u64, wedged: &core::sync::atomic::AtomicBool) {
    use core::sync::atomic::Ordering::Relaxed;
    // Basic check: if UART_BASE_ADDR is physical and MMU is on, we might fault.
    if UART_BASE_ADDR == 0 { return; }

    // Deadline + wedge latch, for the same reason as the x86_64 16550 path (see
    // arch/x86_64/src/lib.rs::putc): `putc` runs in IRQ context, and a host
    // chardev back end that stops accepting bytes leaves TXFF asserted
    // indefinitely. Waiting for it without a deadline freezes the tick, the
    // scheduler and the virtio-input drain on this CPU; dropping the byte does
    // not. Console output may be lost, an interrupt handler may not be stalled.
    if wedged.load(Relaxed) {
        if rd(FR) & FR_TXFF != 0 {
            UART_TX_DROPPED.fetch_add(1, Relaxed);
            return;
        }
        wedged.store(false, Relaxed);
    } else {
        let deadline = cntvct_raw().wrapping_add(wait_ticks);
        loop {
            let fr = rd(FR);
            if fr & FR_TXFF == 0 { break; }
            // Waiting out the TX FIFO (~87 us a byte at 115200) usually runs
            // with IRQs masked (syscalls do), long enough for a pasted line
            // to overrun the 32-byte RX FIFO. Keep RX moving meanwhile.
            if fr & FR_RXFE == 0 { rx_rescue(); }
            gap_mark();
            if cntvct_raw().wrapping_sub(deadline) < (1u64 << 63) {
                wedged.store(true, Relaxed);
                UART_TX_DROPPED.fetch_add(1, Relaxed);
                return;
            }
            core::hint::spin_loop();
        }
    }
    wr(DR, c as u32);
}

/// Software extension of the RX FIFO. `rx_rescue` (TX wait loop, any CPU,
/// any IRQ state) moves hardware bytes in; `getc` takes them out first, so
/// byte order is kept. Both hold `RX_LOCK` for a few MMIO reads at most.
const RX_RING_LEN: usize = 1024;
static mut RX_RING: [u8; RX_RING_LEN] = [0; RX_RING_LEN];
static mut RX_HEAD: usize = 0; // next pop
static mut RX_TAIL: usize = 0; // next push
static RX_LOCK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Take `RX_LOCK` with IRQs masked (returns the saved DAIF): `uart_irq` ->
/// `getc` spins on it, so the holder must not be interruptible on its CPU.
#[inline(always)]
fn rx_try_lock() -> Option<u64> {
    use core::sync::atomic::Ordering::*;
    let daif: u64;
    unsafe { core::arch::asm!("mrs {}, daif", "msr daifset, #2", out(reg) daif, options(nomem, nostack)); }
    if RX_LOCK.compare_exchange(false, true, Acquire, Relaxed).is_ok() { return Some(daif); }
    unsafe { core::arch::asm!("msr daif, {}", in(reg) daif, options(nomem, nostack)); }
    None
}
#[inline(always)]
fn rx_unlock(daif: u64) {
    RX_LOCK.store(false, core::sync::atomic::Ordering::Release);
    unsafe { core::arch::asm!("msr daif, {}", in(reg) daif, options(nomem, nostack)); }
}

/// Read one byte from the hardware FIFO (caller saw RXFE clear), counting
/// overrun / framing errors.
unsafe fn rx_hw_byte() -> u8 {
    use core::sync::atomic::Ordering::Relaxed;
    let d = rd(DR);
    if d & DR_ERR != 0 {
        if d & DR_OE != 0 { UART_RX_OVERRUNS.fetch_add(1, Relaxed); }
        else { UART_RX_ERRORS.fetch_add(1, Relaxed); }
        wr(RSR, 0);
    }
    (d & 0xFF) as u8
}

/// Move the RX FIFO into `RX_RING` and pend the UART interrupt so the normal
/// drain (`uart_irq` -> `getc`) delivers the bytes as soon as IRQs open.
unsafe fn rx_rescue() {
    use core::sync::atomic::Ordering::Relaxed;
    let Some(daif) = rx_try_lock() else { return };
    let mut n = 0u64;
    while RX_TAIL.wrapping_sub(RX_HEAD) < RX_RING_LEN && rd(FR) & FR_RXFE == 0 {
        RX_RING[RX_TAIL % RX_RING_LEN] = rx_hw_byte();
        RX_TAIL = RX_TAIL.wrapping_add(1);
        n += 1;
    }
    rx_unlock(daif);
    if n != 0 {
        UART_RX_RESCUED.fetch_add(n, Relaxed);
        super::gic::pend_uart_irq();
    }
}

/// Service the RX FIFO from inside a long IRQ-masked section (console write,
/// framebuffer scroll/blit): one FR read, and `rx_rescue` when bytes wait.
/// Also a service point for the `txgap` measurement.
pub fn rx_poll() {
    unsafe {
        if UART_BASE_ADDR == 0 { return; }
        gap_mark();
        if rd(FR) & FR_RXFE == 0 { rx_rescue(); }
    }
}

/// `txgap`: the longest stretch of a console write (`tx_window_begin` ..
/// `tx_window_end`) that went without an RX service point (`rx_poll`, or a
/// TX-wait iteration, which also samples RX). Console writes run with IRQs
/// masked, so this is the IRQ-off window the console output path imposes on
/// RX; it must stay well under one RX FIFO (32 bytes = ~2.8 ms at 115200).
/// CNTVCT ticks; 0 = no window open.
static TXGAP_LAST: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static TXGAP_MAX_TICKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Longest cpu0 timer-interrupt lateness (the tick fired this long after its
/// grid point): an upper bound on any IRQ-masked section on the CPU the RX
/// interrupt targets, console or not. CNTVCT ticks; see `timer::on_tick`.
pub static IRQ_LATE_MAX_TICKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

#[inline(always)]
fn gap_mark() {
    use core::sync::atomic::Ordering::Relaxed;
    let last = TXGAP_LAST.load(Relaxed);
    if last == 0 { return; }
    let now = unsafe { cntvct_raw() };
    TXGAP_MAX_TICKS.fetch_max(now.wrapping_sub(last), Relaxed);
    TXGAP_LAST.store(now, Relaxed);
}

pub fn tx_window_begin() {
    TXGAP_LAST.store(unsafe { cntvct_raw() } | 1, core::sync::atomic::Ordering::Relaxed);
}

pub fn tx_window_end() {
    gap_mark();
    TXGAP_LAST.store(0, core::sync::atomic::Ordering::Relaxed);
}

/// CNTVCT ticks -> microseconds.
pub fn ticks_to_us(t: u64) -> u64 {
    let f: u64;
    unsafe { core::arch::asm!("mrs {}, cntfrq_el0", out(reg) f, options(nomem, nostack)); }
    if f == 0 { 0 } else { (t as u128 * 1_000_000 / f as u128) as u64 }
}

/// `(txgap_max_us, irq_late_max_us)` for the `[uart]` line and /proc/kiostat.
pub fn irqoff_stats_us() -> (u64, u64) {
    use core::sync::atomic::Ordering::Relaxed;
    (ticks_to_us(TXGAP_MAX_TICKS.load(Relaxed)), ticks_to_us(IRQ_LATE_MAX_TICKS.load(Relaxed)))
}

/// Interrupts taken by the console UART (`exception::uart_irq`).
pub static UART_RX_IRQS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
/// Bytes the cpu0 tick fallback drained, i.e. bytes the RX IRQ did not take.
pub static UART_RX_TICK_BYTES: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
/// Bytes moved out of the RX FIFO while a console write waited for TX room.
pub static UART_RX_RESCUED: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Blocking putc for the explicit, human-triggered Ctrl-T task dump — see
/// `arch_x86_64::putc_dump` for why this exists as a separate path from the
/// sticky-`TX_WEDGED` `putc` above (shared reasoning, PL011 FR_TXFF in place
/// of 16550 LSR.THRE).
pub unsafe fn putc_dump(c: u8) {
    use core::sync::atomic::Ordering::Relaxed;
    if UART_BASE_ADDR == 0 { return; }
    let deadline = cntvct_raw().wrapping_add(tx_wait_ticks());
    while rd(FR) & FR_TXFF != 0 {
        if cntvct_raw().wrapping_sub(deadline) < (1u64 << 63) {
            UART_TX_DROPPED.fetch_add(1, Relaxed);
            return;
        }
        core::hint::spin_loop();
    }
    wr(DR, c as u32);
}

pub unsafe fn getc() -> Option<u8> {
    if UART_BASE_ADDR == 0 { return None; }
    // Short hold on the other side (a few MMIO reads); spin for it so a
    // rescued byte is never overtaken by a later hardware byte.
    let daif = loop {
        if let Some(d) = rx_try_lock() { break d; }
        core::hint::spin_loop();
    };
    let r = if RX_HEAD != RX_TAIL {
        let b = RX_RING[RX_HEAD % RX_RING_LEN];
        RX_HEAD = RX_HEAD.wrapping_add(1);
        Some(b)
    } else if rd(FR) & FR_RXFE != 0 {
        None
    } else {
        Some(rx_hw_byte())
    };
    rx_unlock(daif);
    r
}

/// Receive overruns: the RX FIFO was full when a byte arrived, so at least
/// one byte was lost (flagged on the next byte read). Nonzero means the RX
/// drain is too slow — see the `[uart]` line in kernel/src/iostat.rs.
pub static UART_RX_OVERRUNS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
/// Framing / parity / break errors on received bytes.
pub static UART_RX_ERRORS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

pub unsafe fn has_data() -> bool {
    if UART_BASE_ADDR == 0 { return false; }
    core::ptr::read_volatile(&raw const RX_HEAD) != core::ptr::read_volatile(&raw const RX_TAIL)
        || rd(FR) & FR_RXFE == 0
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
