//! vttest — coverage for the virtual consoles: `servers/tty/src/vt.rs`, the
//! `VnodeKind::DevVt` node in `servers/vfs`, and the VT/KD ioctl routing in
//! `kernel/src/syscall.rs`.
//!
//! # Why the assertions are shaped the way they are
//!
//! Almost everything a VT does is invisible to the process doing it: a switch
//! moves pixels, and the program that asked for it sees only a zero return.
//! So every subtest here asserts on something that is *readable back through a
//! second syscall* — `VT_GETSTATE` after `VT_ACTIVATE`, `KDGETMODE` after
//! `KDSETMODE`, a notification byte after a switch — rather than on the return
//! code alone, which was 0 for the whole ioctl surface before any of it was
//! routed anywhere.
//!
//! Two properties in particular cannot be proved by a single fd:
//!
//! * **Per-node addressing.** `KDSETMODE` on `/dev/tty3` must change VT 3, not
//!   whatever VT is on screen. A router that folds every console fd onto the
//!   active VT (which is what the pre-existing `fd > 2 && fd_is_console_stdio`
//!   remap in `sys_ioctl` would have done) passes every single-fd test and
//!   fails `modes_are_per_node` — so that subtest holds two fds and compares.
//! * **Per-open notification state.** `/dev/tty0`'s readability is "the active
//!   VT changed since *this open* last read". One fd cannot tell that from a
//!   global flag; `notify_is_per_open` holds two and drains only one.
//!
//! # Sub-commands
//!
//! * `vttest` — the automated suite. Ends back on VT 1 with the console in
//!   `KD_TEXT` and the keyboard in `K_XLATE`, whatever happened in between.
//! * `vttest hold <vt> <ms>` — switch to `<vt>`, print a marker, wait, switch
//!   back to 1. For the screenshot pair: the framebuffer is the only place a
//!   VT switch is visible, and the serial log shows every VT's output
//!   regardless (the console mirror runs ahead of the paint gate), so "VT 2
//!   does not show VT 1's text" is a claim only a screenshot can settle.
//! * `vttest kbmode <raw|xlate>` — set the active VT's keyboard mode and exit.
//!   The mode lives in the VT, not in this process, so it persists — which is
//!   what lets the console-tap gate be measured against `evsplit` running as a
//!   separate process, and what makes restoring `xlate` afterwards mandatory.
//! * `vttest gfx <ms>` — `KD_GRAPHICS`, wait, `KD_TEXT`. Proves the console
//!   goes silent and then repaints from the mirror rather than from a clear.
//!
//! Each check prints "<name>: PASS" or "<name>: FAIL"; the final line is
//! `vttest: <passed>/<total>` and the exit code is the failure count.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;

type c_int = i32;
type c_ulong = u64;
type size_t = usize;
type ssize_t = isize;

const O_RDWR: c_int = 0o2;
const O_NONBLOCK: c_int = 0o4000;

const EAGAIN: c_int = 11;
const EACCES: c_int = 13;
const EBUSY: c_int = 16;
const EINVAL: c_int = 22;
const ENOTTY: c_int = 25;

// VT_* — <linux/vt.h>
const VT_OPENQRY: c_ulong = 0x5600;
const VT_GETMODE: c_ulong = 0x5601;
const VT_SETMODE: c_ulong = 0x5602;
const VT_GETSTATE: c_ulong = 0x5603;
const VT_ACTIVATE: c_ulong = 0x5606;
const VT_WAITACTIVE: c_ulong = 0x5607;

// KD* — <linux/kd.h>
const KDGKBTYPE: c_ulong = 0x4B33;
const KDSETMODE: c_ulong = 0x4B3A;
const KDGETMODE: c_ulong = 0x4B3B;
const KDGKBMODE: c_ulong = 0x4B44;
const KDSKBMODE: c_ulong = 0x4B45;

const KD_TEXT: usize = 0;
const KD_GRAPHICS: usize = 1;

const K_RAW: usize = 0;
const K_XLATE: usize = 1;
const K_MEDIUMRAW: usize = 2;

const VT_AUTO: u8 = 0;
const VT_PROCESS: u8 = 1;

// DRM — <drm/drm.h>. Only the three commands a master test needs. They live in
// the VT suite, not in `drmsmoke`, because what they measure is arbitration
// between a console and a display client: that is a VT property, and the
// harness that can switch VTs is this one.
const DRM_IOCTL_SET_MASTER: c_ulong = 0x0000_641E;
const DRM_IOCTL_DROP_MASTER: c_ulong = 0x0000_641F;
const DRM_IOCTL_MODE_DIRTYFB: c_ulong = 0xC018_64B1;

const POLLIN: i16 = 0x001;

