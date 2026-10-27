//! Pure (hardware-free) half of the Raspberry Pi 5 SoC housekeeping drivers in
//! `rpi_misc.rs`: the iproc-rng200 hardware RNG, the BCM2712 PM watchdog and
//! reset/halt sequences, the AVS temperature conversion, the firmware property
//! tags for the RTC / temperature / ARM clock, and the cpufreq governor.
//! No MMIO and no `alloc` here: the host tests (drivers/rpi-misc-tests)
//! include this file verbatim and drive it against mocks.

// ── Register access seam ─────────────────────────────────────────────────────

/// 32-bit register window (one device block). Offsets are bytes.
pub trait Regs32 {
    fn r(&mut self, off: usize) -> u32;
    fn w(&mut self, off: usize, v: u32);
}

// ── iproc-rng200 (`brcm,bcm2711-rng200`, Linux drivers/char/hw_random/iproc-rng200.c) ──

pub mod rng200 {
    use super::Regs32;

    pub const CTRL: usize = 0x00;
    pub const SOFT_RESET: usize = 0x04;
    pub const RBG_SOFT_RESET: usize = 0x08;
    pub const INT_STATUS: usize = 0x18;
    pub const FIFO_DATA: usize = 0x20;
    pub const FIFO_COUNT: usize = 0x24;

    pub const CTRL_RBGEN_MASK: u32 = 0x1FFF;
    pub const CTRL_RBGEN_ENABLE: u32 = 0x1;
    pub const INT_MASTER_FAIL_LOCKUP: u32 = 0x8000_0000;
    pub const INT_NIST_FAIL: u32 = 0x20;
    pub const FIFO_COUNT_MASK: u32 = 0xFF;

    pub fn enable_set(r: &mut dyn Regs32, on: bool) {
        let mut v = r.r(CTRL) & !CTRL_RBGEN_MASK;
        if on { v |= CTRL_RBGEN_ENABLE; }
        r.w(CTRL, v);
    }

    /// `iproc_rng200_restart`: disable, clear the lockup, pulse both soft
    /// resets, re-enable.
    pub fn restart(r: &mut dyn Regs32) {
        enable_set(r, false);
        let s = r.r(INT_STATUS);
        r.w(INT_STATUS, s | INT_MASTER_FAIL_LOCKUP);
        for off in [SOFT_RESET, RBG_SOFT_RESET] {
            let v = r.r(off);
            r.w(off, v | 1);
            r.w(off, v & !1);
        }
        enable_set(r, true);
    }

    /// True when the generator reports a fatal condition and needs `restart`.
    pub fn failed(r: &mut dyn Regs32) -> bool {
        r.r(INT_STATUS) & (INT_MASTER_FAIL_LOCKUP | INT_NIST_FAIL) != 0
    }

    /// Words currently waiting in the output FIFO.
    pub fn fifo_count(r: &mut dyn Regs32) -> u32 { r.r(FIFO_COUNT) & FIFO_COUNT_MASK }

    /// Fill a prefix of `out` from the FIFO without waiting. Returns the number
    /// of bytes written (whole or partial words; 0 = FIFO empty or the
    /// generator failed, in which case it is restarted for next time).
    pub fn read(r: &mut dyn Regs32, out: &mut [u8]) -> usize {
        if failed(r) {
            restart(r);
            return 0;
        }
        let mut done = 0;
        while done < out.len() {
            if fifo_count(r) == 0 { break; }
            let w = r.r(FIFO_DATA).to_le_bytes();
            let n = (out.len() - done).min(4);
            out[done..done + n].copy_from_slice(&w[..n]);
            done += n;
        }
        done
    }
}

// ── BCM2712 PM block (`brcm,bcm2712-pm`, Linux bcm2835_wdt.c) ────────────────

pub mod pm {
    use super::Regs32;

    pub const RSTC: usize = 0x1c;
    pub const RSTS: usize = 0x20;
    pub const WDOG: usize = 0x24;

