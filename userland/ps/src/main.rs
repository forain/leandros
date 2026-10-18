//! ps — report process status, read from /proc.
//!
//! ```text
//! ps                 processes of the caller's session: PID TTY TIME CMD
//! ps -e | -A | ax    every process
//! ps -f | -ef        UID PID PPID C STIME TTY TIME CMD
//! ps aux | -ax u     USER PID %CPU %MEM VSZ RSS TTY STAT START TIME COMMAND
//! ps -l              long format
//! ps -o a,b=HDR,...  chosen columns (`=` renames; all-empty headers print none)
//! ps -p 1,2 [-p 3]   only these pids (selects regardless of session)
//! ps -T | -L         one row per thread (/proc/<pid>/task when it exists)
//! ps --no-headers | h
//! ```
//!
//! Notes: the kernel reports tty_nr 0 for every process, so TTY is `?`, and
//! "the current terminal" is approximated by "the caller's session". TIME is
//! utime+stime (stat fields 14, 15). COMMAND is /proc/<pid>/cmdline (this
//! kernel keeps only the executable path, not argv), else `[comm]`. START and
//! STIME are UTC. No allocator is used.

#![no_std]
#![no_main]

extern crate leandros_libc;

#[macro_use]
#[path = "common.rs"]
mod common;

use common::*;
use core::fmt::Write;

const MAXP: usize = 4096;
const MAXC: usize = 24;
static mut PIDS: [u32; MAXP] = [0; MAXP];
static mut PASSWD: [u8; 16384] = [0; 16384];
static mut PASSWD_N: usize = 0;

// ── column table ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum K {
    Pid, Lwp, Ppid, Pgid, Sid, Uid, User, Tty, Stat, S, Time, Etime, Start, Stime,
    Cpu, Mem, Vsz, Rss, Nlwp, Pri, Ni, C, Comm, Cmd, Args,
}

// (names, header, width, right-aligned, kind)
const COLS: &[(&[&str], &str, usize, bool, K)] = &[
    (&["pid", "tgid"], "PID", 7, true, K::Pid),
    (&["lwp", "tid", "spid"], "LWP", 7, true, K::Lwp),
    (&["ppid"], "PPID", 7, true, K::Ppid),
    (&["pgid", "pgrp"], "PGID", 7, true, K::Pgid),
    (&["sid", "sess", "session"], "SID", 7, true, K::Sid),
    (&["uid", "euid"], "UID", 5, true, K::Uid),
    (&["user", "uname", "euser"], "USER", 8, false, K::User),
    (&["tty", "tt", "tname"], "TTY", 8, false, K::Tty),
    (&["stat"], "STAT", 4, false, K::Stat),
    (&["s", "state"], "S", 1, false, K::S),
    (&["time", "cputime"], "TIME", 8, true, K::Time),
    (&["etime"], "ELAPSED", 11, true, K::Etime),
    (&["start", "start_time", "lstart"], "START", 5, true, K::Start),
    (&["stime"], "STIME", 5, true, K::Stime),
    (&["%cpu", "pcpu", "cpu"], "%CPU", 4, true, K::Cpu),
    (&["%mem", "pmem"], "%MEM", 4, true, K::Mem),
    (&["vsz", "vsize"], "VSZ", 7, true, K::Vsz),
    (&["rss", "rsz"], "RSS", 6, true, K::Rss),
    (&["nlwp", "thcount"], "NLWP", 4, true, K::Nlwp),
    (&["pri"], "PRI", 3, true, K::Pri),
    (&["ni", "nice"], "NI", 3, true, K::Ni),
    (&["c"], "C", 2, true, K::C),
    (&["comm", "ucmd", "ucomm"], "COMMAND", 15, false, K::Comm),
    (&["cmd"], "CMD", 15, false, K::Cmd),
    (&["args", "command"], "COMMAND", 15, false, K::Args),
];

struct Col { idx: usize, hdr: [u8; 24], hn: usize, renamed: bool }

fn find_col(name: &[u8]) -> Option<usize> {
    COLS.iter().position(|c| c.0.iter().any(|n| n.as_bytes().eq_ignore_ascii_case(name)))
}