const EPOLL_CTL_ADD: c_int = 1;
const EPOLLIN: u32 = 0x001;
const EPOLLET: u32 = 1 << 31;

/// `struct vt_stat { unsigned short v_active, v_signal, v_state; }`.
#[repr(C)]
#[derive(Clone, Copy)]
struct vt_stat {
    v_active: u16,
    v_signal: u16,
    v_state: u16,
}

/// `struct vt_mode { char mode; char waitv; short relsig, acqsig, frsig; }`.
///
/// Eight bytes, not six: two chars then three 2-byte-aligned shorts. A kernel
/// that copies six drops `frsig` AND leaves the caller's last two bytes
/// unwritten on `VT_GETMODE`, so a GETMODE→SETMODE round trip feeds stack
/// garbage back as an acquire signal. `_guard` is what makes this file able to
/// see that: it is set to a known pattern before every GETMODE and must come
/// back untouched.
#[repr(C)]
#[derive(Clone, Copy)]
struct vt_mode {
    mode: u8,
    waitv: u8,
    relsig: u16,
    acqsig: u16,
    frsig: u16,
    _guard: u32,
}

/// `struct drm_mode_fb_dirty_cmd` — 24 bytes, which is the 0x18 size field of
/// the DIRTYFB request code.
#[repr(C)]
#[derive(Clone, Copy)]
struct drm_mode_fb_dirty_cmd {
    fb_id: u32,
    flags: u32,
    color: u32,
    num_clips: u32,
    clips_ptr: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct pollfd {
    fd: c_int,
    events: i16,
    revents: i16,
}

#[repr(C)]
pub struct timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union epoll_data {
    pub ptr: *mut c_void,
    pub fd: c_int,
    pub u32_: u32,
    pub u64_: u64,
}

#[cfg(target_arch = "x86_64")]
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct epoll_event {
    pub events: u32,
    pub data: epoll_data,
}
#[cfg(not(target_arch = "x86_64"))]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct epoll_event {
    pub events: u32,
    pub data: epoll_data,
}

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;

    pub fn open(path: *const u8, oflag: c_int, ...) -> c_int;
    pub fn read(fd: c_int, buf: *mut c_void, count: size_t) -> ssize_t;
    pub fn write(fd: c_int, buf: *const c_void, count: size_t) -> ssize_t;
    pub fn close(fd: c_int) -> c_int;
    pub fn ioctl(fd: c_int, request: c_ulong, out: *mut c_void) -> c_int;
    pub fn pipe(fds: *mut c_int) -> c_int;
    pub fn poll(fds: *mut pollfd, nfds: u64, timeout: c_int) -> c_int;
    pub fn epoll_create1(flags: c_int) -> c_int;
    pub fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut epoll_event) -> c_int;
    pub fn epoll_wait(epfd: c_int, events: *mut epoll_event, maxevents: c_int, timeout: c_int) -> c_int;
    pub fn nanosleep(rqtp: *const timespec, rmtp: *mut timespec) -> c_int;
    pub fn exit(status: c_int) -> !;
    pub fn __errno_location() -> *mut c_int;
}

// ── Entry point (identical shape to ptytest's) ───────────────────────────────

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset vt_main",
    "   and rsp, -16",
    "   call relibc_start_v1",
    "   ud2"
);

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   mov x29, #0",
    "   mov x30, #0",
    "   mov x0, sp",
    "   adrp x1, vt_main",
    "   add x1, x1, :lo12:vt_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}

// ── Output helpers ───────────────────────────────────────────────────────────

unsafe fn out(s: &[u8]) {
    write(1, s.as_ptr() as *const c_void, s.len());
}

unsafe fn out_num(mut v: usize) {
    let mut d = [0u8; 20];
    let mut n = 0;
    if v == 0 {
        d[0] = b'0';
        n = 1;
    } else {
        while v > 0 {
            d[n] = b'0' + (v % 10) as u8;
            n += 1;
            v /= 10;
        }
        d[..n].reverse();
    }
    out(&d[..n]);
}

unsafe fn out_int(v: c_int) {
    if v < 0 { out(b"-"); out_num((-(v as i64)) as usize); } else { out_num(v as usize); }
}

unsafe fn report(name: &[u8], passed: bool) -> bool {
    out(name);
    out(if passed { b": PASS\n" } else { b": FAIL\n" });
    passed
}

unsafe fn errno() -> c_int { *__errno_location() }

unsafe fn sleep_ms(ms: i64) {
    let ts = timespec { tv_sec: ms / 1000, tv_nsec: (ms % 1000) * 1_000_000 };
    nanosleep(&ts as *const timespec, core::ptr::null_mut());
}

