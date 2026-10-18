//! ps — report process status, read from /proc.
//!
//! ```text
//! ps                 processes on the caller's terminal with its euid: PID TTY TIME CMD
//! ps -e | -A | ax    every process
//! ps -a              every process with a terminal, minus session leaders
//! ps -d              every process except session leaders
//! ps a | x           BSD: every process with a terminal | the caller's own
//!                    processes, terminal or not (procps-ng semantics)
//! ps -f | -ef        UID PID PPID C STIME TTY TIME CMD
//! ps aux | -ax u     USER PID %CPU %MEM VSZ RSS TTY STAT START TIME COMMAND
//! ps -l              long format
//! ps -o a,b=HDR,...  chosen columns (`=` renames; all-empty headers print none)
//! ps -p 1,2 [-p 3]   only these pids (selects regardless of session)
//! ps -u|-U user,...  effective | real user (name or uid)
//! ps -g|-G grp,...   session id or effective group name | real group
//! ps -C comm,...     command name
//! ps -t tty,...      terminal: pts/N, ttyN, ttySN, console, `?` for none
//!                    (selectors, -p included, combine as a union, as procps)
//! ps -T | -L         one row per thread (/proc/<pid>/task when it exists)
//! ps --no-headers | h
//! ```
//!
//! Notes: TTY decodes stat field 7 (tty_nr) the way procps names devices:
//! pts/N (major 136..143), ttyN (4, < 64), ttySN (4, >= 64), console (5:1),
//! `?` for none. The default selection is procps's: same tty_nr and same
//! effective uid as the caller. TIME is utime+stime (stat fields 14, 15).
//! COMMAND/args is /proc/<pid>/cmdline (argv, NUL-separated), else `[comm]`
//! (kernel threads). `-T/-L` read /proc/<pid>/task/<tid>/stat per thread.
//! START and STIME are UTC. No allocator is used.

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
    uid: u32, euid: u32, gid: u32, egid: u32, tty: u32, tpgid: i64,
}

/// The first two numbers of a status "Uid:"/"Gid:" line (real, effective).
fn two_ids(r: &[u8]) -> (u32, u32) {
    let mut it = r.split(|c| !c.is_ascii_digit()).filter(|w| !w.is_empty());
    let a = it.next().map_or(0, |w| num(w) as u32);
    let b = it.next().map_or(a, |w| num(w) as u32);
    (a, b)
}

fn num(s: &[u8]) -> u64 { parse_u64(s).unwrap_or(0) }
fn snum(s: &[u8]) -> i64 {
    if let Some(r) = s.strip_prefix(b"-") { -(num(r) as i64) } else { num(s) as i64 }
}

/// `thread`: read the per-thread files under /proc/<pid>/task/<tid>.
fn load(pid: u32, tid: u32, thread: bool) -> Option<Info> {
    let mut dir = Buf::<48>::new();
    if thread { let _ = write!(dir, "/proc/{}/task/{}", pid, tid); }
    else { let _ = write!(dir, "/proc/{}", tid); }
    let mut p = Buf::<64>::new();
    p.push(dir.bytes()); p.push(b"/stat");
    let mut b = [0u8; 1024];
    let n = read_file(p.bytes(), &mut b).ok()?;
    let s = &b[..n];
    let lp = s.iter().position(|&c| c == b'(')?;
    let rp = s.iter().rposition(|&c| c == b')')?;
    let mut info = Info {
        pid, tid, comm: [0; 16], cn: 0, state: b'?', ppid: 0, pgid: 0, sid: 0,
        ut: 0, st: 0, prio: 0, nice: 0, nthr: 1, start: 0, vsize: 0, rss: 0, uid: 0,
        euid: 0, gid: 0, egid: 0, tty: 0, tpgid: -1,
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
    info.tty = num(t[4]) as u32;
    info.tpgid = snum(t[5]);
    info.ut = num(t[11]);
    info.st = num(t[12]);
    info.prio = snum(t[15]);
    info.nice = snum(t[16]);
    info.nthr = num(t[17]).max(1);
    info.start = num(t[19]);
    info.vsize = num(t[20]);
    info.rss = num(t[21]);
    // Real uid from status ("Uid:\tR\tE\tS\tF").
    let mut q = Buf::<64>::new();
    q.push(dir.bytes()); q.push(b"/status");
    let mut sb = [0u8; 1536];
    if let Ok(m) = read_file(q.bytes(), &mut sb) {
        for line in sb[..m].split(|&c| c == b'\n') {
            if let Some(r) = line.strip_prefix(b"Uid:") { (info.uid, info.euid) = two_ids(r); }
            if let Some(r) = line.strip_prefix(b"Gid:") { (info.gid, info.egid) = two_ids(r); }
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

static mut GROUP: [u8; 8192] = [0; 8192];
static mut GROUP_N: usize = 0;

/// The id of `name` in a passwd/group-format file (field 3), or `name`
/// itself when it is a number.
fn id_of(file: &[u8], name: &[u8]) -> Option<u32> {
    if let Some(v) = parse_u64(name) { return Some(v as u32); }
    for line in file.split(|&c| c == b'\n') {
        let mut f = line.split(|&c| c == b':');
        let (Some(n), Some(_), Some(id)) = (f.next(), f.next(), f.next()) else { continue };
        if n == name { return parse_u64(id).map(|v| v as u32); }
    }
    None
}

fn passwd() -> &'static [u8] {
    unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(PASSWD) as *const u8, PASSWD_N) }
}

fn group() -> &'static [u8] {
    unsafe {
        if GROUP_N == 0 {
            GROUP_N = read_file(b"/etc/group", &mut *core::ptr::addr_of_mut!(GROUP)).unwrap_or(0);
        }
        core::slice::from_raw_parts(core::ptr::addr_of!(GROUP) as *const u8, GROUP_N)
    }
}

