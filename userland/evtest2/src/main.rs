//! evtest2 — K4 rung R2: the evdev virtio-tablet (/dev/input/event1).
//!
//! Verifies the absolute-pointer surface libinput needs: device name, EV_ABS
//! capability, input_absinfo (0..32767), no INPUT_PROP_DIRECT (so it classifies
//! as a pointer, not a touchscreen), EVIOCSCLOCKID, and that epoll stays idle
//! (no spurious wakeups) when the pointer is still. If pointer motion is
//! injected during the wait window, it also validates ABS_X/ABS_Y + SYN_REPORT
//! frames with monotonic timestamps.
//!
//! Prints "<name>: PASS"/"<name>: FAIL"; returns the failure count.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;

type c_int = i32;
type c_uint = u32;
type c_ulong = u64;
type size_t = usize;

const O_RDONLY: c_int = 0;
const O_NONBLOCK: c_int = 0o4000;

const EPOLL_CTL_ADD: c_int = 1;
const EPOLLIN: c_uint = 0x001;

// evdev event codes
const EV_SYN: u16 = 0;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const REL_HWHEEL: u16 = 6;
const REL_WHEEL: u16 = 8;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const SYN_REPORT: u16 = 0;

// ioctl request codes ('E' = 0x45). _IOC(dir,type,nr,size).
const EVIOCGNAME_64: c_ulong = 0x80404506;    // _IOC(R,'E',0x06,64)
const EVIOCGID_8: c_ulong = 0x80084502;       // _IOR('E',0x02,input_id=8)
const EVIOCGPROP_8:  c_ulong = 0x80084509;    // _IOC(R,'E',0x09,8)
const EVIOCGBIT0_8:  c_ulong = 0x80084520;    // _IOC(R,'E',0x20+0,8)
const EVIOCGBIT_REL_8: c_ulong = 0x80084522;  // _IOC(R,'E',0x20+2,8)
const EVIOCGBIT_ABS_8: c_ulong = 0x80084523;  // _IOC(R,'E',0x20+3,8)
const EVIOCGABS_X: c_ulong = 0x80184540;      // _IOR('E',0x40,input_absinfo=24)
const EVIOCGABS_Y: c_ulong = 0x80184541;
const EVIOCGKEY_96: c_ulong = 0x80604518;     // _IOC(R,'E',0x18,96)
const EVIOCSCLOCKID: c_ulong = 0x400445a0;    // _IOW('E',0xa0,int)

const CLOCK_MONOTONIC: c_int = 1;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct input_absinfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct input_event {
    tv_sec: i64,
    tv_usec: i64,
    type_: u16,
    code: u16,
    value: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union epoll_data {
    pub ptr: *mut c_void,
    pub fd: c_int,
    pub u32_: c_uint,
    pub u64_: u64,
}

#[cfg(target_arch = "x86_64")]
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct epoll_event {
    pub events: c_uint,
    pub data: epoll_data,
}
#[cfg(not(target_arch = "x86_64"))]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct epoll_event {
    pub events: c_uint,
    pub data: epoll_data,
}

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;
    pub fn puts(s: *const u8) -> i32;
    pub fn write(fd: c_int, buf: *const c_void, count: size_t) -> isize;
    pub fn exit(status: c_int) -> !;
    pub fn open(path: *const u8, oflag: c_int, ...) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn read(fd: c_int, buf: *mut c_void, count: size_t) -> isize;
    pub fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    pub fn epoll_create1(flags: c_int) -> c_int;
    pub fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut epoll_event) -> c_int;
    pub fn epoll_wait(epfd: c_int, events: *mut epoll_event, maxevents: c_int, timeout: c_int) -> c_int;
    pub fn getpid() -> c_int;
    pub fn poll(fds: *mut pollfd, nfds: u64, timeout: c_int) -> c_int;
    pub fn __errno_location() -> *mut c_int;
    pub fn clock_gettime(clk: c_int, ts: *mut timespec) -> c_int;
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct pollfd { fd: c_int, events: i16, revents: i16 }

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct timespec { tv_sec: i64, tv_nsec: i64 }

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start", ".global _start", "_start:",
    "   xor rbp, rbp", "   mov rdi, rsp", "   mov rsi, offset ev_main",
    "   and rsp, -16", "   call relibc_start_v1", "   ud2"
);
#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".section .text._start", ".global _start", "_start:",
    "   mov x29, #0", "   mov x30, #0", "   mov x0, sp",
    "   adrp x1, ev_main", "   add x1, x1, :lo12:ev_main",
    "   and sp, x0, #-16", "   bl relibc_start_v1", "   brk #0"
);