// ── VT helpers ───────────────────────────────────────────────────────────────

/// `VT_ACTIVATE` + `VT_WAITACTIVE`, i.e. a synchronous switch.
///
/// `VT_ACTIVATE` is asynchronous by design when the outgoing console runs
/// `VT_PROCESS` (it returns as soon as the release signal is sent), so asserting
/// on it alone would be asserting on "the request was accepted". `VT_WAITACTIVE`
/// is the only thing that means the display has moved.
unsafe fn switch_to(fd: c_int, vt: usize) -> bool {
    if ioctl(fd, VT_ACTIVATE, vt as *mut c_void) != 0 { return false; }
    ioctl(fd, VT_WAITACTIVE, vt as *mut c_void) == 0
}

unsafe fn active_vt(fd: c_int) -> c_int {
    let mut st = vt_stat { v_active: 0, v_signal: 0, v_state: 0 };
    if ioctl(fd, VT_GETSTATE, &mut st as *mut vt_stat as *mut c_void) != 0 { return -1; }
    st.v_active as c_int
}

unsafe fn get_mode(fd: c_int) -> c_int {
    let mut m: c_int = -1;
    if ioctl(fd, KDGETMODE, &mut m as *mut c_int as *mut c_void) != 0 { return -1; }
    m
}

unsafe fn set_mode(fd: c_int, mode: usize) -> c_int {
    ioctl(fd, KDSETMODE, mode as *mut c_void)
}

unsafe fn kb_mode(fd: c_int) -> c_int {
    let mut m: c_int = -1;
    if ioctl(fd, KDGKBMODE, &mut m as *mut c_int as *mut c_void) != 0 { return -1; }
    m
}

/// One non-blocking read of the /dev/tty0 notification. Returns the new active
/// VT number, 0 for "nothing pending" (EAGAIN), or -1 for anything else.
unsafe fn notify_read(fd: c_int) -> c_int {
    let mut b: u8 = 0;
    let n = read(fd, &mut b as *mut u8 as *mut c_void, 1);
    if n == 1 { return b as c_int; }
    if n < 0 && errno() == EAGAIN { return 0; }
    -1
}

unsafe fn poll_in(fd: c_int, timeout_ms: c_int) -> bool {
    let mut p = pollfd { fd, events: POLLIN, revents: 0 };
    poll(&mut p as *mut pollfd, 1, timeout_ms) > 0 && (p.revents & POLLIN) != 0
}

// ── 1. The node exists and answers VT_GETSTATE ───────────────────────────────

unsafe fn t_open_and_state(tty0: c_int) -> bool {
    let a = active_vt(tty0);
    // v_state's bit 0 is always set (there is no VT 0), and the active console
    // must be marked in use — a zeroed struct that "succeeded" would pass an
    // `a >= 1` test on its own.
    let mut st = vt_stat { v_active: 0, v_signal: 0, v_state: 0 };
    let ok = ioctl(tty0, VT_GETSTATE, &mut st as *mut vt_stat as *mut c_void) == 0;
    let inuse = st.v_state & (1 << a) != 0;
    out(b"  active="); out_int(a);
    out(b" v_state="); out_num(st.v_state as usize); out(b"\n");
    report(b"getstate", ok && (1..=6).contains(&a) && (st.v_state & 1) != 0 && inuse)
}

// ── 2. A switch, and the state that proves it happened ───────────────────────

unsafe fn t_activate(tty0: c_int) -> bool {
    let ok2 = switch_to(tty0, 2) && active_vt(tty0) == 2;
    let ok1 = switch_to(tty0, 1) && active_vt(tty0) == 1;
    report(b"activate_waitactive", ok2 && ok1)
}

// ── 3. An out-of-range switch is refused, not clamped ────────────────────────

unsafe fn t_activate_invalid(tty0: c_int) -> bool {
    let r = ioctl(tty0, VT_ACTIVATE, 9usize as *mut c_void);
    let e = errno();
    let still = active_vt(tty0);
    report(b"activate_out_of_range", r < 0 && e == EINVAL && still == 1)
}

// ── 4. Per-node addressing ───────────────────────────────────────────────────
//
// The subtest the whole ioctl-routing change exists for. `/dev/tty3` and
// `/dev/tty4` are both off screen, so neither KDSETMODE touches a pixel; all
// that is being measured is *which VT the fd named*. A router that resolved
// every console fd to the active VT would set and read back the same state
// through both fds and report them equal.

