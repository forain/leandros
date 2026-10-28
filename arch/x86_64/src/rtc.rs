//! MC146818 CMOS real-time clock — the battery clock every PC (and QEMU's
//! `q35`/`pc`, which seed it from the host's UTC time) exposes at I/O ports
//! 0x70/0x71. Read once at boot to give CLOCK_REALTIME an epoch; the counter
//! (`timer::monotonic_ns`) carries the clock from there.
//!
//! Ref: Intel/Motorola MC146818 datasheet; OSDev wiki "CMOS".

const CMOS_ADDR: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS:   u8 = 0x04;
const REG_DAY:     u8 = 0x07;
const REG_MONTH:   u8 = 0x08;
const REG_YEAR:    u8 = 0x09;
const REG_STATUS_A: u8 = 0x0A;
const REG_STATUS_B: u8 = 0x0B;
/// ACPI FADT `CENTURY` register on QEMU and most firmware (0 = unsupported).
const REG_CENTURY: u8 = 0x32;

unsafe fn outb(port: u16, val: u8) {
    core::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack));
}

unsafe fn inb(port: u16) -> u8 {
    let v: u8;
    core::arch::asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack));
    v
}

/// Read one CMOS register. Bit 7 of the index is the NMI-disable latch; it
/// is left clear so this never changes the NMI state behind the caller.
unsafe fn cmos_read(reg: u8) -> u8 {
    outb(CMOS_ADDR, reg & 0x7f);
    inb(CMOS_DATA)
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Raw { sec: u8, min: u8, hour: u8, day: u8, mon: u8, year: u8, century: u8 }

unsafe fn read_raw() -> Raw {
    Raw {
        sec:     cmos_read(REG_SECONDS),
        min:     cmos_read(REG_MINUTES),
        hour:    cmos_read(REG_HOURS),
        day:     cmos_read(REG_DAY),
        mon:     cmos_read(REG_MONTH),
        year:    cmos_read(REG_YEAR),
        century: cmos_read(REG_CENTURY),
    }
}

#[inline]
fn bcd(v: u8) -> u8 { (v & 0x0f) + ((v >> 4) * 10) }

/// Days since 1970-01-01 for a proleptic-Gregorian civil date (Howard
/// Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = (m as u64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// Seconds since the Unix epoch from the CMOS clock, or `None` if it holds
/// nothing plausible (no clock, or a battery-dead 1980/2000 default is still
/// accepted — only a date before 2000 or an unreadable register is refused).
pub fn epoch_secs() -> Option<u64> {
    unsafe {
        // Wait out an update in progress (bit 7 of status A, ~2 ms/s), then
        // read until two consecutive snapshots agree so a rollover between
        // registers cannot produce 12:59 → 13:00 mixed as 13:59.
        let mut spins = 0u32;
        while cmos_read(REG_STATUS_A) & 0x80 != 0 {
            spins += 1;
            if spins > 1_000_000 { return None; }
        }
        let mut raw = read_raw();
        for _ in 0..8 {
            let again = read_raw();
            if again == raw { break; }
            raw = again;
        }
        let status_b = cmos_read(REG_STATUS_B);
        let binary = status_b & 0x04 != 0;
        let h24    = status_b & 0x02 != 0;
        let pm     = raw.hour & 0x80 != 0;
        let conv = |v: u8| if binary { v } else { bcd(v) };
        let sec  = conv(raw.sec) as u64;
        let min  = conv(raw.min) as u64;
        let mut hour = conv(raw.hour & 0x7f) as u64;
        if !h24 {
            // 12-hour mode: 12 AM is 0, 12 PM is 12, 1–11 PM add 12.
            hour %= 12;
            if pm { hour += 12; }
        }
        let day  = conv(raw.day) as u32;
        let mon  = conv(raw.mon) as u32;
        let yy   = conv(raw.year) as i64;
        let century = if raw.century != 0 && raw.century != 0xff { conv(raw.century) as i64 } else { 20 };
        let year = century * 100 + yy;
        if year < 2000 || !(1..=12).contains(&mon) || !(1..=31).contains(&day)
            || hour > 23 || min > 59 || sec > 59 {
            return None;
        }
        let days = days_from_civil(year, mon, day);
        if days < 0 { return None; }
        Some(days as u64 * 86_400 + hour * 3_600 + min * 60 + sec)
    }
}

// ---- Alarm (lane s3sysfs). Only CMOS registers 0x01/0x03/0x05 (alarm
// sec/min/hour) and the AIE bit of 0x0B are ever written; never 0x0E..0x7F
// or bank 1 (HP's checksummed NVRAM).
const REG_ALRM_SEC: u8 = 0x01;
const REG_ALRM_MIN: u8 = 0x03;
const REG_ALRM_HOUR: u8 = 0x05;
const REG_STATUS_C: u8 = 0x0C;
const AIE: u8 = 0x20;

unsafe fn cmos_write(reg: u8, v: u8) {
    outb(CMOS_ADDR, reg & 0x7f);
    outb(CMOS_DATA, v);
}

unsafe fn wait_uip() -> bool {
    let mut spins = 0u32;
    while cmos_read(REG_STATUS_A) & 0x80 != 0 {
        spins += 1;
        if spins > 1_000_000 { return false; }
    }
    true
}

/// Arm the alarm for the time-of-day of `target` (Unix seconds, must be
/// 1..=86399 s after the current CMOS time). Returns the hardware register
/// snapshot `(sec, min, hour, status_b)` after programming, or a negative errno.
pub fn set_alarm(target: u64) -> Result<(u8, u8, u8, u8), i32> {
    let now = epoch_secs().ok_or(-19)?;
    if target <= now || target - now >= 86_400 { return Err(-22); }
    unsafe {
        if !wait_uip() { return Err(-16); }
        let b = cmos_read(REG_STATUS_B);
        let binary = b & 0x04 != 0;
        let h24 = b & 0x02 != 0;
        let enc = |v: u8| if binary { v } else { ((v / 10) << 4) | (v % 10) };
        let tod = target % 86_400;
        let (h, m, s) = ((tod / 3600) as u8, ((tod / 60) % 60) as u8, (tod % 60) as u8);
        let hreg = if h24 { enc(h) } else {
            let pm = h >= 12;
            let h12 = if h % 12 == 0 { 12 } else { h % 12 };
            enc(h12) | if pm { 0x80 } else { 0 }
        };
        // Disable while rewriting, clear a stale AF (reading 0x0C), then arm.
        cmos_write(REG_STATUS_B, b & !AIE);
        cmos_write(REG_ALRM_SEC, enc(s));
        cmos_write(REG_ALRM_MIN, enc(m));
        cmos_write(REG_ALRM_HOUR, hreg);
        let _ = cmos_read(REG_STATUS_C);
        cmos_write(REG_STATUS_B, b | AIE);
        Ok((cmos_read(REG_ALRM_SEC), cmos_read(REG_ALRM_MIN), cmos_read(REG_ALRM_HOUR), cmos_read(REG_STATUS_B)))
    }
}

/// Disarm: clear AIE and the pending flag.
pub fn clear_alarm() {
    unsafe {
        let b = cmos_read(REG_STATUS_B);
        cmos_write(REG_STATUS_B, b & !AIE);
        let _ = cmos_read(REG_STATUS_C);
    }
}

/// `(sec, min, hour, status_b)` raw alarm registers, for diagnostics.
pub fn alarm_regs() -> (u8, u8, u8, u8) {
    unsafe { (cmos_read(REG_ALRM_SEC), cmos_read(REG_ALRM_MIN), cmos_read(REG_ALRM_HOUR), cmos_read(REG_STATUS_B)) }
}