/// `name=<decimal>` on one line. Used to publish the raw timestamp-resolution
/// counts alongside the PASS/FAIL lines, so a run that fails the sub-tick check
/// says by how much rather than just "false".
fn report_num(name: &[u8], v: u64) {
    let mut buf = [0u8; 24];
    let mut i = buf.len();
    let mut n = v;
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 { break; }
    }
    unsafe {
        write(1, name.as_ptr() as *const c_void, name.len());
        write(1, b"=".as_ptr() as *const c_void, 1);
        write(1, buf.as_ptr().add(i) as *const c_void, buf.len() - i);
        write(1, b"\n".as_ptr() as *const c_void, 1);
    }
}

fn report(name: &[u8], ok: bool) -> bool {
    unsafe {
        write(1, name.as_ptr() as *const c_void, name.len());
        write(1, if ok { b": PASS\n".as_ptr() } else { b": FAIL\n".as_ptr() } as *const c_void, 7);
    }
    ok
}

// ── `evtest2 herd [window_ms]`: no thundering herd across devices ───────────
//
// A waiter parked in epoll_wait on the keyboard (event0) must not be woken by
// events on the tablet (event1): the evdev producer wakes by the device's
// exact poll key, not the whole poll channel. The host
// (`run-leandros/evherdtest.py`) injects tablet motion through QMP for the
// whole window. The waiter counts its own poll-channel wakes from the last
// column of its `/proc/sysstat` `w` line (`Task::poll_wakes`), and the
// tablet frames that arrived prove the injection happened.

// /proc/sysstat passes 256 KiB on a long-lived system.
static mut SYSSTAT: [u8; 4 << 20] = [0; 4 << 20];

fn parse_u64(b: &[u8]) -> Option<u64> {
    if b.is_empty() { return None; }
    let mut v = 0u64;
    for &c in b {
        if !c.is_ascii_digit() { return None; }
        v = v * 10 + (c - b'0') as u64;
    }
    Some(v)
}

/// `poll_wakes` of task `pid` (last field of its `w` line), or None.
unsafe fn poll_wakes_of(pid: u64) -> Option<u64> {
    let fd = open(b"/proc/sysstat\0".as_ptr(), O_RDONLY);
    if fd < 0 { return None; }
    let buf = &mut *core::ptr::addr_of_mut!(SYSSTAT);
    let mut len = 0usize;
    while len < buf.len() {
        let n = read(fd, buf.as_mut_ptr().add(len) as *mut c_void, buf.len() - len);
        if n <= 0 { break; }
        len += n as usize;
    }
    close(fd);
    for line in buf[..len].split(|&c| c == b'\n') {
        let mut f = line.split(|&c| c == b' ').filter(|x| !x.is_empty());
        if f.next() != Some(&b"w"[..]) { continue; }
        if f.next().and_then(parse_u64) != Some(pid) { continue; }
        // pid consumed: pwakes is the 8th field left (futwhy appends keyed fields).
        return f.nth(7).and_then(parse_u64);
    }
    None
}

/// Read `fd` dry; returns (events, SYN_REPORT frames).
unsafe fn drain(fd: c_int) -> (u64, u64) {
    let (mut n_ev, mut n_syn) = (0u64, 0u64);
    let mut buf = [0u8; 24 * 64];
    loop {
        let n = read(fd, buf.as_mut_ptr() as *mut c_void, buf.len());
        if n <= 0 { break; }
        for i in 0..(n as usize) / 24 {
            let e = core::ptr::read_unaligned(buf.as_ptr().add(i * 24) as *const input_event);
            n_ev += 1;
            if e.type_ == EV_SYN && e.code == SYN_REPORT { n_syn += 1; }
        }
    }
    (n_ev, n_syn)
}