unsafe fn t_modes_are_per_node() -> bool {
    let t3 = open(b"/dev/tty3\0".as_ptr(), O_RDWR);
    let t4 = open(b"/dev/tty4\0".as_ptr(), O_RDWR);
    if t3 < 0 || t4 < 0 {
        if t3 >= 0 { close(t3); }
        if t4 >= 0 { close(t4); }
        return report(b"modes_are_per_node", false);
    }
    let set_ok = set_mode(t3, KD_GRAPHICS) == 0;
    let m3 = get_mode(t3);
    let m4 = get_mode(t4);
    // Restore before judging, so a failure here cannot leave VT 3 muted.
    set_mode(t3, KD_TEXT);
    let m3b = get_mode(t3);
    close(t3);
    close(t4);
    out(b"  tty3="); out_int(m3); out(b" tty4="); out_int(m4);
    out(b" tty3_after_restore="); out_int(m3b); out(b"\n");
    report(b"modes_are_per_node",
           set_ok && m3 == KD_GRAPHICS as c_int && m4 == KD_TEXT as c_int
                  && m3b == KD_TEXT as c_int)
}

// ── 5. struct vt_mode is 8 bytes ─────────────────────────────────────────────

unsafe fn t_vt_mode_roundtrip() -> bool {
    let t5 = open(b"/dev/tty5\0".as_ptr(), O_RDWR);
    if t5 < 0 { return report(b"vt_mode_roundtrip", false); }
    let want = vt_mode {
        mode: VT_PROCESS, waitv: 0,
        relsig: 10, acqsig: 12, frsig: 30,   // SIGUSR1 / SIGUSR2 / SIGPWR
        _guard: 0xDEAD_BEEF,
    };
    let set_ok = ioctl(t5, VT_SETMODE, &want as *const vt_mode as *mut c_void) == 0;
    let mut got = vt_mode { mode: 0, waitv: 0, relsig: 0, acqsig: 0, frsig: 0,
                            _guard: 0xA5A5_A5A5 };
    let get_ok = ioctl(t5, VT_GETMODE, &mut got as *mut vt_mode as *mut c_void) == 0;
    // Hand VT 5 back to VT_AUTO: leaving it in VT_PROCESS owned by a process
    // that is about to exit would make the next switch to it wait on the
    // handshake watchdog.
    let auto = vt_mode { mode: VT_AUTO, waitv: 0, relsig: 0, acqsig: 0, frsig: 0, _guard: 0 };
    ioctl(t5, VT_SETMODE, &auto as *const vt_mode as *mut c_void);
    close(t5);
    let fields = got.mode == VT_PROCESS && got.relsig == 10
              && got.acqsig == 12 && got.frsig == 30;
    let guard = got._guard == 0xA5A5_A5A5;
    out(b"  frsig="); out_num(got.frsig as usize);
    out(b" guard_intact="); out(if guard { b"yes" } else { b"no" }); out(b"\n");
    report(b"vt_mode_roundtrip", set_ok && get_ok && fields && guard)
}

// ── 6. KDGKBTYPE / keyboard mode round trip ──────────────────────────────────

unsafe fn t_kb_mode(tty0: c_int) -> bool {
    let mut kbt: u8 = 0;
    let type_ok = ioctl(tty0, KDGKBTYPE, &mut kbt as *mut u8 as *mut c_void) == 0 && kbt == 2;
    let start = kb_mode(tty0);
    let set_ok = ioctl(tty0, KDSKBMODE, K_MEDIUMRAW as *mut c_void) == 0;
    let mid = kb_mode(tty0);
    // Restore first, judge after: K_XLATE is what keeps the console tap fed,
    // and a FAIL that also left the keyboard raw would take the machine's
    // console down with it.
    ioctl(tty0, KDSKBMODE, K_XLATE as *mut c_void);
    let back = kb_mode(tty0);
    let bad = ioctl(tty0, KDSKBMODE, 9usize as *mut c_void);
    report(b"kb_mode_roundtrip",
           type_ok && start == K_XLATE as c_int && set_ok
                   && mid == K_MEDIUMRAW as c_int && back == K_XLATE as c_int
                   && bad < 0 && errno() == EINVAL)
}

// ── 7. VT_OPENQRY ────────────────────────────────────────────────────────────

unsafe fn t_openqry(tty0: c_int) -> bool {
    let mut n: c_int = -99;
    let ok = ioctl(tty0, VT_OPENQRY, &mut n as *mut c_int as *mut c_void) == 0;
    // Either a free console (2..=6 — VT 1 is always allocated) or -1 for none.
    report(b"openqry", ok && (n == -1 || (2..=6).contains(&n)))
}

// ── 8. The notification fd ───────────────────────────────────────────────────
//
// The contract a libseat/logind shim's get_fd()/dispatch() pair is built on:
// a fresh open is quiet, a switch makes it readable, the read yields the new
// VT number and re-arms it. Nothing else in the tree can observe a VT switch
// asynchronously.

