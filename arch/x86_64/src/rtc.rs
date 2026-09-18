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