unsafe fn herd_main(window_ms: c_int) -> i32 {
    let kfd = open(b"/dev/input/event0\0".as_ptr(), O_RDONLY | O_NONBLOCK);
    let tfd = open(b"/dev/input/event1\0".as_ptr(), O_RDONLY | O_NONBLOCK);
    if kfd < 0 || tfd < 0 {
        report(b"herd_open", false);
        return 1;
    }
    drain(kfd);
    drain(tfd);
    let epfd = epoll_create1(0);
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: kfd } };
    epoll_ctl(epfd, EPOLL_CTL_ADD, kfd, &mut ev as *mut _);
    let pid = getpid() as u64;
    puts(b"evtest2 herd: parked on event0; inject on event1 now\0".as_ptr());
    let w0 = poll_wakes_of(pid);
    let mut evs = [epoll_event { events: 0, data: epoll_data { u64_: 0 } }; 8];
    let rc = epoll_wait(epfd, evs.as_mut_ptr(), 8, window_ms);
    let w1 = poll_wakes_of(pid);
    let (t_ev, t_syn) = drain(tfd);
    let wakes = match (w0, w1) { (Some(a), Some(b)) => b.wrapping_sub(a), _ => u64::MAX };
    report_num(b"herd_kbd_rc", rc as u64);
    report_num(b"herd_kbd_wakes", wakes);
    report_num(b"herd_tablet_events", t_ev);
    report_num(b"herd_tablet_frames", t_syn);
    let mut failures = 0;
    if !report(b"herd_sysstat_pwakes", w0.is_some() && w1.is_some()) { failures += 1; }
    // Nothing touched the keyboard, so the wait times out.
    if !report(b"herd_kbd_quiet", rc == 0) { failures += 1; }
    if !report(b"herd_tablet_injected", t_syn >= 10) { failures += 1; }
    // The timeout itself and a stray broadcast may wake it; one wake per
    // tablet frame (the herd) may not.
    if !report(b"herd_no_cross_device_wake", wakes <= 3) { failures += 1; }
    close(epfd);
    close(kfd);
    close(tfd);
    failures
}

/// `evtest2 clock`: EVIOCSCLOCKID must change the clock events are stamped in.
/// For each of CLOCK_REALTIME / MONOTONIC / BOOTTIME: select it, wait for an
/// injected frame, and compare the first event's stamp with clock_gettime() of
/// that same clock taken right after the read. Host injection is required.
unsafe fn clock_main() -> i32 {
    let fd = open(b"/dev/input/event1\0".as_ptr(), O_RDONLY | O_NONBLOCK);
    if fd < 0 { report(b"clock_open", false); return 1; }
    let epfd = epoll_create1(0);
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd } };
    epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &mut ev as *mut _);
    let mut evs = [epoll_event { events: 0, data: epoll_data { u64_: 0 } }; 8];
    let mut failures = 0;
    let clocks: [(c_int, &[u8], &[u8]); 3] = [
        (0, b"clock_realtime_stamp\0", b"clock_realtime_diff_us"),
        (1, b"clock_monotonic_stamp\0", b"clock_monotonic_diff_us"),
        (7, b"clock_boottime_stamp\0", b"clock_boottime_diff_us"),
    ];
    for (id, name, dname) in clocks.iter() {
        let clk = *id;
        if ioctl(fd, EVIOCSCLOCKID, &clk as *const _) != 0 {
            report(b"clock_set", false); failures += 1; continue;
        }
        drain(fd);
        let rc = epoll_wait(epfd, evs.as_mut_ptr(), 8, 3000);
        let mut buf = [0u8; 24];
        let n = if rc > 0 { read(fd, buf.as_mut_ptr() as *mut c_void, 24) } else { -1 };
        let mut now = timespec::default();
        clock_gettime(clk, &mut now);
        let name = &name[..name.len() - 1];
        if n != 24 { report(name, false); failures += 1; continue; }
        let e = core::ptr::read_unaligned(buf.as_ptr() as *const input_event);
        let st = e.tv_sec * 1_000_000 + e.tv_usec;
        let nw = now.tv_sec * 1_000_000 + now.tv_nsec / 1000;
        let d = if nw > st { nw - st } else { st - nw };
        report_num(dname, d as u64);
        if !report(name, d < 50_000) { failures += 1; }
    }
    close(epfd);
    close(fd);
    failures
}

/// `evtest2 btn <ms>`: count BTN_LEFT presses/releases (and SYN_DROPPED) that
/// reach event1 while the host sends pointer motion batched with button events.
unsafe fn btn_main(window_ms: c_int) -> i32 {
    let fd = open(b"/dev/input/event1\0".as_ptr(), O_RDONLY | O_NONBLOCK);
    if fd < 0 { report(b"btn_open", false); return 1; }
    drain(fd);
    let epfd = epoll_create1(0);
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd } };
    epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &mut ev as *mut _);
    let mut evs = [epoll_event { events: 0, data: epoll_data { u64_: 0 } }; 8];
    puts(b"evtest2 btn: ready\0".as_ptr());
    let (mut downs, mut ups, mut dropped, mut abs, mut frames) = (0u64, 0u64, 0u64, 0u64, 0u64);
    // Idle time only: stop after 4 s of silence once events were seen (or
    // window_ms of silence before any).
    let mut waited = 0;
    while waited < (if frames > 0 { 4000 } else { window_ms }) {
        let rc = epoll_wait(epfd, evs.as_mut_ptr(), 8, 200);
        if rc <= 0 { waited += 200; continue; }
        waited = 0;
        let mut buf = [0u8; 24 * 64];
        loop {
            let n = read(fd, buf.as_mut_ptr() as *mut c_void, buf.len());
            if n <= 0 { break; }
            for i in 0..(n as usize) / 24 {
                let e = core::ptr::read_unaligned(buf.as_ptr().add(i * 24) as *const input_event);
                if e.type_ == 1 && e.code == 0x110 {
                    if e.value == 1 { downs += 1; } else if e.value == 0 { ups += 1; }
                }
                if e.type_ == EV_SYN && e.code == 3 { dropped += 1; }
                if e.type_ == EV_SYN && e.code == SYN_REPORT { frames += 1; }
                if e.type_ == EV_ABS { abs += 1; }
            }
        }
    }
    report_num(b"btn_downs", downs);
    report_num(b"btn_ups", ups);
    report_num(b"btn_syn_dropped", dropped);
    report_num(b"btn_abs_events", abs);
    report_num(b"btn_frames", frames);
    close(epfd);
    close(fd);
    0
}