unsafe fn t_notify(tty0: c_int) -> bool {
    let n = open(b"/dev/tty0\0".as_ptr(), O_RDWR | O_NONBLOCK);
    if n < 0 { return report(b"notify_edge", false); }
    // A fresh open must NOT be readable: it starts from what is on screen.
    let quiet = notify_read(n) == 0 && !poll_in(n, 0);
    let moved = switch_to(tty0, 3);
    let ready = poll_in(n, 1000);
    let got = notify_read(n);
    let rearmed = notify_read(n) == 0 && !poll_in(n, 0);
    switch_to(tty0, 1);
    let back = notify_read(n) == 1;
    close(n);
    out(b"  fresh_quiet="); out(if quiet { b"yes" } else { b"no" });
    out(b" byte="); out_int(got); out(b"\n");
    report(b"notify_edge", quiet && moved && ready && got == 3 && rearmed && back)
}

unsafe fn t_notify_per_open(tty0: c_int) -> bool {
    let a = open(b"/dev/tty0\0".as_ptr(), O_RDWR | O_NONBLOCK);
    let b = open(b"/dev/tty0\0".as_ptr(), O_RDWR | O_NONBLOCK);
    if a < 0 || b < 0 {
        if a >= 0 { close(a); }
        if b >= 0 { close(b); }
        return report(b"notify_is_per_open", false);
    }
    let moved = switch_to(tty0, 4);
    // Drain A only. B must still be holding its own edge — a global flag
    // consumed by whoever reads first cannot do this.
    let a_got = notify_read(a);
    let a_drained = notify_read(a) == 0;
    let b_still = poll_in(b, 0);
    let b_got = notify_read(b);
    switch_to(tty0, 1);
    close(a);
    close(b);
    report(b"notify_is_per_open",
           moved && a_got == 4 && a_drained && b_still && b_got == 4)
}

/// EPOLLET on the notification fd. The seq the VFS reports for `/dev/tty0` is
/// the active VT number, so an edge-triggered interest must fire once per
/// *switch* and not once per `epoll_wait` pass — which is exactly what a
/// level-triggered fallback (seq `None`) would do, and what would spin a
/// compositor's event loop at 100 % CPU.
unsafe fn t_notify_epollet(tty0: c_int) -> bool {
    let n = open(b"/dev/tty0\0".as_ptr(), O_RDWR | O_NONBLOCK);
    let ep = epoll_create1(0);
    if n < 0 || ep < 0 {
        if n >= 0 { close(n); }
        if ep >= 0 { close(ep); }
        return report(b"notify_epollet", false);
    }
    let mut ev = epoll_event { events: EPOLLIN | EPOLLET, data: epoll_data { fd: n } };
    let add_ok = epoll_ctl(ep, EPOLL_CTL_ADD, n, &mut ev as *mut epoll_event) == 0;
    let mut got = [epoll_event { events: 0, data: epoll_data { u64_: 0 } }; 4];
    // Nothing has happened yet.
    let quiet = epoll_wait(ep, got.as_mut_ptr(), 4, 0) == 0;
    switch_to(tty0, 5);
    let fired = epoll_wait(ep, got.as_mut_ptr(), 4, 1000) == 1;
    let byte = notify_read(n);
    // Same VT still active and the edge consumed: an ET interest must stay
    // silent. This is the assertion a seq of 0 (or None) fails.
    let silent = epoll_wait(ep, got.as_mut_ptr(), 4, 0) == 0;
    switch_to(tty0, 1);
    let refired = epoll_wait(ep, got.as_mut_ptr(), 4, 1000) == 1;
    let byte2 = notify_read(n);
    close(ep);
    close(n);
    report(b"notify_epollet",
           add_ok && quiet && fired && byte == 5 && silent && refired && byte2 == 1)
}

// ── 9. A VT ioctl on something that is not a VT is ENOTTY ────────────────────
//
// The routing guard has to be narrow as well as present: it sits in front of
// the fd<=2 ENOTTY check in sys_ioctl, so an over-broad version would answer
// KDGETMODE for a pipe.

unsafe fn t_enotty_on_pipe() -> bool {
    let mut fds = [0 as c_int; 2];
    if pipe(fds.as_mut_ptr()) != 0 { return report(b"enotty_on_pipe", false); }
    let mut m: c_int = 0;
    let r = ioctl(fds[0], KDGETMODE, &mut m as *mut c_int as *mut c_void);
    let e = errno();
    close(fds[0]);
    close(fds[1]);
    report(b"enotty_on_pipe", r < 0 && e == ENOTTY)
}

