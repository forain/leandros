//! w - who is logged in and what they are doing (procps layout).
//!   w [-h] [user]
//! Header: time, uptime, user count, load average (/proc/uptime, /proc/loadavg).
//! Rows: USER TTY FROM LOGIN@ IDLE WHAT. IDLE is now minus the tty's atime;
//! WHAT is the cmdline of the tty's foreground process group leader
//! (/proc/<session leader>/stat field 8), else of the session leader.
//! Times are UTC.
#![no_std]
#![no_main]
extern crate leandros_libc;
#[macro_use]
#[path = "../../ps/src/common.rs"]
mod common;
#[path = "ut.rs"]
mod ut;
use common::Buf;
use core::fmt::Write;
use common::*;
use ut::*;

#[cfg(target_arch = "aarch64")] const NEWFSTATAT: usize = 79;
#[cfg(target_arch = "x86_64")] const NEWFSTATAT: usize = 262;

fn tty_atime(line: &[u8]) -> Option<i64> {
    let mut p = Buf::<64>::new();
    p.push(b"/dev/");
    p.push(line);
    p.push(b"\0");
    let mut st = [0u8; 256];
    let r = unsafe { leandros_libc::syscall::syscall4(NEWFSTATAT, -100isize as usize, p.b.as_ptr() as usize, st.as_mut_ptr() as usize, 0) };
    if r < 0 { return None; }
    Some(i64::from_le_bytes(st[72..80].try_into().unwrap()))
}

fn s<const N: usize>(b: &Buf<N>) -> &str { core::str::from_utf8(b.bytes()).unwrap_or("?") }

fn str_(b: &[u8]) -> &str { core::str::from_utf8(b).unwrap_or("0.00") }

fn what(pid: i32, out: &mut Buf<80>) {
    let mut path = Buf::<48>::new();
    let _ = write!(path, "/proc/{}/stat", pid);
    let mut st = [0u8; 512];
    let n = read_file(path.bytes(), &mut st).unwrap_or(0);
    let mut target = pid as u64;
    if n > 0 {
        // fields after the last ')': state ppid pgrp session tty tpgid
        if let Some(p) = st[..n].iter().rposition(|&c| c == b')') {
            let mut it = st[p + 1..n].split(|&c| c == b' ').filter(|s| !s.is_empty());
            if let Some(t) = it.nth(5).and_then(parse_u64) { if t > 0 && t < u32::MAX as u64 { target = t; } }
        }
    }
    for pid in [target, pid as u64] {
        let mut cp = Buf::<48>::new();
        let _ = write!(cp, "/proc/{}/cmdline", pid);
        let mut cl = [0u8; 160];
        let n = read_file(cp.bytes(), &mut cl).unwrap_or(0);
        if n > 0 {
            for c in &mut cl[..n] { if *c == 0 { *c = b' '; } }
            let mut e = n;
            while e > 0 && cl[e - 1] == b' ' { e -= 1; }
            out.push(&cl[..e.min(79)]);
            return;
        }
    }
    out.push(b"-");
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let mut header = true;
    let mut only: Option<&[u8]> = None;
    for i in 1..argc as usize {
        let a = arg(argv, i);
        match a {
            b"-h" | b"--no-header" => header = false,
            b"-s" | b"--short" | b"-f" | b"--from" => {}
            _ if a.starts_with(b"-") => { eprintln!("usage: w [-h] [user]"); return 1; }
            _ => only = Some(a),
        }
    }
    let now = now_secs();
    let mut count = 0;
    each_utmp(|r| { if r.ut_type == USER_PROCESS && r.ut_user[0] != 0 { count += 1; } });
    if header {
        let s = now.rem_euclid(86400);
        let mut up = [0u8; 64];
        let n = read_file(b"/proc/uptime", &mut up).unwrap_or(0);
        let secs = up[..n].split(|&c| c == b'.').next().and_then(parse_u64).unwrap_or(0);
        let mut la = [0u8; 64];
        let ln = read_file(b"/proc/loadavg", &mut la).unwrap_or(0);
        let mut f = la[..ln].split(|&c| c == b' ');
        let (l1, l5, l15) = (f.next().unwrap_or(b"0.00"), f.next().unwrap_or(b"0.00"), f.next().unwrap_or(b"0.00"));
        let (d, h, m) = (secs / 86400, (secs / 3600) % 24, (secs / 60) % 60);
        say!(" {:02}:{:02}:{:02} up ", s / 3600, (s / 60) % 60, s % 60);
        if d > 0 { say!("{} day{}, ", d, if d == 1 { "" } else { "s" }); }
        if secs < 3600 && d == 0 { say!("{} min", m); } else { say!("{:2}:{:02}", h, m); }
        sayln!(",  {} user{},  load average: {}, {}, {}", count, if count == 1 { "" } else { "s" },
            str_(l1), str_(l5), str_(l15));
        sayln!("USER     TTY      FROM             LOGIN@   IDLE WHAT");
    }
    each_utmp(|r| {
        if r.ut_type != USER_PROCESS || r.ut_user[0] == 0 { return; }
        let user = cstr(&r.ut_user);
        if let Some(o) = only { if o != user { return; } }
        let mut u = Buf::<8>::new(); u.push(user);
        let line = cstr(&r.ut_line);
        let mut l = Buf::<8>::new(); l.push(line);
        let host = cstr(&r.ut_host);
        let mut hb = Buf::<16>::new(); hb.push(if host.is_empty() { b"-" } else { host });
        let t = r.tv_sec % 86400;
        let mut idle = Buf::<12>::new();
        match tty_atime(line) {
            Some(a) if now > a => {
                let d = (now - a) as u64;
                if d >= 86400 { let _ = write!(idle, "{}days", d / 86400); }
                else if d >= 3600 { let _ = write!(idle, "{}:{:02}m", d / 3600, (d / 60) % 60); }
                else if d >= 60 { let _ = write!(idle, "{}:{:02}", d / 60, d % 60); }
                else { let _ = write!(idle, "{}.00s", d); }
            }
            _ => { idle.push(b"0.00s"); }
        }
        let mut w = Buf::<80>::new();
        what(r.ut_pid, &mut w);
        sayln!("{:<8} {:<8} {:<16} {:02}:{:02} {:>7} {}", s(&u), s(&l), s(&hb), t / 3600, (t / 60) % 60, s(&idle), s(&w));
    });
    0
}