/// `-t` names (procps's): pts/N, /dev/pts/N, ttyN, ttySN, console; `?`/`-`
/// select processes without a terminal. Returns the tty_nr.
fn tty_nr_of(name: &[u8]) -> Option<u32> {
    let name = name.strip_prefix(b"/dev/").unwrap_or(name);
    let dev = |maj: u32, min: u32| (maj << 8) | (min & 0xff) | ((min & !0xff) << 12);
    if name == b"?" || name == b"-" { return Some(0); }
    if name == b"console" { return Some(dev(5, 1)); }
    if let Some(n) = name.strip_prefix(b"pts/").and_then(parse_u64) {
        let n = n as u32;
        return Some(dev(136 + n / 256, n % 256));
    }
    if let Some(n) = name.strip_prefix(b"ttyS").and_then(parse_u64) { return Some(dev(4, 64 + n as u32)); }
    if let Some(n) = name.strip_prefix(b"tty").and_then(parse_u64) { return Some(dev(4, n as u32)); }
    None
}

/// procps selection lists (`-u -U -g -G -C -t`); a process matching any one
/// of them, or `-p`, is shown.
const NSEL: usize = 32;
struct List { v: [u32; NSEL], n: usize }
impl List {
    const fn new() -> Self { List { v: [0; NSEL], n: 0 } }
    fn add(&mut self, x: u32) { if self.n < NSEL { self.v[self.n] = x; self.n += 1; } }
    fn has(&self, x: u32) -> bool { self.v[..self.n].contains(&x) }
}
/// procps-ng's "simple selections": `-a` (a terminal, not a session
/// leader), `-d` (not a session leader), BSD `a` (lifts "only yourself":
/// every process with a terminal) and `x` (lifts "must have a terminal": the
/// caller's own processes); `ax` is every process.
struct Simple { a: bool, d: bool, bsd_a: bool, bsd_x: bool }
impl Simple {
    fn any(&self) -> bool { self.a || self.d || self.bsd_a || self.bsd_x }
    fn matches(&self, i: &Info, my_euid: u32) -> bool {
        let leader = i.sid == i.pid;
        (self.bsd_a && self.bsd_x)
            || (self.bsd_a && i.tty != 0)
            || (self.bsd_x && i.euid == my_euid)
            || (self.a && i.tty != 0 && !leader)
            || (self.d && !leader)
    }
}