    pub const PASSWORD: u32 = 0x5a00_0000;
    pub const WDOG_TIME_SET: u32 = 0x000f_ffff;
    pub const RSTC_WRCFG_CLR: u32 = 0xffff_ffcf;
    pub const RSTC_WRCFG_FULL_RESET: u32 = 0x0000_0020;
    pub const RSTC_RESET: u32 = 0x0000_0102;
    /// Boot partition 63: the firmware's "halt, wait for the power button".
    pub const RSTS_RASPBERRYPI_HALT: u32 = 0x555;
    /// The watchdog counts 1/65536 s ticks; 20 bits ⇒ just under 16 s.
    pub const MAX_SECS: u32 = WDOG_TIME_SET >> 16;

    pub fn secs_to_ticks(secs: u32) -> u32 { (secs.min(MAX_SECS).max(1) << 16) & WDOG_TIME_SET }

    /// Arm (or re-arm = kick) the watchdog to fire a full reset in `ticks`.
    pub fn arm(r: &mut dyn Regs32, ticks: u32) {
        r.w(WDOG, PASSWORD | (ticks & WDOG_TIME_SET));
        let c = r.r(RSTC);
        r.w(RSTC, PASSWORD | (c & RSTC_WRCFG_CLR) | RSTC_WRCFG_FULL_RESET);
    }

    pub fn disarm(r: &mut dyn Regs32) { r.w(RSTC, PASSWORD | RSTC_RESET); }

    pub fn is_running(r: &mut dyn Regs32) -> bool { r.r(RSTC) & RSTC_WRCFG_FULL_RESET != 0 }

    /// `__bcm2835_restart`: full reset ~150 µs from now.
    pub fn restart(r: &mut dyn Regs32) { arm(r, 10); }

    /// `bcm2835_power_off`: boot partition 63 (firmware halt) then reset.
    pub fn power_off(r: &mut dyn Regs32) {
        let v = r.r(RSTS);
        r.w(RSTS, PASSWORD | v | RSTS_RASPBERRYPI_HALT);
        restart(r);
    }
}

// ── AVS temperature monitor (`brcm,bcm2711-thermal`) ─────────────────────────

pub mod avs {
    /// `AVS_RO_TEMP_STATUS` inside the avs-monitor syscon.
    pub const TEMP_STATUS: usize = 0x200;
    pub const VALID_MASK: u32 = (1 << 16) | (1 << 10);
    pub const DATA_MASK: u32 = 0x3ff;
    /// The D0 dts `cpu-thermal` coefficients: <0xfffffdda 0x6ddd0>.
    pub const SLOPE: i32 = -550;
    pub const OFFSET: i32 = 450_000;

    /// Raw status word → millidegrees Celsius (None: not valid yet).
    pub fn millicelsius(raw: u32) -> Option<i32> {
        if raw & VALID_MASK == 0 { return None; }
        Some(SLOPE * (raw & DATA_MASK) as i32 + OFFSET)
    }
}

// ── Firmware property tags ───────────────────────────────────────────────────

pub mod fw {
    pub const GET_CLOCK_RATE: u32 = 0x0003_0002;
    pub const GET_MAX_CLOCK_RATE: u32 = 0x0003_0004;
    pub const GET_MIN_CLOCK_RATE: u32 = 0x0003_0007;
    pub const SET_CLOCK_RATE: u32 = 0x0003_8002;
    pub const GET_TEMPERATURE: u32 = 0x0003_0006;
    pub const GET_RTC_REG: u32 = 0x0003_0087;
    pub const SET_RTC_REG: u32 = 0x0003_8087;

    pub const CLK_ARM: u32 = 3;
    pub const RTC_TIME: u32 = 0;

    pub const RESP_OK: u32 = 0x8000_0000;
    /// Value words reserved per tag (requests and replies here need ≤ 3).
    pub const VAL_WORDS: usize = 3;
    pub const MSG_WORDS: usize = 5 + VAL_WORDS + 1;