fn add_cols(cols: &mut [Col; MAXC], nc: &mut usize, spec: &[u8]) -> bool {
    for item in spec.split(|&c| c == b',' || c == b' ') {
        if item.is_empty() { continue; }
        let (name, hdr) = match item.iter().position(|&c| c == b'=') {
            Some(i) => (&item[..i], Some(&item[i + 1..])),
            None => (item, None),
        };
        let Some(idx) = find_col(name) else {
            eprintln!("ps: error: unknown user-defined format specifier \"{}\"", core::str::from_utf8(name).unwrap_or("?"));
            return false;
        };
        if *nc >= MAXC { return false; }
        let mut c = Col { idx, hdr: [0; 24], hn: 0, renamed: false };
        let h: &[u8] = match hdr { Some(h) => { c.renamed = true; h } None => COLS[idx].1.as_bytes() };
        c.hn = h.len().min(24);
        c.hdr[..c.hn].copy_from_slice(&h[..c.hn]);
        cols[*nc] = c;
        *nc += 1;
    }
    true
}

// ── process data ───────────────────────────────────────────────────────────

struct Info {
    pid: u32, tid: u32,
    comm: [u8; 16], cn: usize,
    state: u8, ppid: u32, pgid: u32, sid: u32,
    ut: u64, st: u64, prio: i64, nice: i64, nthr: u64, start: u64, vsize: u64, rss: u64,
    uid: u32,
}

fn num(s: &[u8]) -> u64 { parse_u64(s).unwrap_or(0) }
fn snum(s: &[u8]) -> i64 {
    if let Some(r) = s.strip_prefix(b"-") { -(num(r) as i64) } else { num(s) as i64 }
}

fn load(pid: u32, tid: u32) -> Option<Info> {
    let mut p = Buf::<48>::new();
    let _ = write!(p, "/proc/{}/stat", tid);
    let mut b = [0u8; 1024];
    let n = read_file(p.bytes(), &mut b).ok()?;
    let s = &b[..n];
    let lp = s.iter().position(|&c| c == b'(')?;
    let rp = s.iter().rposition(|&c| c == b')')?;
    let mut info = Info {
        pid, tid, comm: [0; 16], cn: 0, state: b'?', ppid: 0, pgid: 0, sid: 0,
        ut: 0, st: 0, prio: 0, nice: 0, nthr: 1, start: 0, vsize: 0, rss: 0, uid: 0,
    };
    let c = &s[lp + 1..rp];
    info.cn = c.len().min(15);
    info.comm[..info.cn].copy_from_slice(&c[..info.cn]);
    let mut t: [&[u8]; 24] = [&[]; 24];
    let mut nt = 0;
    for w in s[rp + 1..].split(|&c| c == b' ' || c == b'\n') {
        if w.is_empty() { continue; }
        if nt < 24 { t[nt] = w; nt += 1; }
    }
    if nt < 22 { return None; }
    info.state = t[0][0];
    info.ppid = num(t[1]) as u32;
    info.pgid = num(t[2]) as u32;
    info.sid = num(t[3]) as u32;
    info.ut = num(t[11]);
    info.st = num(t[12]);
    info.prio = snum(t[15]);
    info.nice = snum(t[16]);
    info.nthr = num(t[17]).max(1);
    info.start = num(t[19]);
    info.vsize = num(t[20]);
    info.rss = num(t[21]);
    // Real uid from status ("Uid:\tR\tE\tS\tF").
    let mut q = Buf::<48>::new();
    let _ = write!(q, "/proc/{}/status", tid);
    let mut sb = [0u8; 1536];
    if let Ok(m) = read_file(q.bytes(), &mut sb) {
        for line in sb[..m].split(|&c| c == b'\n') {
            if let Some(r) = line.strip_prefix(b"Uid:") {
                let r = r.iter().position(|c| c.is_ascii_digit()).map(|i| &r[i..]).unwrap_or(&[]);
                let e = r.iter().position(|c| !c.is_ascii_digit()).unwrap_or(r.len());
                info.uid = num(&r[..e]) as u32;
            }
        }
    }
    Some(info)
}

fn load_passwd() {
    unsafe {
        let b = &mut *core::ptr::addr_of_mut!(PASSWD);
        PASSWD_N = read_file(b"/etc/passwd", b).unwrap_or(0);
    }
}

fn user_name(uid: u32) -> Option<&'static [u8]> {
    let b: &'static [u8] = unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(PASSWD) as *const u8, PASSWD_N) };
    for line in b.split(|&c| c == b'\n') {
        let mut f = line.split(|&c| c == b':');
        let (Some(name), Some(_), Some(id)) = (f.next(), f.next(), f.next()) else { continue };
        if parse_u64(id) == Some(uid as u64) { return Some(name); }
    }
    None
}