/// Read `fd` dry, noting SYN_DROPPED and the last value of `code`.
unsafe fn scan_key(fd: c_int, code: u16, dropped: &mut bool, last: &mut i32) {
    let mut buf = [0u8; 24 * 64];
    loop {
        let n = read(fd, buf.as_mut_ptr() as *mut c_void, buf.len());
        if n <= 0 { break; }
        for i in 0..(n as usize) / 24 {
            let e = core::ptr::read_unaligned(buf.as_ptr().add(i * 24) as *const input_event);
            if e.type_ == EV_SYN && e.code == 3 { *dropped = true; }
            if e.type_ == 1 && e.code == code { *last = e.value; }
        }
    }
}

unsafe fn key_held(fd: c_int, code: u16) -> bool {
    let mut bm = [0u8; 96];
    let rc = ioctl(fd, EVIOCGKEY_96, bm.as_mut_ptr());
    if rc < 0 { return false; } // Linux-style: returns bytes copied
    bm[code as usize / 8] & (1 << (code % 8)) != 0
}

/// Wait up to `ms` for `code` to reach `want` (1 down / 0 up) on `fd`.
unsafe fn wait_key(fd: c_int, code: u16, want: i32, ms: c_int) -> bool {
    let epfd = epoll_create1(0);
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd } };
    epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &mut ev as *mut _);
    let mut evs = [epoll_event { events: 0, data: epoll_data { u64_: 0 } }; 4];
    let mut waited = 0;
    let (mut d, mut last) = (false, -1);
    while waited < ms {
        if epoll_wait(epfd, evs.as_mut_ptr(), 4, 200) <= 0 { waited += 200; continue; }
        scan_key(fd, code, &mut d, &mut last);
        if last == want { close(epfd); return true; }
    }
    close(epfd);
    false
}

/// `evtest2 held`: EVIOCGKEY must report held keys. The host (evgkeytest.py)
/// presses BTN_LEFT on event1 and a keyboard key (KEY_A) on event0, holds,
/// then releases; a final press is held across a >256-event motion burst
/// while this program is not reading, so the queue overflows (SYN_DROPPED).
unsafe fn held_main() -> i32 {
    let tfd = open(b"/dev/input/event1\0".as_ptr(), O_RDONLY | O_NONBLOCK);
    let kfd = open(b"/dev/input/event0\0".as_ptr(), O_RDONLY | O_NONBLOCK);
    if tfd < 0 || kfd < 0 { report(b"held_open", false); return 1; }
    drain(tfd);
    drain(kfd);
    let mut failures = 0;
    puts(b"evtest2 held: waiting for BTN_LEFT down\n\0".as_ptr());
    let seen = wait_key(tfd, 0x110, 1, 40000);
    if !report(b"held_btn_seen_down", seen) { return 1; }
    if !report(b"held_btn_bit_set", key_held(tfd, 0x110)) { failures += 1; }
    wait_key(tfd, 0x110, 0, 20000);
    if !report(b"held_btn_bit_cleared", !key_held(tfd, 0x110)) { failures += 1; }
    if !wait_key(kfd, 30, 1, 20000) { report(b"held_key_seen_down", false); return 1; }
    if !report(b"held_key_bit_set", key_held(kfd, 30)) { failures += 1; }
    wait_key(kfd, 30, 0, 20000);
    if !report(b"held_key_bit_cleared", !key_held(kfd, 30)) { failures += 1; }
    // Overflow: press, then do not read while the host floods motion.
    if !wait_key(tfd, 0x110, 1, 20000) { report(b"held_ovf_seen_down", false); return 1; }
    puts(b"evtest2 held: overflow phase\n\0".as_ptr());
    let sl = epoll_create1(0);
    let mut none = [epoll_event { events: 0, data: epoll_data { u64_: 0 } }; 1];
    epoll_wait(sl, none.as_mut_ptr(), 1, 6000); // sleep, not reading
    close(sl);
    let (mut d, mut last) = (false, -1);
    scan_key(tfd, 0x110, &mut d, &mut last);
    report(b"held_ovf_syn_dropped", d);
    if !report(b"held_ovf_btn_still_held", key_held(tfd, 0x110)) { failures += 1; }
    close(tfd);
    close(kfd);
    failures
}

