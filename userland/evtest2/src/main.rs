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
const EV_ABS: u16 = 3;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const SYN_REPORT: u16 = 0;

// ioctl request codes ('E' = 0x45). _IOC(dir,type,nr,size).
const EVIOCGNAME_64: c_ulong = 0x80404506;    // _IOC(R,'E',0x06,64)
const EVIOCGPROP_8:  c_ulong = 0x80084509;    // _IOC(R,'E',0x09,8)
const EVIOCGBIT0_8:  c_ulong = 0x80084520;    // _IOC(R,'E',0x20+0,8)
const EVIOCGBIT_ABS_8: c_ulong = 0x80084523;  // _IOC(R,'E',0x20+3,8)
const EVIOCGABS_X: c_ulong = 0x80184540;      // _IOR('E',0x40,input_absinfo=24)
const EVIOCGABS_Y: c_ulong = 0x80184541;
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
}

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

#[no_mangle]
pub unsafe extern "C" fn ev_main(_argc: isize, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
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
    let mut waited = 0;
    // Collect a real sample before stopping: exiting on the first ABS+SYN pair
    // leaves too few timestamps to say anything about resolution.
    while waited < 6000 && !(saw_abs && saw_syn && n_events >= 32) {
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
        }
    }
    if saw_abs || saw_syn {
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