// ── 10. KD_GRAPHICS silences the console, KD_TEXT repaints it ────────────────
//
// Only the framebuffer can show this, so what is asserted here is the state
// machine: the mode takes effect immediately on the ACTIVE console (a
// compositor sets KD_GRAPHICS before its first present, and a mode that landed
// at the next switch would leave the console painting over its opening frames),
// and KD_TEXT is accepted back. The `gfx` sub-command below is the visual half.

unsafe fn t_graphics_mode(tty0: c_int) -> bool {
    let before = get_mode(tty0);
    let to_gfx = set_mode(tty0, KD_GRAPHICS) == 0;
    let in_gfx = get_mode(tty0);
    // This line is written while the console is muted. It must reach the serial
    // log (the mirror runs ahead of the paint gate) and must NOT be on screen.
    out(b"  [written under KD_GRAPHICS]\n");
    sleep_ms(200);
    let to_txt = set_mode(tty0, KD_TEXT) == 0;
    let in_txt = get_mode(tty0);
    let bad = set_mode(tty0, 7);
    report(b"graphics_mode",
           before == KD_TEXT as c_int && to_gfx && in_gfx == KD_GRAPHICS as c_int
                 && to_txt && in_txt == KD_TEXT as c_int
                 && bad < 0 && errno() == EINVAL)
}

// ── Sub-commands ─────────────────────────────────────────────────────────────

unsafe fn cmd_hold(tty0: c_int, vt: usize, ms: i64) -> c_int {
    if !switch_to(tty0, vt) {
        out(b"vttest hold: switch failed\n");
        return 1;
    }
    out(b"=== VTMARK-"); out_num(vt); out(b" === this text belongs to VT ");
    out_num(vt); out(b" only\n");
    // `ms == 0` means "switch and stay". The screenshot pair needs the display
    // to sit on the target VT across a whole separate driver invocation, and
    // staying is also the stronger claim: VT 1's text has to survive an
    // arbitrary interval, not a timed one this program controls.
    if ms > 0 {
        sleep_ms(ms);
        if vt != 1 && !switch_to(tty0, 1) {
            out(b"vttest hold: switch back failed\n");
            return 1;
        }
    }
    out(b"vttest hold: done, active="); out_int(active_vt(tty0)); out(b"\n");
    0
}

unsafe fn cmd_kbmode(tty0: c_int, raw: bool) -> c_int {
    let want = if raw { K_RAW } else { K_XLATE };
    if ioctl(tty0, KDSKBMODE, want as *mut c_void) != 0 {
        out(b"vttest kbmode: KDSKBMODE failed\n");
        return 1;
    }
    out(b"vttest kbmode: "); out_int(kb_mode(tty0)); out(b"\n");
    0
}

unsafe fn cmd_gfx(tty0: c_int, ms: i64) -> c_int {
    if set_mode(tty0, KD_GRAPHICS) != 0 {
        out(b"vttest gfx: KDSETMODE(KD_GRAPHICS) failed\n");
        return 1;
    }
    out(b"=== GFXMARK === printed while the console is in KD_GRAPHICS\n");
    sleep_ms(ms);
    if set_mode(tty0, KD_TEXT) != 0 {
        out(b"vttest gfx: KDSETMODE(KD_TEXT) failed\n");
        return 1;
    }
    out(b"vttest gfx: back in KD_TEXT\n");
    0
}

// ── 11-13. DRM master ────────────────────────────────────────────────────────
//
// Master decides who may put pixels on the screen, and until it was enforced
// the VT layer could take the display back but not keep it: `vt.rs`'s
// `fb_vt_scanout_revoke` cleared the ownership word and the client's very next
// present set it again, so a switch away from a graphical session flickered
// back within a frame.
//
// The probe is DIRTYFB with `fb_id` 0. It is in the gated set, it is cheap, and
// naming no framebuffer means the handler finds nothing to flush — so a
// permitted call returns 0 while touching neither the scanout nor the console,
// and the only thing these subtests can move is the permission itself. Anything
// that actually presented would blank the console mid-suite and make the run
// unreadable.

unsafe fn card0() -> c_int {
    open(b"/dev/dri/card0\0".as_ptr(), O_RDWR)
}

unsafe fn set_master(fd: c_int) -> c_int {
    ioctl(fd, DRM_IOCTL_SET_MASTER, core::ptr::null_mut())
}

unsafe fn drop_master(fd: c_int) -> c_int {
    ioctl(fd, DRM_IOCTL_DROP_MASTER, core::ptr::null_mut())
}

unsafe fn dirtyfb(fd: c_int) -> c_int {
    let mut cmd = drm_mode_fb_dirty_cmd {
        fb_id: 0, flags: 0, color: 0, num_clips: 0, clips_ptr: 0,
    };
    ioctl(fd, DRM_IOCTL_MODE_DIRTYFB, &mut cmd as *mut drm_mode_fb_dirty_cmd as *mut c_void)
}