// ── `evtest2 info <node>` / `evtest2 gone <node> <secs>`: per-device nodes ──

struct Out { b: [u8; 160], n: usize }
impl Out {
    fn new() -> Out { Out { b: [0; 160], n: 0 } }
    fn s(&mut self, t: &[u8]) { for &c in t { if self.n < self.b.len() - 1 { self.b[self.n] = c; self.n += 1; } } }
    fn dec(&mut self, v: u64) {
        let mut t = [0u8; 20]; let mut i = t.len(); let mut n = v;
        loop { i -= 1; t[i] = b'0' + (n % 10) as u8; n /= 10; if n == 0 { break; } }
        self.s(&t[i..]);
    }
    fn hex(&mut self, v: u64) {
        let mut t = [0u8; 16]; let mut i = t.len(); let mut n = v;
        loop { i -= 1; t[i] = b"0123456789abcdef"[(n & 15) as usize]; n >>= 4; if n == 0 { break; } }
        self.s(&t[i..]);
    }
    fn flush(&mut self) {
        self.s(b"\n");
        unsafe { write(1, self.b.as_ptr() as *const c_void, self.n); }
        self.n = 0;
    }
}

/// Appends ` name=<..> bus=<hex> vid=<hex> pid=<hex>` for an open evdev fd.
unsafe fn id_fields(o: &mut Out, fd: c_int) {
    let mut name = [0u8; 64];
    let nr = ioctl(fd, EVIOCGNAME_64, name.as_mut_ptr());
    let mut l = 0;
    if nr > 0 { while l < 63 && name[l] != 0 { l += 1; } }
    let mut id = [0u16; 4];
    ioctl(fd, EVIOCGID_8, id.as_mut_ptr());
    o.s(b" name="); o.s(&name[..l]);
    o.s(b" bus="); o.hex(id[0] as u64);
    o.s(b" vid="); o.hex(id[1] as u64);
    o.s(b" pid="); o.hex(id[2] as u64);
}

unsafe fn arg_bytes<'a>(argc: isize, argv: *mut *mut u8, i: isize) -> &'a [u8] {
    if argc > i && !(*argv.offset(i)).is_null() {
        core::ffi::CStr::from_ptr(*argv.offset(i) as *const core::ffi::c_char).to_bytes()
    } else { b"" }
}

unsafe fn with_path(p: &[u8], f: impl FnOnce(*const u8) -> i32) -> i32 {
    let mut buf = [0u8; 128];
    let n = if p.len() < 127 { p.len() } else { 127 };
    buf[..n].copy_from_slice(&p[..n]);
    f(buf.as_ptr())
}

unsafe fn info_main(argc: isize, argv: *mut *mut u8) -> i32 {
    let node = arg_bytes(argc, argv, 2);
    with_path(node, |path| {
        let mut o = Out::new();
        o.s(b"INFO "); o.s(node);
        let fd = open(path, O_RDONLY | O_NONBLOCK);
        if fd < 0 {
            o.s(b" err="); o.dec(*__errno_location() as u64); o.flush();
            return 1;
        }
        id_fields(&mut o, fd);
        o.flush();
        close(fd);
        0
    })
}