fn read_secs100(path: &[u8]) -> u64 {
    // First number of /proc/uptime ("123.45 ...") in 1/100 s.
    let mut b = [0u8; 64];
    let n = read_file(path, &mut b).unwrap_or(0);
    let s = &b[..n];
    let e = s.iter().position(|&c| c == b' ' || c == b'\n').unwrap_or(n);
    let s = &s[..e];
    match s.iter().position(|&c| c == b'.') {
        Some(d) => {
            let mut frac = [b'0'; 2];
            for (i, &c) in s[d + 1..].iter().take(2).enumerate() { frac[i] = c; }
            num(&s[..d]) * 100 + num(&frac)
        }
        None => num(s) * 100,
    }
}

fn mem_total_kb() -> u64 {
    let mut b = [0u8; 512];
    let n = read_file(b"/proc/meminfo", &mut b).unwrap_or(0);
    for line in b[..n].split(|&c| c == b'\n') {
        if let Some(r) = line.strip_prefix(b"MemTotal:") {
            let r: &[u8] = r.iter().position(|c| c.is_ascii_digit()).map(|i| &r[i..]).unwrap_or(&[]);
            let e = r.iter().position(|c| !c.is_ascii_digit()).unwrap_or(r.len());
            return num(&r[..e]);
        }
    }
    0
}

fn now_secs() -> i64 {
    let mut ts = leandros_libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { leandros_libc::clock_gettime(leandros_libc::CLOCK_REALTIME, &mut ts); }
    ts.tv_sec
}