/// Two opens cannot both be master, and dropping frees it for the other.
unsafe fn t_master_is_exclusive() -> bool {
    let a = card0();
    let b = card0();
    if a < 0 || b < 0 {
        if a >= 0 { close(a); }
        if b >= 0 { close(b); }
        return report(b"master_is_exclusive", false);
    }
    let grant = set_master(a) == 0;
    let busy_rc = set_master(b);
    let busy = busy_rc < 0 && errno() == EBUSY;
    // Idempotent for the holder: Linux answers 0, and a compositor that
    // re-asserts master on every session resume must not be told EBUSY by
    // itself.
    let again = set_master(a) == 0;
    let released = drop_master(a) == 0;
    let handover = set_master(b) == 0;
    drop_master(b);
    close(a);
    close(b);
    out(b"  grant="); out(if grant { b"0" } else { b"!0" });
    out(b" second="); out_int(busy_rc);
    out(b" errno="); out_int(if busy { EBUSY } else { errno() }); out(b"\n");
    report(b"master_is_exclusive", grant && busy && again && released && handover)
}

/// `DROP_MASTER` from an open that never held it is EINVAL, not success.
unsafe fn t_drop_master_not_master() -> bool {
    let fd = card0();
    if fd < 0 { return report(b"drop_master_not_master", false); }
    let r = drop_master(fd);
    let e = errno();
    close(fd);
    out(b"  rc="); out_int(r); out(b" errno="); out_int(e); out(b"\n");
    report(b"drop_master_not_master", r < 0 && e == EINVAL)
}

/// A non-master open cannot present. **EACCES specifically**: that is what
/// Linux's `drm_ioctl_permit()` returns, and it is what smithay maps to the
/// recoverable `DrmError::Access`. ENODEV in its place makes smithay tear the
/// device down and takes the compositor with it, so the value is asserted, not
/// merely the sign.
unsafe fn t_master_gates_present() -> bool {
    let a = card0();
    let b = card0();
    if a < 0 || b < 0 {
        if a >= 0 { close(a); }
        if b >= 0 { close(b); }
        return report(b"master_gates_present", false);
    }
    let grant = set_master(a) == 0;
    let mine = dirtyfb(a) == 0;
    let theirs_rc = dirtyfb(b);
    let theirs = theirs_rc < 0 && errno() == EACCES;
    let e = errno();
    drop_master(a);
    close(a);
    close(b);
    out(b"  master_present="); out(if mine { b"0" } else { b"!0" });
    out(b" other_present="); out_int(theirs_rc);
    out(b" errno="); out_int(e); out(b"\n");
    report(b"master_gates_present", grant && mine && theirs)
}

// ── 14-15. Master is scoped to the VT it was granted on ──────────────────────
//
// The half of the handoff that makes a switch mean something. A grant is armed
// only while the VT that was on screen when it was made is still on screen; a
// switch away suspends it with no write into the DRM layer at all, and a switch
// back re-arms it.
//
// These two run with no compositor anywhere, which is the point: the property is
// the kernel's, and measuring it through cosmic-comp would measure smithay's
// error handling at the same time.

/// Present, switch away, present, switch back, present.
unsafe fn t_master_follows_vt(tty0: c_int) -> bool {
    let fd = card0();
    if fd < 0 { return report(b"master_follows_vt", false); }
    let grant = set_master(fd) == 0;
    let on_vt1 = dirtyfb(fd) == 0;

    let moved = switch_to(tty0, 2) && active_vt(tty0) == 2;
    let bg_rc = dirtyfb(fd);
    let bg_errno = errno();
    let suspended = bg_rc < 0 && bg_errno == EACCES;

    let back = switch_to(tty0, 1) && active_vt(tty0) == 1;
    // Re-armed WITHOUT a second SET_MASTER. A compositor is not obliged to
    // re-assert master on resume, and one that does not must not come back to a
    // dead display with no console under it either — its VT is KD_GRAPHICS, so
    // nothing else would be drawing.
    let rearmed = dirtyfb(fd) == 0;

    drop_master(fd);
    close(fd);
    // Never judge before restoring VT 1: a FAIL that returns from VT 2 leaves
    // the rest of the suite printing onto a console nobody is looking at.
    out(b"  vt1="); out(if on_vt1 { b"0" } else { b"!0" });
    out(b" vt2="); out_int(bg_rc); out(b" errno="); out_int(bg_errno);
    out(b" vt1_again="); out(if rearmed { b"0" } else { b"!0" }); out(b"\n");
    report(b"master_follows_vt", grant && on_vt1 && moved && suspended && back && rearmed)
}