unsafe fn gone_main(argc: isize, argv: *mut *mut u8) -> i32 {
    let node = arg_bytes(argc, argv, 2);
    let secs = parse_u64(arg_bytes(argc, argv, 3)).unwrap_or(10);
    with_path(node, |path| {
        let fd = open(path, O_RDONLY | O_NONBLOCK);
        let mut o = Out::new();
        if fd < 0 {
            o.s(b"GONE-OPEN err="); o.dec(*__errno_location() as u64); o.flush();
            return 1;
        }
        o.s(b"GONE-OPEN"); id_fields(&mut o, fd); o.flush();
        let mut events: u64 = 0;
        let mut last: i32 = -1;
        let mut buf = [0u8; 24 * 16];
        let mut iter = 0u64;
        while iter < secs * 10 {
            iter += 1;
            let mut p = pollfd { fd, events: 1, revents: 0 };
            poll(&mut p, 1, 100);
            let rv = p.revents as u16 as i32;
            if rv != last {
                last = rv;
                o.s(b"GONE-REVENTS 0x"); o.hex(rv as u64); o.flush();
            }
            if rv & 1 != 0 || rv & 0x18 != 0 {
                loop {
                    let r = read(fd, buf.as_mut_ptr() as *mut c_void, buf.len());
                    if r < 0 {
                        let e = *__errno_location();
                        if e == 11 { break; }
                        o.s(b"GONE-READ errno="); o.dec(e as u64);
                        o.s(b" revents=0x"); o.hex(rv as u64);
                        o.s(b" events="); o.dec(events); o.flush();
                        return 0;
                    }
                    if r == 0 { break; }
                    events += (r as u64) / 24;
                }
            }
        }
        o.s(b"GONE-TIMEOUT events="); o.dec(events); o.flush();
        1
    })
}

/// `evtest2 cap <node> <secs> [churn]`: count what one open of `node` receives
/// for `secs`, then print `CAP bytes=N keys=K` and exit on its own. A capture
/// that a harness has to kill (the old `cat > file &` + kill) can lose what the
/// reader already took off the queue; this one reports after its own deadline.
/// `churn` first opens and closes the node that many times, so a queue left
/// behind by a closed open (or an open id recycled onto a live one) shows up as
/// a short count here or in a concurrent capture.
/// `gofile` (optional): after CAP-OPEN, keep draining but do not start the
/// `secs` deadline until that path exists (at most 60 s), so a harness that
/// launches several readers through slow round trips can start every capture
/// window at once, after all readers are open.
unsafe fn cap_main(argc: isize, argv: *mut *mut u8) -> i32 {
    let node = arg_bytes(argc, argv, 2);
    let secs = parse_u64(arg_bytes(argc, argv, 3)).unwrap_or(3);
    let churn = parse_u64(arg_bytes(argc, argv, 4)).unwrap_or(0);
    with_path(node, |path| {
        let mut o = Out::new();
        for _ in 0..churn {
            let f = open(path, O_RDONLY | O_NONBLOCK);
            if f >= 0 { let mut b = [0u8; 24]; read(f, b.as_mut_ptr() as *mut c_void, 24); close(f); }
        }
        let fd = open(path, O_RDONLY | O_NONBLOCK);
        if fd < 0 { o.s(b"CAP err="); o.dec(*__errno_location() as u64); o.flush(); return 1; }
        o.s(b"CAP-OPEN"); o.flush();
        let (mut bytes, mut keys) = (0u64, 0u64);
        let mut buf = [0u8; 24 * 16];
        let mut tick = |bytes: &mut u64, keys: &mut u64| {
            let mut p = pollfd { fd, events: 1, revents: 0 };
            poll(&mut p, 1, 100);
            loop {
                let r = read(fd, buf.as_mut_ptr() as *mut c_void, buf.len());
                if r <= 0 { break; }
                *bytes += r as u64;
                for e in buf[..r as usize].chunks_exact(24) {
                    if u16::from_le_bytes([e[16], e[17]]) == 1 { *keys += 1; }
                }
            }
        };
        let go = arg_bytes(argc, argv, 5);
        if !go.is_empty() {
            let mut seen = false;
            for _ in 0..600 {
                seen = with_path(go, |g| { let f = open(g, O_RDONLY); if f >= 0 { close(f); 1 } else { 0 } }) == 1;
                if seen { break; }
                tick(&mut bytes, &mut keys);
            }
            if !seen { o.s(b"CAP go-timeout"); o.flush(); }
        }
        for _ in 0..secs * 10 { tick(&mut bytes, &mut keys); }
        close(fd);
        o.s(b"CAP bytes="); o.dec(bytes); o.s(b" keys="); o.dec(keys); o.flush();
        0
    })
}