struct Sel {
    euid: List, ruid: List, sid: List, egid: List, rgid: List, tty: List,
    comm: [[u8; 16]; NSEL], ncomm: usize,
}
impl Sel {
    fn any(&self) -> bool {
        self.euid.n + self.ruid.n + self.sid.n + self.egid.n + self.rgid.n + self.tty.n + self.ncomm > 0
    }
    fn matches(&self, i: &Info) -> bool {
        self.euid.has(i.euid) || self.ruid.has(i.uid) || self.sid.has(i.sid)
            || self.egid.has(i.egid) || self.rgid.has(i.gid) || self.tty.has(i.tty)
            || self.comm[..self.ncomm].iter().any(|c| {
                let n = c.iter().position(|&b| b == 0).unwrap_or(16);
                &c[..n] == &i.comm[..i.cn]
            })
    }
}

/// Parse one selector's comma list; false (with a message) on an unknown name.
fn add_sel(sel: &mut Sel, opt: u8, v: &[u8]) -> bool {
    for w in v.split(|&c| c == b',' || c == b' ') {
        if w.is_empty() { continue; }
        let s = core::str::from_utf8(w).unwrap_or("?");
        match opt {
            b'u' | b'U' => match id_of(passwd(), w) {
                Some(id) => if opt == b'u' { sel.euid.add(id) } else { sel.ruid.add(id) },
                None => { eprintln!("ps: user name does not exist: {}", s); return false; }
            },
            b'G' => match id_of(group(), w) {
                Some(id) => sel.rgid.add(id),
                None => { eprintln!("ps: group name does not exist: {}", s); return false; }
            },
            // -g: a session id when numeric, else an effective group name
            b'g' => match parse_u64(w) {
                Some(id) => sel.sid.add(id as u32),
                None => match id_of(group(), w) {
                    Some(id) => sel.egid.add(id),
                    None => { eprintln!("ps: group name does not exist: {}", s); return false; }
                },
            },
            b't' => match tty_nr_of(w) {
                Some(nr) => sel.tty.add(nr),
                None => { eprintln!("ps: TTY could not be found: {}", s); return false; }
            },
            _ /* C */ => if sel.ncomm < NSEL {
                let n = w.len().min(15);
                sel.comm[sel.ncomm] = [0; 16];
                sel.comm[sel.ncomm][..n].copy_from_slice(&w[..n]);
                sel.ncomm += 1;
            },
        }
    }
    true
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

struct Ctx { uptime: u64, now: i64, memkb: u64, caller_tty: u32, caller_euid: u32 }

/// procps's device names for a tty_nr (Linux dev_t encoding).
fn tty_name(out: &mut Buf<OUTW>, nr: u32) {
    if nr == 0 { out.push(b"?"); return; }
    let major = (nr >> 8) & 0xfff;
    let minor = (nr & 0xff) | ((nr >> 12) & 0xfff00);
    match major {
        136..=143 => { let _ = write!(out, "pts/{}", (major - 136) * 256 + minor); }
        4 if minor < 64 => { let _ = write!(out, "tty{}", minor); }
        4 => { let _ = write!(out, "ttyS{}", minor - 64); }
        5 if minor == 1 => out.push(b"console"),
        _ => { let _ = write!(out, "{},{}", major, minor); }
    }
}

const OUTW: usize = 4096;

fn fmt_time(out: &mut Buf<OUTW>, secs: u64) {
    let (d, h, m, s) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    if d > 0 { let _ = write!(out, "{}-{:02}:{:02}:{:02}", d, h, m, s); }
    else { let _ = write!(out, "{:02}:{:02}:{:02}", h, m, s); }
}

fn cell(k: K, i: &Info, cx: &Ctx, out: &mut Buf<OUTW>, cmdbuf: &mut Buf<OUTW>) {
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
        K::Tty => tty_name(out, i.tty),
        K::Stat => {
            out.push(&[i.state]);
            if i.nice < 0 { out.push(b"<"); } else if i.nice > 0 { out.push(b"N"); }
            if i.sid == i.pid { out.push(b"s"); }
            if i.nthr > 1 { out.push(b"l"); }
            if i.tty != 0 && i.tpgid == i.pgid as i64 { out.push(b"+"); }
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
    let mut line = Buf::<8192>::new();
    let mut cmdbuf = Buf::<OUTW>::new();
    let mut v = Buf::<OUTW>::new();
    for (ci, c) in cols.iter().enumerate() {
        let (_, _, w, right, k) = COLS[c.idx];
        v.clear();
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
    eprintln!("usage: ps [-e|-A|-a|-d|a|x] [-f|-l|u] [-T|-L] [-o col[=HDR],...] [-p pid,...] [--no-headers]");
    eprintln!("          [-u|-U user,...] [-g|-G group,...] [-C comm,...] [-t tty,...]");
    eprintln!("columns: pid lwp ppid pgid sid uid user tty stat s time etime start stime %cpu %mem vsz rss nlwp pri ni c comm cmd args");
    1
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let argc = argc as usize;
    let mut all = false;
    let mut simple = Simple { a: false, d: false, bsd_a: false, bsd_x: false };
    // `-x` is not a Unix option; `ps -ax` / `-aux` mean BSD `ax` / `aux` to
    // procps (absent a user named "x"), i.e. every process.
    let mut dash_x = false;
    let mut full = false;
    let mut long = false;
    let mut user_fmt = false;
    let mut threads = false;
    let mut headers = true;
    let mut cols: [Col; MAXC] = core::array::from_fn(|_| Col { idx: 0, hdr: [0; 24], hn: 0, renamed: false });
    let mut nc = 0usize;
    let mut sel = [0u32; 64];
    let mut nsel = 0usize;
    let mut sels = Sel {
        euid: List::new(), ruid: List::new(), sid: List::new(), egid: List::new(),
        rgid: List::new(), tty: List::new(), comm: [[0; 16]; NSEL], ncomm: 0,
    };
    load_passwd();

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
                b'e' | b'A' => all = true,
                b'a' if dash => simple.a = true,
                b'd' if dash => simple.d = true,
                b'a' => simple.bsd_a = true,
                b'x' => { simple.bsd_x = true; if dash { dash_x = true; } }
                b'f' => full = true,
                b'l' => long = true,
                b'u' if !dash => user_fmt = true,
                b'T' | b'L' => threads = true,
                b'h' if !dash => headers = false,
                b'w' | b'n' | b'r' | b'c' | b'm' | b'H' | b'j' => {}
                b'o' | b'p' | b'U' | b'g' | b'G' | b'C' | b't' => {
                    let rest = &letters[j..];
                    j = letters.len();
                    let v: &[u8] = if !rest.is_empty() { rest } else if i < argc { i += 1; arg(argv, i - 1) } else { return usage() };
                    if c == b'o' { if !add_cols(&mut cols, &mut nc, v) { return 1; } }
                    else if c == b'p' { if !add_pids(&mut sel, &mut nsel, v) { return usage(); } }
                    else if !dash && c != b't' { eprintln!("ps: unsupported option '{}'", c as char); return usage(); }
                    else if !add_sel(&mut sels, c, v) { return 1; }
                }
                // -u user: procps's effective-user selector (BSD `u` is the format)
                b'u' => {
                    let rest = &letters[j..];
                    j = letters.len();
                    let v: &[u8] = if !rest.is_empty() { rest } else if i < argc { i += 1; arg(argv, i - 1) } else { return usage() };
                    if !add_sel(&mut sels, b'u', v) { return 1; }
                }
                _ => { eprintln!("ps: unsupported option '{}'", c as char); return usage(); }
            }
        }
    }

    if dash_x && simple.a { all = true; }

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

    let me = leandros_libc::getpid() as u32;
    let caller = load(me, me, false);
    let cx = Ctx {
        uptime: read_secs100(b"/proc/uptime"),
        now: now_secs(),
        memkb: mem_total_kb(),
        caller_tty: caller.as_ref().map_or(0, |x| x.tty),
        caller_euid: caller.as_ref().map_or(0, |x| x.euid),
    };

    let pids = &mut *core::ptr::addr_of_mut!(PIDS);
    let np = if nsel > 0 && !sels.any() {
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
            let Some(info) = load(pid, tids[t], threads && nt > 1 || tids[t] != pid) else { continue };
            // The simple selections (-e -a -d, BSD a/x) and the lists combine
            // as a union, as procps's; with none of them the default is the
            // caller's terminal and effective uid.
            let simple_hit = all || simple.matches(&info, cx.caller_euid);
            if sels.any() || nsel > 0 {
                if !simple_hit && !sel[..nsel].contains(&pid) && !sels.matches(&info) { continue; }
            } else if simple.any() || all {
                if !simple_hit { continue; }
            } else if info.tty != cx.caller_tty || info.euid != cx.caller_euid { continue; }
            print_row(&cols[..nc], &info, &cx);
            shown += 1;
        }
    }
    if shown == 0 { 1 } else { 0 }
}