/// SET_MASTER from the holder while its VT is off screen is EACCES — and does
/// NOT move the grant to the VT now on screen. If it did, a background client
/// could take a console back just by asking.
unsafe fn t_setmaster_background(tty0: c_int) -> bool {
    let fd = card0();
    if fd < 0 { return report(b"setmaster_background", false); }
    let grant = set_master(fd) == 0;
    let moved = switch_to(tty0, 2) && active_vt(tty0) == 2;
    let rc = set_master(fd);
    let e = errno();
    let refused = rc < 0 && e == EACCES;
    // Still refused for presenting, i.e. the failed SET_MASTER did not re-arm.
    let still_suspended = dirtyfb(fd) < 0;
    let back = switch_to(tty0, 1) && active_vt(tty0) == 1;
    drop_master(fd);
    close(fd);
    out(b"  rc="); out_int(rc); out(b" errno="); out_int(e); out(b"\n");
    report(b"setmaster_background", grant && moved && refused && still_suspended && back)
}

// ── main ─────────────────────────────────────────────────────────────────────

unsafe fn arg_eq(argv: *mut *mut u8, i: isize, s: &[u8]) -> bool {
    let p = *argv.offset(i);
    if p.is_null() { return false; }
    for (k, &c) in s.iter().enumerate() {
        if *p.add(k) != c { return false; }
    }
    *p.add(s.len()) == 0
}

unsafe fn arg_num(argv: *mut *mut u8, i: isize) -> usize {
    let mut p = *argv.offset(i);
    let mut v = 0usize;
    if p.is_null() { return 0; }
    while *p >= b'0' && *p <= b'9' {
        v = v * 10 + (*p - b'0') as usize;
        p = p.add(1);
    }
    v
}

#[no_mangle]
pub unsafe extern "C" fn vt_main(argc: isize, argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    // O_RDWR, not O_RDONLY: the notification read and the console write both
    // go through this fd in the sub-commands.
    let tty0 = open(b"/dev/tty0\0".as_ptr(), O_RDWR);
    if tty0 < 0 {
        out(b"vttest: open /dev/tty0 failed, errno="); out_int(errno()); out(b"\n");
        return 1;
    }

    if argc > 1 {
        let rc = if arg_eq(argv, 1, b"hold") {
            let vt = if argc > 2 { arg_num(argv, 2) } else { 2 };
            let ms = if argc > 3 { arg_num(argv, 3) as i64 } else { 3000 };
            cmd_hold(tty0, vt, ms)
        } else if arg_eq(argv, 1, b"kbmode") {
            cmd_kbmode(tty0, argc > 2 && arg_eq(argv, 2, b"raw"))
        } else if arg_eq(argv, 1, b"gfx") {
            cmd_gfx(tty0, if argc > 2 { arg_num(argv, 2) as i64 } else { 3000 })
        } else {
            out(b"usage: vttest [hold <vt> <ms> | kbmode raw|xlate | gfx <ms>]\n");
            2
        };
        close(tty0);
        return rc;
    }

    out(b"vttest: virtual consoles\n");
    let mut passed = 0usize;
    let total = 17usize;

    if t_open_and_state(tty0) { passed += 1; }
    if t_activate(tty0) { passed += 1; }
    if t_activate_invalid(tty0) { passed += 1; }
    if t_modes_are_per_node() { passed += 1; }
    if t_vt_mode_roundtrip() { passed += 1; }
    if t_kb_mode(tty0) { passed += 1; }
    if t_openqry(tty0) { passed += 1; }
    if t_notify(tty0) { passed += 1; }
    if t_notify_per_open(tty0) { passed += 1; }
    if t_notify_epollet(tty0) { passed += 1; }
    if t_enotty_on_pipe() { passed += 1; }
    if t_graphics_mode(tty0) { passed += 1; }
    if t_master_is_exclusive() { passed += 1; }
    if t_drop_master_not_master() { passed += 1; }
    if t_master_gates_present() { passed += 1; }
    if t_master_follows_vt(tty0) { passed += 1; }
    if t_setmaster_background(tty0) { passed += 1; }

    // Whatever happened, leave the machine usable: VT 1, text mode, K_XLATE.
    // A suite that fails halfway through a KD_GRAPHICS subtest and stops there
    // hands back a console nobody can read.
    switch_to(tty0, 1);
    set_mode(tty0, KD_TEXT);
    ioctl(tty0, KDSKBMODE, K_XLATE as *mut c_void);

    out(b"vttest: "); out_num(passed); out(b"/"); out_num(total); out(b"\n");
    out(b"--- vttest done ---\n");
    close(tty0);
    (total - passed) as i32
}