#[no_mangle]
pub unsafe extern "C" fn ev_main(argc: isize, argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    if argc >= 2 {
        let a1 = *argv.add(1);
        match arg_bytes(argc, argv, 1) {
            b"info" => return info_main(argc, argv),
            b"gone" => return gone_main(argc, argv),
            b"cap" => return cap_main(argc, argv),
            _ => {}
        }
        if !a1.is_null() && core::ffi::CStr::from_ptr(a1 as *const core::ffi::c_char).to_bytes() == b"btn" {
            let f = btn_main(40000);
            puts(b"--- evtest2 btn done ---\0".as_ptr());
            return f;
        }
        if !a1.is_null() && core::ffi::CStr::from_ptr(a1 as *const core::ffi::c_char).to_bytes() == b"held" {
            let f = held_main();
            puts(b"--- evtest2 held done ---\0".as_ptr());
            return f;
        }
        if !a1.is_null() && core::ffi::CStr::from_ptr(a1 as *const core::ffi::c_char).to_bytes() == b"clock" {
            let f = clock_main();
            puts(b"--- evtest2 clock done ---\0".as_ptr());
            return f;
        }
        if !a1.is_null() && core::ffi::CStr::from_ptr(a1 as *const core::ffi::c_char).to_bytes() == b"herd" {
            let mut ms: c_int = 4000;
            if argc >= 3 && !(*argv.add(2)).is_null() {
                if let Some(v) = parse_u64(core::ffi::CStr::from_ptr(*argv.add(2) as *const core::ffi::c_char).to_bytes()) {
                    ms = v as c_int;
                }
            }
            let f = herd_main(ms);
            puts(b"--- evtest2 herd done ---\0".as_ptr());
            return f;
        }
    }
    let mut failures = 0i32;

    let fd = open(b"/dev/input/event1\0".as_ptr(), O_RDONLY | O_NONBLOCK);
    if fd < 0 {
        report(b"open_event1", false);
        puts(b"--- evtest2 done (open failed) ---\n\0".as_ptr());
        return 1;
    }
    report(b"open_event1", true);

    // EVIOCGNAME == "QEMU Virtio Tablet"
    let mut namebuf = [0u8; 64];
    let nr = ioctl(fd, EVIOCGNAME_64, namebuf.as_mut_ptr());
    let expect = b"QEMU Virtio Tablet";
    let name_ok = nr > 0 && &namebuf[..expect.len()] == expect;
    if !report(b"EVIOCGNAME_tablet", name_ok) { failures += 1; }

    // EVIOCGBIT(0) advertises EV_ABS (bit 3)
    let mut evbits = [0u8; 8];
    ioctl(fd, EVIOCGBIT0_8, evbits.as_mut_ptr());
    let has_abs = (evbits[0] & (1 << 3)) != 0;
    if !report(b"EVIOCGBIT_has_EV_ABS", has_abs) { failures += 1; }

    // EVIOCGBIT(0) advertises EV_REL and EVIOCGBIT(EV_REL) has REL_WHEEL: the
    // scroll wheel. libevdev (inside libinput) silently drops every event whose
    // code the node did not advertise, so without these bits wheel events
    // reach the node and never become a wl_pointer.axis.
    let has_rel = (evbits[0] & (1 << 2)) != 0;
    if !report(b"EVIOCGBIT_has_EV_REL", has_rel) { failures += 1; }
    let mut relbits = [0u8; 8];
    ioctl(fd, EVIOCGBIT_REL_8, relbits.as_mut_ptr());
    let has_wheel = (relbits[1] & (1 << (REL_WHEEL - 8))) != 0;
    if !report(b"EVIOCGBIT_REL_has_WHEEL", has_wheel) { failures += 1; }
    // No REL_X/REL_Y: an absolute pointer that also claims relative motion is
    // rejected by libinput.
    if !report(b"EVIOCGBIT_REL_no_XY", (relbits[0] & 0x03) == 0) { failures += 1; }

    // EVIOCGBIT(EV_ABS) advertises ABS_X and ABS_Y
    let mut absbits = [0u8; 8];
    ioctl(fd, EVIOCGBIT_ABS_8, absbits.as_mut_ptr());
    let has_xy = (absbits[0] & 0x03) == 0x03;
    if !report(b"EVIOCGBIT_ABS_has_XY", has_xy) { failures += 1; }

    // EVIOCGABS(ABS_X/Y).max == 32767, both present with equal resolution
    let mut ax = input_absinfo::default();
    let mut ay = input_absinfo::default();
    ioctl(fd, EVIOCGABS_X, &mut ax as *mut _);
    ioctl(fd, EVIOCGABS_Y, &mut ay as *mut _);
    let absinfo_ok = ax.maximum == 32767 && ay.maximum == 32767
        && ax.resolution == ay.resolution;
    if !report(b"EVIOCGABS_max_32767", absinfo_ok) { failures += 1; }

    // EVIOCGPROP has no INPUT_PROP_DIRECT (bit 1) — stays a pointer
    let mut props = [0u8; 8];
    ioctl(fd, EVIOCGPROP_8, props.as_mut_ptr());
    let not_direct = (props[0] & (1 << 1)) == 0;
    if !report(b"no_INPUT_PROP_DIRECT", not_direct) { failures += 1; }

    // EVIOCSCLOCKID(CLOCK_MONOTONIC)
    let clk = CLOCK_MONOTONIC;
    let sclk_ok = ioctl(fd, EVIOCSCLOCKID, &clk as *const _) == 0;
    if !report(b"EVIOCSCLOCKID_monotonic", sclk_ok) { failures += 1; }

    // epoll: idle (no motion) must NOT wake within 300ms — no false POLLIN.
    let epfd = epoll_create1(0);
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd } };
    epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &mut ev as *mut _);
    let mut evs = [epoll_event { events: 0, data: epoll_data { u64_: 0 } }; 8];
    let idle_rc = epoll_wait(epfd, evs.as_mut_ptr(), 8, 300);
    if !report(b"epoll_idle_no_false_wake", idle_rc == 0) { failures += 1; }

    // Motion phase: wait up to ~6s for injected pointer motion. If frames arrive,
    // validate an ABS axis + SYN_REPORT and monotonic timestamps. Informational
    // (depends on host injection) — reported, not counted as a hard failure.
    puts(b"evtest2: waiting up to 6s for injected pointer motion...\n\0".as_ptr());
    let mut saw_abs = false;
    let mut saw_syn = false;
    let mut last_ts: i64 = -1;
    let mut ts_monotonic = true;
    // Timestamp *resolution*: with a whole-tick stamp every tv_usec is a
    // multiple of 10 000 and every event drained in one tick shares a timeval.
    // Counting both tells a finer clock from a coarse one without a second run.
    let mut n_events: u64 = 0;
    let mut n_subtick: u64 = 0;
    let mut n_distinct: u64 = 0;
    // Wheel: EV_REL REL_WHEEL (+1 up / -1 down per detent) and REL_HWHEEL.
    let mut wheel_up: u64 = 0;
    let mut wheel_down: u64 = 0;
    let mut hwheel: u64 = 0;
    let mut waited = 0;
    // Collect a real sample before stopping: exiting on the first ABS+SYN pair
    // leaves too few timestamps to say anything about resolution.
    while waited < 6000 && !(saw_abs && saw_syn && n_events >= 32)
        && !(wheel_up > 0 && wheel_down > 0 && saw_syn) {
        let rc = epoll_wait(epfd, evs.as_mut_ptr(), 8, 500);
        waited += 500;
        if rc <= 0 { continue; }
        let mut buf = [0u8; 24 * 64];
        let n = read(fd, buf.as_mut_ptr() as *mut c_void, buf.len());
        if n <= 0 { continue; }
        let cnt = (n as usize) / 24;
        for i in 0..cnt {
            let e = core::ptr::read_unaligned(buf.as_ptr().add(i * 24) as *const input_event);
            let ts = e.tv_sec * 1_000_000 + e.tv_usec;
            if last_ts >= 0 && ts < last_ts { ts_monotonic = false; }
            if last_ts != ts { n_distinct += 1; }
            if e.tv_usec % 10_000 != 0 { n_subtick += 1; }
            n_events += 1;
            last_ts = ts;
            if e.type_ == EV_ABS && (e.code == ABS_X || e.code == ABS_Y) { saw_abs = true; }
            if e.type_ == EV_SYN && e.code == SYN_REPORT { saw_syn = true; }
            if e.type_ == EV_REL && e.code == REL_WHEEL {
                if e.value > 0 { wheel_up += 1; } else if e.value < 0 { wheel_down += 1; }
            }
            if e.type_ == EV_REL && e.code == REL_HWHEEL { hwheel += 1; }
        }
    }
    if wheel_up + wheel_down + hwheel > 0 {
        report_num(b"wheel_up", wheel_up);
        report_num(b"wheel_down", wheel_down);
        report_num(b"hwheel", hwheel);
        report(b"wheel_frame", (wheel_up > 0 || wheel_down > 0) && saw_syn);
    }
    // A wheel-only frame is SYN without ABS: not a broken motion frame.
    if saw_abs || (saw_syn && wheel_up + wheel_down + hwheel == 0) {
        report(b"motion_abs_frame", saw_abs && saw_syn);
        report(b"motion_ts_monotonic", ts_monotonic);
        report_num(b"motion_events", n_events);
        report_num(b"motion_ts_subtick_usec", n_subtick);
        report_num(b"motion_ts_distinct", n_distinct);
        // Informational, like the two above: only meaningful under injection.
        report(b"motion_ts_subtick", n_subtick > 0);
    } else {
        puts(b"motion: none observed (no injection) - capability checks above are the gate\n\0".as_ptr());
    }

    close(epfd);
    close(fd);
    puts(b"--- evtest2 done ---\n\0".as_ptr());
    failures
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}
