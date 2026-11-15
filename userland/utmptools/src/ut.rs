//! Shared by w, users, lastlog and utmptest: utmp/lastlog readers and a
//! UTC time formatter. No allocator.
#![allow(dead_code)]

use crate::common::*;
use core::fmt::Write;
use leandros_libc::syscall::{nr, syscall3};

#[path = "../../../lib/utmpx/utmpx.rs"]
pub mod utmpx;
pub use utmpx::*;

pub const UTMP: &[u8] = b"/run/utmps/utmp";
pub const LASTLOG: &[u8] = b"/var/log/lastlog";

pub fn cstr(b: &[u8]) -> &[u8] {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    &b[..n]
}

/// Call `f` for every record of /run/utmps/utmp (shared flock held, so a
/// writer's read-modify-write is never seen half done). False if unreadable.
pub fn each_utmp(mut f: impl FnMut(&Utmpx)) -> bool {
    let fd = match sys_open(UTMP, O_RDONLY) { Ok(f) => f, Err(_) => return false };
    unsafe { syscall3(nr::FLOCK, fd as usize, 1, 0); }
    let mut b = [0u8; REC];
    loop {
        let mut n = 0;
        while n < REC {
            match sys_read(fd, &mut b[n..]) { Ok(0) | Err(_) => break, Ok(k) => n += k }
        }
        if n < REC { break; }
        f(&Utmpx::from_bytes(&b));
    }
    unsafe { syscall3(nr::FLOCK, fd as usize, 8, 0); }
    sys_close(fd);
    true
}

/// /etc/passwd lookups: iterate (name, uid, ...) entries.
pub fn each_passwd(buf: &mut [u8], mut f: impl FnMut(&[u8], u32)) {
    let n = read_file(b"/etc/passwd", buf).unwrap_or(0);
    for line in buf[..n].split(|&c| c == b'\n') {
        let mut it = line.split(|&c| c == b':');
        let name = it.next().unwrap_or(b"");
        let _ = it.next();
        if let Some(uid) = it.next().and_then(parse_u64) {
            if !name.is_empty() { f(name, uid as u32); }
        }
    }
}

/// Days since 1970-01-01 -> (year, month 1-12, day 1-31).
pub fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

const MON: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const DAY: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];

/// `Sat Nov 14 20:51:30 +0000 2026` (lastlog's format; the clock is UTC).
pub fn full_time(t: i64, out: &mut impl Write) {
    let days = t.div_euclid(86400);
    let s = t.rem_euclid(86400);
    let (y, m, d) = civil(days);
    let _ = write!(out, "{} {} {:2} {:02}:{:02}:{:02} +0000 {}", DAY[days.rem_euclid(7) as usize],
        MON[(m - 1) as usize], d, s / 3600, (s / 60) % 60, s % 60, y);
}

pub fn now_secs() -> i64 { utmpx::now().0 }