fn civil(days: i64) -> (i64, u32, u32) {
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

struct Ctx { uptime: u64, now: i64, memkb: u64, caller_sid: u32 }

fn fmt_time(out: &mut Buf<64>, secs: u64) {
    let (d, h, m, s) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    if d > 0 { let _ = write!(out, "{}-{:02}:{:02}:{:02}", d, h, m, s); }
    else { let _ = write!(out, "{:02}:{:02}:{:02}", h, m, s); }
}

fn cell(k: K, i: &Info, cx: &Ctx, out: &mut Buf<64>, cmdbuf: &mut Buf<512>) {
    let cpu = i.ut + i.st;
    let elapsed = cx.uptime.saturating_sub(i.start).max(1);
    match k {
        K::Pid => { let _ = write!(out, "{}", i.pid); }
        K::Lwp => { let _ = write!(out, "{}", i.tid); }
        K::Ppid => { let _ = write!(out, "{}", i.ppid); }
        K::Pgid => { let _ = write!(out, "{}", i.pgid); }
        K::Sid => { let _ = write!(out, "{}", i.sid); }
        K::Uid => { let _ = write!(out, "{}", i.uid); }
        K::User => match user_name(i.uid) {
            Some(n) if n.len() > 8 => { out.push(&n[..7]); out.push(b"+"); }
            Some(n) => out.push(n),
            None => { let _ = write!(out, "{}", i.uid); }
        },
        K::Tty => out.push(b"?"),
        K::Stat => {
            out.push(&[i.state]);
            if i.nice < 0 { out.push(b"<"); } else if i.nice > 0 { out.push(b"N"); }
            if i.sid == i.pid { out.push(b"s"); }
            if i.nthr > 1 { out.push(b"l"); }
        }
        K::S => out.push(&[i.state]),
        K::Time => fmt_time(out, cpu / 100),
        K::Etime => {
            let s = elapsed / 100;
            let (d, h, m, sec) = (s / 86400, s / 3600 % 24, s / 60 % 60, s % 60);
            if d > 0 { let _ = write!(out, "{}-{:02}:{:02}:{:02}", d, h, m, sec); }
            else if h > 0 { let _ = write!(out, "{:02}:{:02}:{:02}", h, m, sec); }
            else { let _ = write!(out, "{:02}:{:02}", m, sec); }
        }
        K::Start | K::Stime => {
            let t = cx.now - (cx.uptime as i64 / 100) + (i.start as i64 / 100);
            let age = cx.now - t;
            let (days, rem) = (t.div_euclid(86400), t.rem_euclid(86400));
            if age < 86400 {
                let _ = write!(out, "{:02}:{:02}", rem / 3600, rem / 60 % 60);
            } else {
                const MON: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
                let (_, m, d) = civil(days);
                let _ = write!(out, "{}{:02}", MON[(m - 1) as usize % 12], d);
            }
        }
        K::Cpu => {
            let p10 = cpu * 1000 / elapsed;
            let _ = write!(out, "{}.{}", p10 / 10, p10 % 10);
        }
        K::Mem => {
            let p10 = if cx.memkb > 0 { i.rss * 4 * 1000 / cx.memkb } else { 0 };
            let _ = write!(out, "{}.{}", p10 / 10, p10 % 10);
        }
        K::Vsz => { let _ = write!(out, "{}", i.vsize / 1024); }
        K::Rss => { let _ = write!(out, "{}", i.rss * 4); }
        K::Nlwp => { let _ = write!(out, "{}", i.nthr); }
        K::Pri => { let _ = write!(out, "{}", i.prio); }
        K::Ni => { let _ = write!(out, "{}", i.nice); }
        K::C => { let _ = write!(out, "{}", (cpu * 100 / elapsed).min(99)); }
        K::Comm => out.push(&i.comm[..i.cn]),
        K::Cmd | K::Args => {
            cmdbuf.clear();
            let mut p = Buf::<48>::new();
            let _ = write!(p, "/proc/{}/cmdline", i.pid);
            let n = read_file(p.bytes(), &mut cmdbuf.b).unwrap_or(0);
            let mut n = n;
            while n > 0 && cmdbuf.b[n - 1] == 0 { n -= 1; }
            if n == 0 {
                if i.comm[0] == b'[' {
                    out.push(&i.comm[..i.cn]);
                } else {
                    out.push(b"[");
                    out.push(&i.comm[..i.cn]);
                    out.push(b"]");
                }
            } else {
                for c in cmdbuf.b[..n].iter_mut() { if *c == 0 { *c = b' '; } }
                out.push(&cmdbuf.b[..n]);
            }
        }
    }
}

fn print_row(cols: &[Col], i: &Info, cx: &Ctx) {
    let mut line = Buf::<768>::new();
    let mut cmdbuf = Buf::<512>::new();
    for (ci, c) in cols.iter().enumerate() {
        let (_, _, w, right, k) = COLS[c.idx];
        let mut v = Buf::<64>::new();
        cell(k, i, cx, &mut v, &mut cmdbuf);
        let last = ci + 1 == cols.len();
        if ci > 0 { line.push(b" "); }
        let vl = v.n;
        if right { for _ in vl..w { line.push(b" "); } }
        line.push(v.bytes());
        if !right && !last { for _ in vl..w { line.push(b" "); } }
    }
    line.push(b"\n");
    write_all(1, line.bytes());
}

fn print_header(cols: &[Col]) {
    if cols.iter().all(|c| c.renamed && c.hn == 0) { return; }
    let mut line = Buf::<768>::new();
    for (ci, c) in cols.iter().enumerate() {
        let (_, _, w, right, _) = COLS[c.idx];
        let last = ci + 1 == cols.len();
        if ci > 0 { line.push(b" "); }
        if right { for _ in c.hn..w { line.push(b" "); } }
        line.push(&c.hdr[..c.hn]);
        if !right && !last { for _ in c.hn..w { line.push(b" "); } }
    }
    // procps trims trailing blanks of the header line
    while line.n > 0 && line.b[line.n - 1] == b' ' { line.n -= 1; }
    line.push(b"\n");
    write_all(1, line.bytes());
}

// ── enumeration ────────────────────────────────────────────────────────────

fn list_dir_numbers(path: &[u8], out: &mut [u32]) -> usize {
    let Ok(fd) = sys_open(path, O_RDONLY | O_DIRECTORY) else { return 0 };
    let mut buf = [0u8; 4096];
    let mut n = 0;
    loop {
        let got = match sys_getdents(fd, &mut buf) { Ok(0) | Err(_) => break, Ok(k) => k };
        let mut off = 0;
        while off + 19 <= got {
            let reclen = u16::from_le_bytes([buf[off + 16], buf[off + 17]]) as usize;
            if reclen == 0 { break; }
            let name = &buf[off + 19..off + reclen];
            let e = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            if let Some(v) = parse_u64(&name[..e]) {
                if n < out.len() { out[n] = v as u32; n += 1; }
            }
            off += reclen;
        }
    }
    sys_close(fd);
    n
}

fn usage() -> i32 {
    eprintln!("usage: ps [-e|-A|-a|-x] [-f|-l|u] [-T|-L] [-o col[=HDR],...] [-p pid,...] [--no-headers]");
    eprintln!("columns: pid lwp ppid pgid sid uid user tty stat s time etime start stime %cpu %mem vsz rss nlwp pri ni c comm cmd args");
    1
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let argc = argc as usize;
    let mut all = false;
    let mut full = false;
    let mut long = false;
    let mut user_fmt = false;
    let mut threads = false;
    let mut headers = true;
    let mut cols: [Col; MAXC] = core::array::from_fn(|_| Col { idx: 0, hdr: [0; 24], hn: 0, renamed: false });
    let mut nc = 0usize;
    let mut sel = [0u32; 64];
    let mut nsel = 0usize;

    fn add_pids(sel: &mut [u32; 64], nsel: &mut usize, s: &[u8]) -> bool {
        for w in s.split(|&c| c == b',' || c == b' ') {
            if w.is_empty() { continue; }
            let Some(v) = parse_u64(w) else { return false };
            if *nsel < 64 { sel[*nsel] = v as u32; *nsel += 1; }
        }
        true
    }

    let mut i = 1;
    while i < argc {
        let a = arg(argv, i);
        i += 1;
        if a == b"--no-headers" || a == b"--noheadings" { headers = false; continue; }
        if a == b"--help" { return usage(); }
        if a.starts_with(b"--") { eprintln!("ps: unknown option {}", core::str::from_utf8(a).unwrap_or("?")); return usage(); }
        let (dash, letters) = match a.strip_prefix(b"-") { Some(r) => (true, r), None => (false, a) };
        if !dash && parse_u64(a).is_some() {
            if !add_pids(&mut sel, &mut nsel, a) { return usage(); }
            continue;
        }
        let mut j = 0;
        while j < letters.len() {
            let c = letters[j];
            j += 1;
            match c {
                b'e' | b'A' | b'a' | b'x' => all = true,
                b'f' => full = true,
                b'l' => long = true,
                b'u' if !dash => user_fmt = true,
                b'T' | b'L' => threads = true,
                b'h' if !dash => headers = false,
                b'w' | b'n' | b'r' | b'c' | b'm' | b'H' | b'j' => {}
                b'o' | b'p' => {
                    let rest = &letters[j..];
                    j = letters.len();
                    let v: &[u8] = if !rest.is_empty() { rest } else if i < argc { i += 1; arg(argv, i - 1) } else { return usage() };
                    if c == b'o' { if !add_cols(&mut cols, &mut nc, v) { return 1; } }
                    else if !add_pids(&mut sel, &mut nsel, v) { return usage(); }
                }
                _ => { eprintln!("ps: unsupported option '{}'", c as char); return usage(); }
            }
        }
    }

    if nc == 0 {
        let spec: &[u8] = if user_fmt {
            b"user,pid,%cpu,%mem,vsz,rss,tty,stat,start,time,args=COMMAND"
        } else if full {
            if threads { b"user=UID,pid,lwp=LWP,ppid,nlwp=NLWP,c,stime,tty,time,args=CMD" }
            else { b"user=UID,pid,ppid,c,stime,tty,time,args=CMD" }
        } else if long {
            b"s,uid,pid,ppid,c,pri,ni,vsz,tty,time,comm=CMD"
        } else if threads {
            b"pid,lwp=SPID,tty,time,comm=CMD"
        } else {
            b"pid,tty,time,comm=CMD"
        };
        add_cols(&mut cols, &mut nc, spec);
    }

    load_passwd();
    let me = leandros_libc::getpid() as u32;
    let caller_sid = load(me, me).map(|x| x.sid).unwrap_or(0);
    let cx = Ctx {
        uptime: read_secs100(b"/proc/uptime"),
        now: now_secs(),
        memkb: mem_total_kb(),
        caller_sid,
    };

    let pids = &mut *core::ptr::addr_of_mut!(PIDS);
    let np = if nsel > 0 {
        let mut n = 0;
        for k in 0..nsel { pids[n] = sel[k]; n += 1; }
        n
    } else {
        list_dir_numbers(b"/proc", pids)
    };
    pids[..np].sort_unstable();

    if headers { print_header(&cols[..nc]); }
    let mut shown = 0;
    let mut last = u32::MAX;
    for k in 0..np {
        let pid = pids[k];
        if pid == last { continue; }
        last = pid;
        let mut tids = [pid; 256];
        let mut nt = 1;
        if threads {
            let mut p = Buf::<48>::new();
            let _ = write!(p, "/proc/{}/task", pid);
            let n = list_dir_numbers(p.bytes(), &mut tids);
            if n > 0 { nt = n; tids[..n].sort_unstable(); } else { tids[0] = pid; }
        }
        for t in 0..nt {
            let Some(info) = load(pid, tids[t]) else { continue };
            if nsel == 0 && !all && info.sid != cx.caller_sid { continue; }
            print_row(&cols[..nc], &info, &cx);
            shown += 1;
        }
    }
    if shown == 0 { 1 } else { 0 }
}