    /// One-tag property message: size, request code, tag, value buffer size,
    /// request/response code, `VAL_WORDS` value words, end tag.
    pub fn build(tag: u32, vals: &[u32], msg: &mut [u32; MSG_WORDS]) {
        *msg = [0; MSG_WORDS];
        msg[0] = (MSG_WORDS * 4) as u32;
        msg[2] = tag;
        msg[3] = (VAL_WORDS * 4) as u32;
        let n = vals.len().min(VAL_WORDS);
        msg[5..5 + n].copy_from_slice(&vals[..n]);
    }

    /// The tag's value words from a reply, if the firmware handled the tag.
    pub fn parse(reply: &[u32]) -> Option<[u32; VAL_WORDS]> {
        if reply.len() < 5 + VAL_WORDS || reply[1] != RESP_OK || reply[4] & RESP_OK == 0 {
            return None;
        }
        let mut out = [0u32; VAL_WORDS];
        out.copy_from_slice(&reply[5..5 + VAL_WORDS]);
        Some(out)
    }

    /// 2020-01-01T00:00:00Z. A firmware RTC below this was never set.
    pub const RTC_SANE_MIN: u32 = 1_577_836_800;
    pub fn rtc_plausible(secs: u32) -> bool { secs >= RTC_SANE_MIN }
}

// ── cpufreq governor (ondemand-style, two-level) ─────────────────────────────

/// Decides the ARM clock from periodic load samples. A sample is "busy" when
/// any CPU had runnable work besides the governor itself. Like ondemand:
/// jump straight to `max` on load, step down to `min` only after the whole
/// window has stayed mostly idle.
pub struct Governor {
    pub min_hz: u32,
    pub max_hz: u32,
    pub cur_hz: u32,
    /// Busy flags of the last `WINDOW` samples, newest in bit 0.
    hist: u32,
}

impl Governor {
    pub const WINDOW: u32 = 10;
    /// Step down when at most this many of the last `WINDOW` samples were busy.
    pub const DOWN_BUSY_MAX: u32 = 1;

    pub fn new(min_hz: u32, max_hz: u32, cur_hz: u32) -> Self {
        Governor { min_hz, max_hz: max_hz.max(min_hz), cur_hz, hist: 0 }
    }

    /// Feed one sample; returns the new target if the clock should change.
    pub fn sample(&mut self, busy: bool) -> Option<u32> {
        self.hist = ((self.hist << 1) | busy as u32) & ((1 << Self::WINDOW) - 1);
        let target = if busy {
            self.max_hz
        } else if self.hist.count_ones() <= Self::DOWN_BUSY_MAX {
            self.min_hz
        } else {
            self.cur_hz
        };
        if target != self.cur_hz && self.min_hz != 0 {
            self.cur_hz = target;
            Some(target)
        } else {
            None
        }
    }
}

/// `nr_running` includes the sampling thread itself on its own CPU.
pub fn busy_from_nr_running(nr: u32) -> bool { nr > 1 }

// ── Small formatting helper (no alloc) ───────────────────────────────────────

pub struct Buf<'a> { pub b: &'a mut [u8], pub n: usize }

impl<'a> Buf<'a> {
    pub fn new(b: &'a mut [u8]) -> Self { Buf { b, n: 0 } }
    pub fn s(&mut self, s: &str) -> &mut Self {
        for &c in s.as_bytes() { if self.n < self.b.len() { self.b[self.n] = c; self.n += 1; } }
        self
    }
    pub fn u(&mut self, mut v: u64) -> &mut Self {
        let mut t = [0u8; 20];
        let mut i = 0;
        loop { t[i] = b'0' + (v % 10) as u8; i += 1; v /= 10; if v == 0 { break; } }
        while i > 0 { i -= 1; let c = t[i]; if self.n < self.b.len() { self.b[self.n] = c; self.n += 1; } }
        self
    }
    pub fn i(&mut self, v: i64) -> &mut Self {
        if v < 0 { self.s("-"); }
        self.u(v.unsigned_abs())
    }
    /// Millidegrees as "52.3".
    pub fn mc(&mut self, m: i32) -> &mut Self {
        if m < 0 { self.s("-"); }
        let a = m.unsigned_abs();
        self.u((a / 1000) as u64).s(".").u(((a % 1000) / 100) as u64)
    }
}
