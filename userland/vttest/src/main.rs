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
//! * `vttest trap <ms>` — grab the keyboard, `K_RAW`, `KD_GRAPHICS`, then sleep
//!   and restore NOTHING. The deliberately unrecoverable machine that TODO.md
//!   item 20's escape hatch (Ctrl+Alt+Esc) has to rescue; if it comes back
//!   before `<ms>` elapses, the kernel brought it back.
//! * `vttest state` — print the active VT, console mode, keyboard mode and
//!   whether the keyboard is grabbable. What a rescue is checked against.
//!
//! Each check prints "<name>: PASS" or "<name>: FAIL"; the final line is
//! `vttest: <passed>/<total>` and the exit code is the failure count.

#![no_std]
#![no_main]
#![feature(c_variadic)]
#![allow(non_camel_case_types)]

#[path = "../../muslrt/rt.rs"]
mod rt;

use core::ffi::c_void;

type c_int = i32;
type c_ulong = u64;
type size_t = usize;
type ssize_t = isize;

const O_RDWR: c_int = 0o2;
const TCSETS: c_ulong = 0x5402;
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
const VT_RELDISP: c_ulong = 0x5605;
const VT_WAITACTIVE: c_ulong = 0x5607;

// KD* — <linux/kd.h>
const KDGKBTYPE: c_ulong = 0x4B33;
const KDSETMODE: c_ulong = 0x4B3A;
const KDGETMODE: c_ulong = 0x4B3B;
const KDGKBMODE: c_ulong = 0x4B44;
const KDSKBMODE: c_ulong = 0x4B45;
const KDGETLED: c_ulong = 0x4B31;
const KDSETLED: c_ulong = 0x4B32;
const KDGKBENT: c_ulong = 0x4B46;
const KDSKBENT: c_ulong = 0x4B47;
const KDGKBSENT: c_ulong = 0x4B48;
const KDSKBSENT: c_ulong = 0x4B49;
const KDKBDREP: c_ulong = 0x4B52;
const KDGKBMETA: c_ulong = 0x4B62;
const KDGKBLED: c_ulong = 0x4B64;
const KDSKBLED: c_ulong = 0x4B65;
const K_ESCPREFIX: c_int = 4;
const K_HOLE: u16 = 0x0200;
const K_NOSUCHMAP: u16 = 0x027f;
const ENOMEM: c_int = 12;

const KD_TEXT: usize = 0;
const KD_GRAPHICS: usize = 1;

const K_RAW: usize = 0;
const K_XLATE: usize = 1;
const K_MEDIUMRAW: usize = 2;
/// The boot default (Linux `vt.default_utf8=1`).
const K_UNICODE: usize = 3;

const VT_AUTO: u8 = 0;
const VT_PROCESS: u8 = 1;

// DRM — <drm/drm.h>. Only the three commands a master test needs. They live in
// the VT suite, not in `drmsmoke`, because what they measure is arbitration
// between a console and a display client: that is a VT property, and the
// harness that can switch VTs is this one.
const DRM_IOCTL_SET_MASTER: c_ulong = 0x0000_641E;
const DRM_IOCTL_DROP_MASTER: c_ulong = 0x0000_641F;
const DRM_IOCTL_MODE_DIRTYFB: c_ulong = 0xC018_64B1;

// EVIOCGRAB — `_IOW('E', 0x90, int)`. The int goes by VALUE, not by pointer:
// userspace calls `ioctl(fd, EVIOCGRAB, 1)`.
const EVIOCGRAB: c_ulong = 0x4004_4590;
// EVIOCREVOKE — `_IOW('E', 0x91, int)`, int by value and required to be 0.
const EVIOCREVOKE: c_ulong = 0x4004_4591;
// EVIOCGVERSION — `_IOR('E', 0x01, int)`.
const EVIOCGVERSION: c_ulong = 0x8004_4501;
const ENODEV: c_int = 19;
const O_NOCTTY: c_int = 0o400;
const TCGETS: c_ulong = 0x5401;
const TIOCGWINSZ: c_ulong = 0x5413;
const POLLERR: i16 = 0x008;
const POLLHUP: i16 = 0x010;

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

    pub fn open(path: *const u8, oflag: c_int, ...) -> c_int;
    pub fn read(fd: c_int, buf: *mut c_void, count: size_t) -> ssize_t;
    pub fn write(fd: c_int, buf: *const c_void, count: size_t) -> ssize_t;
    pub fn close(fd: c_int) -> c_int;
    pub fn lseek(fd: c_int, off: i64, whence: c_int) -> i64;
    pub fn kill(pid: c_int, sig: c_int) -> c_int;
    pub fn ioctl(fd: c_int, request: c_ulong, out: *mut c_void) -> c_int;
    pub fn pipe(fds: *mut c_int) -> c_int;
    pub fn poll(fds: *mut pollfd, nfds: u64, timeout: c_int) -> c_int;
    pub fn epoll_create1(flags: c_int) -> c_int;
    pub fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut epoll_event) -> c_int;
    pub fn epoll_wait(epfd: c_int, events: *mut epoll_event, maxevents: c_int, timeout: c_int) -> c_int;
    pub fn nanosleep(rqtp: *const timespec, rmtp: *mut timespec) -> c_int;
    pub fn exit(status: c_int) -> !;
    pub fn getpid() -> c_int;
    pub fn __errno_location() -> *mut c_int;
    pub fn readlink(path: *const u8, buf: *mut u8, len: size_t) -> ssize_t;
    pub fn fork() -> c_int;
    pub fn signal(sig: c_int, handler: usize) -> usize;
    pub fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
}

// ── Entry point (identical shape to ptytest's) ───────────────────────────────



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
    { // One write: a kernel console line must not splice into the verdict.
        let mut vline = [0u8; 160];
        let vn = name.len().min(140);
        vline[..vn].copy_from_slice(&name[..vn]);
        let vsuf: &[u8] = if passed { b": PASS\n" } else { b": FAIL\n" };
        vline[vn..vn + 7].copy_from_slice(vsuf);
        out(&vline[..vn + 7]); }
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
    // Restore first, judge after: a text mode (K_UNICODE by default, K_XLATE
    // if someone set it) is what keeps the console tap fed, and a FAIL that
    // also left the keyboard raw would take the machine's console down with it.
    let text_start = start == K_UNICODE as c_int || start == K_XLATE as c_int;
    let restore = if text_start { start as usize } else { K_UNICODE };
    ioctl(tty0, KDSKBMODE, restore as *mut c_void);
    let back = kb_mode(tty0);
    let bad = ioctl(tty0, KDSKBMODE, 9usize as *mut c_void);
    report(b"kb_mode_roundtrip",
           type_ok && text_start && set_ok
                   && mid == K_MEDIUMRAW as c_int && back == restore as c_int
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

/// A present issued while a `VT_PROCESS` release is outstanding is refused.
///
/// This is the window the VT-switch flash lived in: `VT_ACTIVATE` sends the
/// release signal but the display has not moved (`ACTIVE` still names the
/// outgoing VT) until `VT_RELDISP(1)`, so a compositor that flips from inside
/// its release handler was still master and reached the scanout — and the flip
/// that lands just before the handover is what shows over the incoming console.
/// The probe is the same DIRTYFB-with-no-framebuffer as the other master tests:
/// permitted returns 0 and touches nothing, so only the permission moves. A
/// refused release (`VT_RELDISP(0)`) must give master back.
unsafe fn t_present_in_release_window(tty0: c_int) -> bool {
    const SIGUSR1: c_int = 10;
    const SIG_IGN: usize = 1;
    let fd = card0();
    let t1 = open(b"/dev/tty1\0".as_ptr(), O_RDWR);
    if fd < 0 || t1 < 0 || active_vt(tty0) != 1 {
        if fd >= 0 { close(fd); }
        if t1 >= 0 { close(t1); }
        return report(b"present_in_release_window", false);
    }
    // Both the release (USR1) and acquire (USR2) signals must be ignored: the
    // default action of either kills the test.
    let old = signal(SIGUSR1, SIG_IGN);
    let old2 = signal(12, SIG_IGN);
    let grant = set_master(fd) == 0;
    let before = dirtyfb(fd) == 0;
    let pm = vt_mode { mode: VT_PROCESS, waitv: 0, relsig: 10, acqsig: 12, frsig: 0, _guard: 0 };
    let armed = ioctl(t1, VT_SETMODE, &pm as *const vt_mode as *mut c_void) == 0;

    // Refused release first: the display must not move and master must return.
    let req = ioctl(tty0, VT_ACTIVATE, 2usize as *mut c_void) == 0;
    let still1 = active_vt(tty0) == 1;
    let in_rc = dirtyfb(fd);
    let in_errno = errno();
    let refused_in_window = in_rc < 0 && in_errno == EACCES;
    let nack = ioctl(t1, VT_RELDISP, 0usize as *mut c_void) == 0;
    let back_after_nack = dirtyfb(fd) == 0;

    // Granted release: refused during the window and after the switch.
    let req2 = ioctl(tty0, VT_ACTIVATE, 2usize as *mut c_void) == 0;
    let win2 = dirtyfb(fd) < 0 && errno() == EACCES;
    let grant_rel = ioctl(t1, VT_RELDISP, 1usize as *mut c_void) == 0;
    let moved = ioctl(tty0, VT_WAITACTIVE, 2usize as *mut c_void) == 0 && active_vt(tty0) == 2;
    let bg = dirtyfb(fd) < 0 && errno() == EACCES;

    // Return: switch_to completes at once (VT 2 is VT_AUTO) and asks VT 1 to
    // acknowledge the acquire.
    let ret = switch_to(tty0, 1);
    let ack = ioctl(t1, VT_RELDISP, 2usize as *mut c_void) == 0;
    let auto = vt_mode { mode: VT_AUTO, waitv: 0, relsig: 0, acqsig: 0, frsig: 0, _guard: 0 };
    ioctl(t1, VT_SETMODE, &auto as *const vt_mode as *mut c_void);
    let rearmed = dirtyfb(fd) == 0;

    drop_master(fd);
    close(fd);
    close(t1);
    signal(SIGUSR1, old);
    signal(12, old2);
    out(b"  in_window_rc="); out_int(in_rc); out(b" errno="); out_int(in_errno); out(b"\n");
    report(b"present_in_release_window",
           grant && before && armed && req && still1 && refused_in_window && nack
           && back_after_nack && req2 && win2 && grant_rel && moved && bg
           && ret && ack && rearmed)
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

/// One master per VT. A session holding master on VT 1 must not lock VT 2's
/// display: a second open gets master there, each presents only on its own
/// VT, and neither steals the other's slot. Before this the grant was one per
/// device: a KMS program on a text VT (DOOM) holding it made the desktop's
/// SET_MASTER EBUSY on the switch back, and a compositor that kept its
/// suspended grant made every present on the text VT EACCES.
unsafe fn t_master_per_vt(tty0: c_int) -> bool {
    let a = card0();
    let b = card0();
    if a < 0 || b < 0 {
        if a >= 0 { close(a); }
        if b >= 0 { close(b); }
        return report(b"master_per_vt", false);
    }
    let a_grant = set_master(a) == 0;                       // VT 1: a
    let moved = switch_to(tty0, 2) && active_vt(tty0) == 2;
    let b_grant = set_master(b) == 0;                       // VT 2: b, not EBUSY
    let b_present = dirtyfb(b) == 0;
    let a_bg = dirtyfb(a) < 0 && errno() == EACCES;         // a suspended on VT 2
    let back = switch_to(tty0, 1) && active_vt(tty0) == 1;
    let a_present = dirtyfb(a) == 0;                        // a re-armed on VT 1
    let b_bg = dirtyfb(b) < 0 && errno() == EACCES;         // b suspended on VT 1
    let b_steal = set_master(b);                            // b holds VT 2: EACCES
    let b_steal_e = errno();
    drop_master(a);
    drop_master(b);
    close(a);
    close(b);
    out(b"  a_grant="); out(if a_grant { b"1" } else { b"0" });
    out(b" b_grant="); out(if b_grant { b"1" } else { b"0" });
    out(b" b_present="); out(if b_present { b"1" } else { b"0" });
    out(b" a_bg="); out(if a_bg { b"1" } else { b"0" });
    out(b" a_present="); out(if a_present { b"1" } else { b"0" });
    out(b" b_bg="); out(if b_bg { b"1" } else { b"0" });
    out(b" b_steal="); out_int(b_steal); out(b" errno="); out_int(b_steal_e); out(b"\n");
    report(b"master_per_vt", a_grant && moved && b_grant && b_present && a_bg && back
        && a_present && b_bg && b_steal < 0 && b_steal_e == EACCES)
}

// ── 18-21. EVIOCGRAB ─────────────────────────────────────────────────────────
//
// The grab was accepted and ignored until TODO.md item 20 was implemented, and
// enforcing it is the one change in this tree whose failure mode is a machine
// nobody can ask anything. These subtests measure the three properties that
// bound that risk, and every one of them is about a grab ENDING rather than a
// grab working:
//
//   * it is exclusive while it is held (or it is not a grab),
//   * it does not survive a switch away from the VT that took it,
//   * it does not survive the fd that took it.
//
// They are deliberately written against the ioctl surface alone. Exclusive
// DELIVERY needs injected events and therefore a host, which is what the
// `evsplit` runs measure; what a self-contained suite can prove is that the
// grab is released on every path that must release it — and that is the half
// where a bug leaves a dead machine.

unsafe fn kbd() -> c_int {
    open(b"/dev/input/event0\0".as_ptr(), O_RDWR | O_NONBLOCK)
}

unsafe fn grab(fd: c_int) -> c_int { ioctl(fd, EVIOCGRAB, 1usize as *mut c_void) }
unsafe fn ungrab(fd: c_int) -> c_int { ioctl(fd, EVIOCGRAB, core::ptr::null_mut()) }

/// Grab, release, and release again. The second release must be refused: an
/// ungrab that answers 0 to a caller holding nothing cannot be distinguished
/// from one that silently dropped somebody else's grab.
unsafe fn t_grab_roundtrip() -> bool {
    let fd = kbd();
    if fd < 0 { return report(b"grab_roundtrip", false); }
    let took = grab(fd) == 0;
    let gave = ungrab(fd) == 0;
    let again = ungrab(fd);
    let e = errno();
    close(fd);
    out(b"  took="); out_int(took as c_int);
    out(b" again="); out_int(again); out(b" errno="); out_int(e); out(b"\n");
    report(b"grab_roundtrip", took && gave && again < 0 && e == EINVAL)
}

/// Two opens, one node. The second grab is EBUSY while the first holds, and
/// succeeds the moment it does not.
unsafe fn t_grab_is_exclusive() -> bool {
    let a = kbd();
    let b = kbd();
    if a < 0 || b < 0 { return report(b"grab_is_exclusive", false); }
    let took = grab(a) == 0;
    let denied = grab(b);
    let e = errno();
    let released = ungrab(a) == 0;
    let now_ok = grab(b) == 0;
    ungrab(b);
    close(a);
    close(b);
    out(b"  took="); out_int(took as c_int);
    out(b" denied="); out_int(denied); out(b" errno="); out_int(e);
    out(b" after_release="); out_int(now_ok as c_int); out(b"\n");
    report(b"grab_is_exclusive", took && denied < 0 && e == EBUSY && released && now_ok)
}

/// **The escape hatch's automated half.** A grab is scoped to the VT that took
/// it: switching away drops it, and the holder is not merely suspended.
///
/// Proved two ways at once, because either alone is weak. A second open can
/// grab the node afterwards (so the grab is really gone, not dormant), and the
/// original holder's ungrab is refused with EINVAL (so it is gone from the
/// holder's point of view too, rather than having been quietly handed over).
unsafe fn t_grab_dies_on_vt_switch(tty0: c_int) -> bool {
    let a = kbd();
    let b = kbd();
    if a < 0 || b < 0 { return report(b"grab_dies_on_vt_switch", false); }
    let took = grab(a) == 0;
    let away = switch_to(tty0, 2) && active_vt(tty0) == 2;
    let back = switch_to(tty0, 1) && active_vt(tty0) == 1;
    let others_turn = grab(b) == 0;
    let holder_has_nothing = ungrab(a);
    let e = errno();
    ungrab(b);
    close(a);
    close(b);
    out(b"  took="); out_int(took as c_int);
    out(b" others_turn="); out_int(others_turn as c_int);
    out(b" holder_ungrab="); out_int(holder_has_nothing);
    out(b" errno="); out_int(e); out(b"\n");
    report(b"grab_dies_on_vt_switch",
           took && away && back && others_turn && holder_has_nothing < 0 && e == EINVAL)
}

/// A grab does not outlive the fd that took it. This is the path a crashed
/// client takes, and without it the node stays grabbed by a process that no
/// longer exists — with no events flowing, so nothing would ever notice.
unsafe fn t_grab_dies_on_close() -> bool {
    let a = kbd();
    if a < 0 { return report(b"grab_dies_on_close", false); }
    let took = grab(a) == 0;
    close(a);
    let b = kbd();
    if b < 0 { return report(b"grab_dies_on_close", false); }
    let inherited = grab(b) == 0;
    ungrab(b);
    close(b);
    out(b"  took="); out_int(took as c_int);
    out(b" next_open_can_grab="); out_int(inherited as c_int); out(b"\n");
    report(b"grab_dies_on_close", took && inherited)
}

// ── 22. VT 2..6 are terminals of their own ───────────────────────────────────
//
// `/dev/ttyN` for N >= 2 is a text session with its own line discipline (the
// kernel is the master of a pty pair; servers/tty/src/vt.rs "Text sessions").
// What a getty and a shell need from it, read back through second syscalls:
// it is a tty (TCGETS), it names itself `/dev/ttyN` (what ttyname() reads),
// it knows the console's size, and the VT/KD set still addresses THAT VT.

// ── Tabs and double-width cells ──────────────────────────────────────────────
//
// Cursor position is observable without a framebuffer: ESC[6n answers from
// the VT's own text plane, which is what a repaint puts back on screen.

/// Every VT 2..6 runs a login (userland/init), blocked reading the very pty
/// this suite drives. A reader that wakes first steals bytes of our CPR reply
/// (the "lost ESC"), so stop the session leader while the test owns the VT.
/// Returns the stopped pid (0 = none) for [`login_resume`].
unsafe fn login_pause(fd: c_int) -> c_int {
    let mut sid: c_int = 0;
    if ioctl(fd, 0x5429, &mut sid as *mut c_int as *mut c_void) != 0 || sid <= 1 { return 0; }
    kill(sid, 19); // SIGSTOP
    sleep_ms(200);
    sid
}
unsafe fn login_resume(sid: c_int) { if sid > 1 { kill(sid, 18); } } // SIGCONT

unsafe fn cpr(fd: c_int, seq: &[u8]) -> (u32, u32) {
    // Drop anything left over from the previous query.
    let mut junk = [0u8; 32];
    while read(fd, junk.as_mut_ptr() as *mut c_void, junk.len()) > 0 {}
    write(fd, seq.as_ptr() as *const c_void, seq.len());
    write(fd, b"\x1b[6n".as_ptr() as *const c_void, 4);
    let mut buf = [0u8; 24];
    let mut n = 0usize;
    // Non-blocking fd: wait up to ~1.5 s for the reply, then take what is there.
    let mut spins = 0;
    while n < buf.len() && spins < 300 {
        let r = read(fd, buf.as_mut_ptr().add(n) as *mut c_void, buf.len() - n);
        if r > 0 {
            n += r as usize;
            if buf[..n].contains(&b'R') { break; }
        } else {
            spins += 1;
            sleep_ms(5);
        }
    }
    // ESC [ row ; col R — a reply that lost its leading ESC is a failure.
    if n < 2 || buf[0] != 0x1b || buf[1] != b'[' { return (0, 0); }
    let (mut row, mut col, mut in_col) = (0u32, 0u32, false);
    for &b in &buf[..n] {
        match b {
            b'0'..=b'9' => { let d = (b - b'0') as u32; if in_col { col = col * 10 + d } else { row = row * 10 + d } }
            b';' => in_col = true,
            _ => {}
        }
    }
    (row, col)
}

/// Regression: the CPR reply must always start with ESC. A pty read once
/// delivered it without the leading 0x1B now and then.
unsafe fn t_cpr_esc(tty0: c_int) -> bool {
    let mut free: c_int = -1;
    ioctl(tty0, VT_OPENQRY, &mut free as *mut c_int as *mut c_void);
    if !(2..=6).contains(&free) { free = 6; }
    let path = [b'/', b'd', b'e', b'v', b'/', b't', b't', b'y', b'0' + free as u8, 0];
    let fd = open(path.as_ptr(), O_RDWR | O_NOCTTY | O_NONBLOCK);
    if fd < 0 { out(b"  open failed\n"); return report(b"cpr_esc", false); }
    let mut tio = [0u8; 64];
    ioctl(fd, TCGETS, tio.as_mut_ptr() as *mut c_void);
    for i in 0..4 { if i != 2 { for k in 0..4 { tio[i * 4 + k] = 0; } } }
    tio[12..16].fill(0);
    ioctl(fd, TCSETS, tio.as_mut_ptr() as *mut c_void);
    let sid = login_pause(fd);
    let (mut bad, mut empty) = (0u32, 0u32);
    for _ in 0..200 {
        let mut junk = [0u8; 32];
        let mut dn = 0usize; loop { let r = read(fd, junk.as_mut_ptr() as *mut c_void, junk.len()); if r <= 0 { break; } dn += r as usize; }
        write(fd, b"\x1b[6n".as_ptr() as *const c_void, 4);
        let mut buf = [0u8; 24];
        let mut n = 0usize;
        let mut spins = 0; let mut chunks = 0usize; let mut c0 = 0usize;
        while n < buf.len() && spins < 300 {
            let r = read(fd, buf.as_mut_ptr().add(n) as *mut c_void, buf.len() - n);
            if r > 0 { if chunks == 0 { c0 = r as usize; } chunks += 1; n += r as usize; if buf[..n].contains(&b'R') { break; } }
            else { spins += 1; sleep_ms(5); }
        }
        if n == 0 { empty += 1; } else if buf[0] != 0x1b { bad += 1; if bad == 1 { out(b"  drained="); out_num(dn); out(b" chunks="); out_num(chunks); out(b" c0="); out_num(c0); out(b" spins="); out_num(spins as usize); out(b" first byte="); out_num(buf[0] as usize); out(b" n="); out_num(n); out(b"\n"); } }
    }
    login_resume(sid);
    close(fd);
    out(b"  cpr_esc sid="); out_num(sid as usize); out(b" bad="); out_num(bad as usize); out(b" empty="); out_num(empty as usize); out(b"\n");
    report(b"cpr_esc", bad == 0 && empty == 0)
}

unsafe fn t_tabs_wide(tty0: c_int) -> bool {
    // The VT's login is stopped below, or it would eat the reply.
    let mut free: c_int = -1;
    ioctl(tty0, VT_OPENQRY, &mut free as *mut c_int as *mut c_void);
    if !(2..=6).contains(&free) { free = 6; }
    let path = [b'/', b'd', b'e', b'v', b'/', b't', b't', b'y', b'0' + free as u8, 0];
    let fd = open(path.as_ptr(), O_RDWR | O_NOCTTY | O_NONBLOCK);
    if fd < 0 { out(b"  open failed\n"); return report(b"tabs_wide", false); }
    let sid = login_pause(fd);
    // Raw, so the CPR reply is delivered without a newline.
    let mut tio = [0u8; 64];
    ioctl(fd, TCGETS, tio.as_mut_ptr() as *mut c_void);
    for i in 0..4 { if i != 2 { for k in 0..4 { tio[i * 4 + k] = 0; } } }
    tio[12..16].fill(0);
    ioctl(fd, TCSETS, tio.as_mut_ptr() as *mut c_void);
    let reset = b"\x1b[2J\x1b[H";
    // HT from col 3 lands on the stop at col 9 (1-based); from a stop, the next.
    write(fd, reset.as_ptr() as *const c_void, reset.len());
    let tab1 = cpr(fd, b"ab\t") == (1, 9);
    let tab2 = cpr(fd, b"\t") == (1, 17);
    // CHT 2 from col 1, then CBT 1 back one stop.
    write(fd, reset.as_ptr() as *const c_void, reset.len());
    let cht = cpr(fd, b"\x1b[2I") == (1, 17);
    let cbt = cpr(fd, b"\x1b[Z") == (1, 9);
    // U+4E2D (CJK, East-Asian Wide) takes two cells; U+00E9 takes one.
    write(fd, reset.as_ptr() as *const c_void, reset.len());
    let wide = cpr(fd, "\u{4e2d}".as_bytes()) == (1, 3);
    write(fd, reset.as_ptr() as *const c_void, reset.len());
    let narrow = cpr(fd, "\u{e9}".as_bytes()) == (1, 2);
    // Emoji (U+1F600) is wide too; a wide glyph that cannot fit wraps whole.
    write(fd, reset.as_ptr() as *const c_void, reset.len());
    let emoji = cpr(fd, "\u{1f600}".as_bytes()) == (1, 3);
    write(fd, reset.as_ptr() as *const c_void, reset.len());
    let wrap = cpr(fd, b"\x1b[1;1H\x1b[999C\xe4\xb8\xad") == (2, 3);
    // Mixed run: a(1) + wide(2) + b(1) = 4 cells, cursor at col 5.
    write(fd, reset.as_ptr() as *const c_void, reset.len());
    let mixed = cpr(fd, "a\u{6f22}b".as_bytes()) == (1, 5);
    // Combining marks never take a cell: e+U+0301 is one cell, a+U+0300+U+0323
    // (unknown/unprecomposed marks) one cell, CJK + mark still two.
    write(fd, reset.as_ptr() as *const c_void, reset.len());
    let comb1 = cpr(fd, "e\u{301}".as_bytes()) == (1, 2);
    write(fd, reset.as_ptr() as *const c_void, reset.len());
    let comb2 = cpr(fd, "a\u{300}\u{323}b".as_bytes()) == (1, 3);
    write(fd, reset.as_ptr() as *const c_void, reset.len());
    let comb3 = cpr(fd, "\u{6f22}\u{301}".as_bytes()) == (1, 3);
    write(fd, reset.as_ptr() as *const c_void, reset.len());
    login_resume(sid);
    close(fd);
    out(b" mixed="); out_int(mixed as c_int); out(b" comb="); out_int((comb1 && comb2 && comb3) as c_int);
    out(b"  tab1="); out_int(tab1 as c_int); out(b" tab2="); out_int(tab2 as c_int);
    out(b" cht="); out_int(cht as c_int); out(b" cbt="); out_int(cbt as c_int);
    out(b" wide="); out_int(wide as c_int); out(b" narrow="); out_int(narrow as c_int);
    out(b" emoji="); out_int(emoji as c_int); out(b" wrap="); out_int(wrap as c_int); out(b"\n");
    report(b"tabs_wide", tab1 && tab2 && cht && cbt && wide && narrow && emoji && wrap && mixed && comb1 && comb2 && comb3)
}

unsafe fn t_session_node(tty0: c_int) -> bool {
    let fd = open(b"/dev/tty5\0".as_ptr(), O_RDWR | O_NOCTTY);
    if fd < 0 {
        out(b"  open errno="); out_int(errno()); out(b"\n");
        return report(b"session_node", false);
    }
    let mut tio = [0u8; 64];
    let is_tty = ioctl(fd, TCGETS, tio.as_mut_ptr() as *mut c_void) == 0;
    // /proc/self/fd/<fd>
    let mut path = *b"/proc/self/fd/\0\0\0\0";
    let mut n = 14;
    let mut v = fd as usize;
    let mut d = [0u8; 3]; let mut k = 0;
    loop { d[k] = b'0' + (v % 10) as u8; v /= 10; k += 1; if v == 0 || k == 3 { break; } }
    while k > 0 { k -= 1; path[n] = d[k]; n += 1; }
    path[n] = 0;
    let mut name = [0u8; 32];
    let nl = readlink(path.as_ptr(), name.as_mut_ptr(), name.len());
    let named = nl == 9 && &name[..9] == b"/dev/tty5";
    let mut ws = [0u16; 4];
    let sized = ioctl(fd, TIOCGWINSZ, ws.as_mut_ptr() as *mut c_void) == 0 && ws[0] > 0 && ws[1] > 0;
    // VT/KD on this fd: VT_GETSTATE answers the machine's state, KDGETMODE
    // this VT's mode.
    let mut st = vt_stat { v_active: 0, v_signal: 0, v_state: 0 };
    let vt_ioctl = ioctl(fd, VT_GETSTATE, &mut st as *mut vt_stat as *mut c_void) == 0
        && st.v_active as c_int == active_vt(tty0)
        && (st.v_state & (1 << 5)) != 0;
    let kd = get_mode(fd) == KD_TEXT as c_int;
    let msg: &[u8] = b"\r\n[vttest wrote this on tty5]\r\n";
    let wrote = write(fd, msg.as_ptr() as *const c_void, msg.len()) == msg.len() as isize;
    close(fd);
    out(b"  tty="); out_int(is_tty as c_int);
    out(b" readlink="); out(&name[..if nl > 0 { nl as usize } else { 0 }]);
    out(b" winsize="); out_int(ws[0] as c_int); out(b"x"); out_int(ws[1] as c_int);
    out(b" vt_ioctl="); out_int(vt_ioctl as c_int); out(b" kd="); out_int(kd as c_int);
    out(b" write="); out_int(wrote as c_int); out(b"\n");
    report(b"session_node", is_tty && named && sized && vt_ioctl && kd && wrote)
}

// ── 23-24. EVIOCREVOKE ───────────────────────────────────────────────────────
//
// What seatd and logind do to a session's input devices when they switch away
// from it: the open is dead for good — reads, ioctls and writes ENODEV, poll
// POLLERR|POLLHUP — while every OTHER open of the node is untouched, and a grab
// the revoked open held is released.

unsafe fn t_revoke() -> bool {
    let a = kbd();
    let b = kbd();
    if a < 0 || b < 0 { return report(b"revoke", false); }
    let mut ver: c_int = 0;
    let a_live = ioctl(a, EVIOCGVERSION, &mut ver as *mut c_int as *mut c_void) == 0;
    let bad_arg = ioctl(a, EVIOCREVOKE, 1usize as *mut c_void);
    let bad_errno = errno();
    let revoked = ioctl(a, EVIOCREVOKE, core::ptr::null_mut()) == 0;
    let mut buf = [0u8; 24];
    let rd = read(a, buf.as_mut_ptr() as *mut c_void, 24);
    let rd_errno = errno();
    let io = ioctl(a, EVIOCGVERSION, &mut ver as *mut c_int as *mut c_void);
    let io_errno = errno();
    let mut p = pollfd { fd: a, events: POLLIN, revents: 0 };
    let pr = poll(&mut p as *mut pollfd, 1, 0);
    let hup = pr > 0 && (p.revents & (POLLERR | POLLHUP)) == (POLLERR | POLLHUP);
    let b_ok = ioctl(b, EVIOCGVERSION, &mut ver as *mut c_int as *mut c_void) == 0;
    close(a);
    close(b);
    out(b"  bad_arg="); out_int(bad_arg); out(b" errno="); out_int(bad_errno);
    out(b" read="); out_int(rd as c_int); out(b" errno="); out_int(rd_errno);
    out(b" ioctl="); out_int(io); out(b" errno="); out_int(io_errno);
    out(b" revents="); out_int(p.revents as c_int);
    out(b" other_open_ok="); out_int(b_ok as c_int); out(b"\n");
    report(b"revoke", a_live && bad_arg < 0 && bad_errno == EINVAL && revoked
        && rd < 0 && rd_errno == ENODEV && io < 0 && io_errno == ENODEV && hup && b_ok)
}

unsafe fn t_revoke_releases_grab() -> bool {
    let a = kbd();
    let b = kbd();
    if a < 0 || b < 0 { return report(b"revoke_releases_grab", false); }
    let took = grab(a) == 0;
    let blocked = grab(b) < 0 && errno() == EBUSY;
    let revoked = ioctl(a, EVIOCREVOKE, core::ptr::null_mut()) == 0;
    let freed = grab(b) == 0;
    // A revoked open can never grab again.
    ungrab(b);
    let regrab = grab(a);
    let e = errno();
    close(a);
    close(b);
    out(b"  took="); out_int(took as c_int); out(b" blocked="); out_int(blocked as c_int);
    out(b" freed="); out_int(freed as c_int);
    out(b" regrab="); out_int(regrab); out(b" errno="); out_int(e); out(b"\n");
    report(b"revoke_releases_grab", took && blocked && revoked && freed && regrab < 0 && e == ENODEV)
}

// ── `trap` — the worst client we can build ───────────────────────────────────

/// Grab the keyboard, take the VT out of `K_XLATE`, take the console into
/// `KD_GRAPHICS`, and then do nothing at all for `ms`.
///
/// This is not a test, it is the MACHINE the escape hatch has to rescue, and it
/// is built to be exactly as bad as the design argument says it can get:
///
/// * `EVIOCGRAB` means no other evdev reader — the console tap included — sees
///   a keystroke.
/// * `K_RAW` means `chord_key` disables itself, so **Ctrl+Alt+Fn does nothing**.
///   That is deliberate and matches Linux, and it is what makes the chord an
///   insufficient escape on its own.
/// * `KD_GRAPHICS` means the framebuffer console is gated off, so even handing
///   the keyboard back would leave nothing on screen to type at.
///
/// Nothing this process does after the setup can help: it never restores
/// anything and it never exits early. If the machine comes back before `ms`
/// elapses, something in the kernel brought it back.
unsafe fn cmd_trap(tty0: c_int, ms: i64) -> c_int {
    let fd = kbd();
    if fd < 0 {
        out(b"vttest trap: open /dev/input/event0 failed\n");
        return 1;
    }
    if grab(fd) != 0 {
        out(b"vttest trap: EVIOCGRAB failed, errno="); out_int(errno()); out(b"\n");
        return 1;
    }
    ioctl(tty0, KDSKBMODE, K_RAW as *mut c_void);
    set_mode(tty0, KD_GRAPHICS);
    // The pid is part of the instrument, not decoration: half of what this
    // exists to demonstrate is that KILLING the grabbing client frees the node,
    // and a trap that does not say which process to kill makes the tester guess
    // — which, with every `vttest state` probe also registering an evdev queue,
    // is a guess that silently picks the wrong pid.
    out(b"=== TRAPPED === pid="); out_int(getpid());
    out(b" grab held, K_RAW, KD_GRAPHICS; no chord can reach me\n");
    sleep_ms(ms);
    // Reached only if the rescue never came. Say so, and put the machine back
    // by hand so a failed run does not need a reboot.
    out(b"=== TRAP EXPIRED === the rescue did NOT arrive within the hold\n");
    ungrab(fd);
    close(fd);
    ioctl(tty0, KDSKBMODE, K_UNICODE as *mut c_void);
    set_mode(tty0, KD_TEXT);
    0
}

/// Report what a rescue is supposed to have restored: the active VT, the
/// console mode, the keyboard mode, and whether the keyboard can be grabbed
/// (i.e. nobody is still holding it).
unsafe fn cmd_state(tty0: c_int) -> c_int {
    let fd = kbd();
    let can_grab = fd >= 0 && grab(fd) == 0;
    if can_grab { ungrab(fd); }
    if fd >= 0 { close(fd); }
    out(b"vttest state: vt="); out_int(active_vt(tty0));
    out(b" kdmode="); out_int(get_mode(tty0));
    out(b" kbmode="); out_int(kb_mode(tty0));
    out(b" grabbable="); out_int(can_grab as c_int);
    out(b"\n");
    0
}

// ── 25. KD_GRAPHICS dies with the process that set it ────────────────────────
//
// On Linux the session manager restores KD_TEXT when a compositor dies; here
// the seat manager lives inside the compositor (the libseat shim), so the
// kernel does it. A child takes /dev/tty3 (off screen: no pixels move) into
// KD_GRAPHICS, reports while alive, and exits WITHOUT restoring.

unsafe fn t_kd_graphics_owner_death() -> bool {
    let mut p: [c_int; 2] = [-1, -1];
    if pipe(p.as_mut_ptr()) != 0 { return report(b"kd_graphics_owner_death", false); }
    let pid = fork();
    if pid == 0 {
        close(p[0]);
        let t3 = open(b"/dev/tty3\0".as_ptr(), O_RDWR | O_NOCTTY);
        let ok = t3 >= 0 && set_mode(t3, KD_GRAPHICS) == 0;
        let b: u8 = if ok { b'1' } else { b'0' };
        write(p[1], &b as *const u8 as *const c_void, 1);
        sleep_ms(300);
        exit(0); // no KD_TEXT, no close: the kernel must notice the death
    }
    close(p[1]);
    let mut b: u8 = 0;
    let got = read(p[0], &mut b as *mut u8 as *mut c_void, 1) == 1 && b == b'1';
    close(p[0]);
    let t3 = open(b"/dev/tty3\0".as_ptr(), O_RDWR | O_NOCTTY);
    let alive = get_mode(t3);
    let mut st: c_int = 0;
    let reaped = pid > 0 && waitpid(pid, &mut st, 0) == pid;
    let dead = get_mode(t3);
    if dead != KD_TEXT as c_int { set_mode(t3, KD_TEXT); }
    close(t3);
    out(b"  while_alive="); out_int(alive); out(b" after_exit="); out_int(dead); out(b"\n");
    report(b"kd_graphics_owner_death",
           got && reaped && alive == KD_GRAPHICS as c_int && dead == KD_TEXT as c_int)
}

// ── 26. KD_GRAPHICS is a property of the VT: it survives switching away ──────

unsafe fn t_kd_graphics_survives_switch(tty0: c_int) -> bool {
    let t4 = open(b"/dev/tty4\0".as_ptr(), O_RDWR | O_NOCTTY);
    if t4 < 0 { return report(b"kd_graphics_survives_switch", false); }
    let set_ok = set_mode(t4, KD_GRAPHICS) == 0;
    // Through VT 4 and back: a switch into a graphical VT must not reset it,
    // nor repaint its text (the flash on return to a compositor).
    let to4 = switch_to(tty0, 4);
    let on4 = get_mode(t4);
    let back = switch_to(tty0, 1);
    let after = get_mode(t4);
    set_mode(t4, KD_TEXT);
    close(t4);
    out(b"  on_vt4="); out_int(on4); out(b" after_return="); out_int(after); out(b"\n");
    report(b"kd_graphics_survives_switch",
           set_ok && to4 && back && on4 == KD_GRAPHICS as c_int && after == KD_GRAPHICS as c_int)
}

// ── 27. KDKBDREP: autorepeat delay/period ────────────────────────────────────

#[repr(C)]
struct kbd_repeat { delay: c_int, period: c_int }

unsafe fn t_kbdrep() -> bool {
    let t2 = open(b"/dev/tty2\0".as_ptr(), O_RDWR | O_NOCTTY);
    if t2 < 0 { return report(b"kbdrep", false); }
    // Non-positive fields only read (Linux's kbd_rate).
    let mut cur = kbd_repeat { delay: -1, period: -1 };
    let get_ok = ioctl(t2, KDKBDREP, &mut cur as *mut kbd_repeat as *mut c_void) == 0;
    let mut set = kbd_repeat { delay: 500, period: 100 };
    let set_ok = ioctl(t2, KDKBDREP, &mut set as *mut kbd_repeat as *mut c_void) == 0;
    let mut chk = kbd_repeat { delay: 0, period: 0 };
    ioctl(t2, KDKBDREP, &mut chk as *mut kbd_repeat as *mut c_void);
    let mut restore = kbd_repeat { delay: cur.delay, period: cur.period };
    ioctl(t2, KDKBDREP, &mut restore as *mut kbd_repeat as *mut c_void);
    close(t2);
    out(b"  default="); out_int(cur.delay); out(b"/"); out_int(cur.period);
    out(b" set="); out_int(chk.delay); out(b"/"); out_int(chk.period); out(b"\n");
    report(b"kbdrep", get_ok && set_ok && cur.delay > 0 && cur.period > 0
                      && set.delay == 500 && set.period == 100
                      && chk.delay == 500 && chk.period == 100)
}

// ── 28. Keyboard defaults: K_UNICODE, ESC-prefix meta ────────────────────────

unsafe fn t_kb_defaults() -> bool {
    // VT 6 is never touched by this suite.
    let t6 = open(b"/dev/tty6\0".as_ptr(), O_RDWR | O_NOCTTY);
    if t6 < 0 { return report(b"kb_defaults", false); }
    let m = kb_mode(t6);
    let mut meta: c_int = -1;
    let meta_ok = ioctl(t6, KDGKBMETA, &mut meta as *mut c_int as *mut c_void) == 0;
    close(t6);
    out(b"  kbmode="); out_int(m); out(b" meta="); out_int(meta); out(b"\n");
    report(b"kb_defaults", m == K_UNICODE as c_int && meta_ok && meta == K_ESCPREFIX)
}

// ── 29. KDGKBENT / KDSKBENT ──────────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
struct kbentry { table: u8, index: u8, value: u16 }

unsafe fn kbent_get(fd: c_int, table: u8, index: u8) -> i32 {
    let mut e = kbentry { table, index, value: 0xffff };
    if ioctl(fd, KDGKBENT, &mut e as *mut kbentry as *mut c_void) != 0 { return -1; }
    e.value as i32
}

unsafe fn kbent_set(fd: c_int, table: u8, index: u8, value: u16) -> c_int {
    let mut e = kbentry { table, index, value };
    if ioctl(fd, KDSKBENT, &mut e as *mut kbentry as *mut c_void) != 0 { return errno(); }
    0
}

unsafe fn t_keymap_entries() -> bool {
    let t2 = open(b"/dev/tty2\0".as_ptr(), O_RDWR | O_NOCTTY);
    if t2 < 0 { return report(b"keymap_entries", false); }
    // Linux's defkeymap: KEY_A (30) is +a, Shift +A, Ctrl ^A; KEY_LEFTSHIFT
    // (42) is Shift; KEY_UP (103) is Up; KEY_F1 (59) is F1.
    let a0 = kbent_get(t2, 0, 30);
    let a1 = kbent_get(t2, 1, 30);
    let a4 = kbent_get(t2, 4, 30);
    let sh = kbent_get(t2, 0, 42);
    let up = kbent_get(t2, 0, 103);
    let f1 = kbent_get(t2, 0, 59);
    let defaults = a0 == 0x0b61 && a1 == 0x0b41 && a4 == 0x0001 && sh == 0x0700
                && up == 0x0603 && f1 == 0x0100;
    // Write, read back, restore. Only root may (EPERM otherwise).
    let w = kbent_set(t2, 0, 30, 0x0b62);
    let wb = kbent_get(t2, 0, 30);
    kbent_set(t2, 0, 30, 0x0b61);
    let restored = kbent_get(t2, 0, 30);
    // KVAL out of range for its type (KT_SHIFT max 8), and a table past ours.
    let bad = kbent_set(t2, 0, 30, 0x07ff);
    let nomem = kbent_set(t2, 200, 30, 0x0b61);
    // Deallocate keymap 15, observe it gone, then rebuild it as it was.
    let mut saved = [0u16; 256];
    for k in 0..256 { saved[k] = kbent_get(t2, 15, k as u8) as u16; }
    let d = kbent_set(t2, 15, 0, K_NOSUCHMAP);
    let gone0 = kbent_get(t2, 15, 0);
    let gone5 = kbent_get(t2, 15, 5);
    for k in 1..256 { kbent_set(t2, 15, k as u8, saved[k]); }
    let back = kbent_get(t2, 15, 30) as u16 == saved[30];
    close(t2);
    out(b"  a="); out_int(a0); out(b"/"); out_int(a1); out(b"/"); out_int(a4);
    out(b" write="); out_int(w); out(b" readback="); out_int(wb);
    out(b" bad="); out_int(bad); out(b" nomem="); out_int(nomem);
    out(b" dealloc="); out_int(d); out(b" gone="); out_int(gone0); out(b"/"); out_int(gone5);
    out(b"\n");
    report(b"keymap_entries",
           defaults && w == 0 && wb == 0x0b62 && restored == 0x0b61 && bad == EINVAL
                    && nomem == ENOMEM && d == 0 && gone0 == K_NOSUCHMAP as i32
                    && gone5 == K_HOLE as i32 && back)
}

// ── 30. Lock flags and LEDs (KDGKBLED/KDSKBLED, KDGETLED/KDSETLED) ───────────

unsafe fn t_kbled() -> bool {
    let t4 = open(b"/dev/tty4\0".as_ptr(), O_RDWR | O_NOCTTY);
    if t4 < 0 { return report(b"kbled", false); }
    let mut f0: u8 = 0xff;
    ioctl(t4, KDGKBLED, &mut f0 as *mut u8 as *mut c_void);
    // CapsLock flag on: the LEDs follow the flags.
    let s1 = ioctl(t4, KDSKBLED, 0x04usize as *mut c_void);
    let mut f1: u8 = 0; ioctl(t4, KDGKBLED, &mut f1 as *mut u8 as *mut c_void);
    let mut l1: u8 = 0; ioctl(t4, KDGETLED, &mut l1 as *mut u8 as *mut c_void);
    // KDSETLED overrides what the LEDs show, and a value above 7 hands them
    // back to the flags.
    ioctl(t4, KDSETLED, 0x01usize as *mut c_void);
    let mut l2: u8 = 0; ioctl(t4, KDGETLED, &mut l2 as *mut u8 as *mut c_void);
    ioctl(t4, KDSETLED, 0xffusize as *mut c_void);
    let mut l3: u8 = 0; ioctl(t4, KDGETLED, &mut l3 as *mut u8 as *mut c_void);
    let bad = ioctl(t4, KDSKBLED, 0x100usize as *mut c_void);
    let bad_e = errno();
    ioctl(t4, KDSKBLED, f0 as usize as *mut c_void);
    close(t4);
    out(b"  default="); out_num(f0 as usize); out(b" caps_flags="); out_num(f1 as usize);
    out(b" leds="); out_num(l1 as usize); out(b"/"); out_num(l2 as usize); out(b"/");
    out_num(l3 as usize); out(b"\n");
    // Default: NumLock on (flags 0x02, default 0x02 << 4).
    report(b"kbled", f0 == 0x22 && s1 == 0 && f1 == 0x04 && l1 == 0x04 && l2 == 0x01
                     && l3 == 0x04 && bad < 0 && bad_e == EINVAL)
}

// ── 31. Function-key strings (KDGKBSENT/KDSKBSENT) ───────────────────────────

#[repr(C)]
struct kbsentry { func: u8, string: [u8; 512] }

unsafe fn t_kbsent() -> bool {
    let t2 = open(b"/dev/tty2\0".as_ptr(), O_RDWR | O_NOCTTY);
    if t2 < 0 { return report(b"kbsent", false); }
    let mut e = kbsentry { func: 0, string: [0xAA; 512] };
    let g = ioctl(t2, KDGKBSENT, &mut e as *mut kbsentry as *mut c_void);
    let f1_ok = g == 0 && &e.string[..5] == b"\x1b[[A\0";
    let mut w = kbsentry { func: 40, string: [0; 512] };
    w.string[..3].copy_from_slice(b"xyz");
    let s = ioctl(t2, KDSKBSENT, &mut w as *mut kbsentry as *mut c_void);
    let mut r = kbsentry { func: 40, string: [0xAA; 512] };
    ioctl(t2, KDGKBSENT, &mut r as *mut kbsentry as *mut c_void);
    let rb = &r.string[..4] == b"xyz\0";
    let mut clr = kbsentry { func: 40, string: [0; 512] };
    ioctl(t2, KDSKBSENT, &mut clr as *mut kbsentry as *mut c_void);
    close(t2);
    report(b"kbsent", f1_ok && s == 0 && rb)
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

// ── Console emulation: tab stops and double-width cells ──────────────────────
//
// The per-VT text mirror (`servers/tty/src/vt.rs`) is the only readable copy of
// what a VT displays, and `/proc/vtstat` dumps its cursor and first rows as hex
// code points (`vtcur <vt> <col> <row>`, `vtrow <vt> <r>: ...`). A session's
// bytes go through the same emulator that draws the framebuffer, so asserting
// on the mirror asserts on where the console put each character.

const WIDE_CONT: u32 = 0x8000_0000;
static mut VTSTAT_BUF: [u8; 16384] = [0; 16384];

struct Snap { col: u32, row: u32, cells: [[u32; 24]; 4] }

fn parse_num(b: &[u8], i: &mut usize, radix: u32) -> u32 {
    let mut v = 0u32;
    while *i < b.len() {
        let d = match b[*i] { b'0'..=b'9' => b[*i] - b'0', b'a'..=b'f' => b[*i] - b'a' + 10, _ => break };
        if d as u32 >= radix { break; }
        v = v.wrapping_mul(radix).wrapping_add(d as u32);
        *i += 1;
    }
    v
}

/// Read `/proc/vtstat` and extract VT `vt`'s cursor and top rows.
unsafe fn snap(vt: u32) -> Option<Snap> {
    let fd = open(b"/proc/vtstat\0".as_ptr(), 0);
    if fd < 0 { return None; }
    let buf = &mut *core::ptr::addr_of_mut!(VTSTAT_BUF);
    let mut n = 0usize;
    loop {
        let r = read(fd, buf.as_mut_ptr().add(n) as *mut c_void, buf.len() - n);
        if r <= 0 { break; }
        n += r as usize;
    }
    close(fd);
    let mut s = Snap { col: 0, row: 0, cells: [[0; 24]; 4] };
    let mut seen_cur = false;
    for line in buf[..n].split(|&b| b == b'\n') {
        let mut i;
        if line.starts_with(b"vtcur ") {
            i = 6;
            if parse_num(line, &mut i, 10) != vt { continue; }
            i += 1; s.col = parse_num(line, &mut i, 10);
            i += 1; s.row = parse_num(line, &mut i, 10);
            seen_cur = true;
        } else if line.starts_with(b"vtrow ") {
            i = 6;
            if parse_num(line, &mut i, 10) != vt { continue; }
            i += 1; let r = parse_num(line, &mut i, 10) as usize;
            i += 1; // ':'
            if r >= 4 { continue; }
            for c in 0..24 { i += 1; s.cells[r][c] = parse_num(line, &mut i, 16); }
        }
    }
    if seen_cur { Some(s) } else { None }
}

/// Clear VT 6 (it is only ever used here), write `bytes`, give the renderer
/// time to drain them, and return the mirror.
unsafe fn emu(fd: c_int, bytes: &[u8]) -> Option<Snap> {
    let pre = b"\x1b[2J\x1b[H";
    write(fd, pre.as_ptr() as *const c_void, pre.len());
    write(fd, bytes.as_ptr() as *const c_void, bytes.len());
    sleep_ms(250);
    snap(6)
}

unsafe fn dump(tag: &[u8], s: &Option<Snap>) {
    out(b"  dbg "); out(tag);
    match s {
        Some(s) => {
            out(b" cur="); out_num(s.col as usize); out(b","); out_num(s.row as usize);
            out(b" r0:");
            for c in 0..14 { out(b" "); out_hex(s.cells[0][c]); }
            out(b" r1:");
            for c in 0..4 { out(b" "); out_hex(s.cells[1][c]); }
        }
        None => out(b" none"),
    }
    out(b"\n");
}

unsafe fn out_hex(v: u32) {
    let mut b = [0u8; 8];
    for i in 0..8 { let d = ((v >> (28 - 4 * i)) & 0xf) as u8; b[i] = if d < 10 { b'0' + d } else { b'a' + d - 10 }; }
    out(&b);
}

/// Put VT 6's tab stops back to every 8 columns (through col 80).
unsafe fn reset_tabs(fd: c_int) {
    let mut seq = [0u8; 160];
    let mut n = 0;
    let head = b"\x1b[3g";
    seq[..head.len()].copy_from_slice(head); n += head.len();
    let mut c = 9usize; // 1-based column 9 == index 8
    while c <= 81 {
        let s = [0x1b, b'[', b'0' + (c / 10) as u8, b'0' + (c % 10) as u8, b'G', 0x1b, b'H'];
        seq[n..n + 7].copy_from_slice(&s); n += 7;
        c += 8;
    }
    write(fd, seq.as_ptr() as *const c_void, n);
    sleep_ms(100);
}

unsafe fn emu2(fd: c_int, a: &[u8], b: &[u8]) -> Option<Snap> {
    let mut o = [0u8; 48];
    o[..a.len()].copy_from_slice(a);
    o[a.len()..a.len() + b.len()].copy_from_slice(b);
    emu(fd, &o[..a.len() + b.len()])
}

unsafe fn open_emu_vt() -> c_int { open(b"/dev/tty6\0".as_ptr(), O_RDWR | O_NOCTTY) }

unsafe fn t_emu_tab_default() -> bool {
    let fd = open_emu_vt();
    if fd < 0 { return report(b"emu_tab_default", false); }
    let s = emu(fd, b"a\tb\tc\x1b[2;1H1234567\tz");
    close(fd);
    let ok = match s {
        Some(s) => s.cells[0][0] == b'a' as u32 && s.cells[0][8] == b'b' as u32
            && s.cells[0][16] == b'c' as u32 && s.cells[1][8] == b'z' as u32
            && s.col == 9 && s.row == 1,
        None => false,
    };
    report(b"emu_tab_default", ok)
}

unsafe fn t_emu_tab_stops() -> bool {
    let fd = open_emu_vt();
    if fd < 0 { return report(b"emu_tab_stops", false); }
    // Clear all, then HTS at columns 4 and 11 (0-based).
    let s1 = emu(fd, b"\x1b[3g\x1b[5G\x1bH\x1b[12G\x1bH\r\tX\tY");
    // CHT from col 0 over both stops; CBT back from col 20.
    let s2 = emu(fd, b"\r\x1b[2I");
    let s3 = emu(fd, b"\x1b[21G\x1b[Z");
    let s4 = emu(fd, b"\x1b[21G\x1b[2Z");
    // TBC at the cursor: clear col 4, then a tab from 0 goes straight to 11.
    let s5 = emu(fd, b"\x1b[5G\x1b[g\r\tW");
    reset_tabs(fd);
    let s6 = emu(fd, b"\r\tq");
    close(fd);
    let c = |s: &Option<Snap>| s.as_ref().map(|s| (s.col, s.row));
    let ok1 = s1.as_ref().map_or(false, |s| s.cells[0][4] == b'X' as u32 && s.cells[0][11] == b'Y' as u32);
    let ok2 = c(&s2) == Some((11, 0));
    let ok3 = c(&s3) == Some((11, 0));
    let ok4 = c(&s4) == Some((4, 0));
    let ok5 = s5.as_ref().map_or(false, |s| s.cells[0][11] == b'W' as u32 && s.cells[0][4] == 0);
    let ok6 = s6.as_ref().map_or(false, |s| s.cells[0][8] == b'q' as u32);
    dump(b"s1", &s1); dump(b"s2", &s2); dump(b"s3", &s3); dump(b"s4", &s4); dump(b"s5", &s5); dump(b"s6", &s6);
    out(b"  hts="); out_int(ok1 as c_int); out(b" cht="); out_int(ok2 as c_int);
    out(b" cbt="); out_int((ok3 && ok4) as c_int); out(b" tbc="); out_int(ok5 as c_int);
    out(b" reset="); out_int(ok6 as c_int); out(b"\n");
    report(b"emu_tab_stops", ok1 && ok2 && ok3 && ok4 && ok5 && ok6)
}

unsafe fn t_emu_wide_advance() -> bool {
    let fd = open_emu_vt();
    if fd < 0 { return report(b"emu_wide_advance", false); }
    // A, CJK (2 cells), B, emoji, fullwidth 'a', e + combining acute (folds into
    // the precomposed e-acute in the cell before the cursor), e-acute.
    let s = emu(fd, "A\u{4e16}B\u{1F600}\u{ff41}e\u{301}\u{e9}".as_bytes());
    close(fd);
    dump(b"adv", &s);
    let ok = match s {
        Some(s) => {
            let r = &s.cells[0];
            r[0] == b'A' as u32 && r[1] == 0x4e16 && r[2] == WIDE_CONT && r[3] == b'B' as u32
                && r[4] == 0x1F600 && r[5] == WIDE_CONT && r[6] == 0xff41 && r[7] == WIDE_CONT
                && r[8] == 0xe9 && r[9] == 0xe9 && s.col == 10 && s.row == 0
        }
        None => false,
    };
    report(b"emu_wide_advance", ok)
}

unsafe fn t_emu_wide_overwrite() -> bool {
    let fd = open_emu_vt();
    if fd < 0 { return report(b"emu_wide_overwrite", false); }
    let a = "\u{4e16}\u{754c}".as_bytes();
    // Right half of the first character overwritten: the left half goes.
    let s1 = emu2(fd, a, b"\x1b[2Gx");
    // Left half of the second overwritten: its right half goes.
    let s2 = emu2(fd, a, b"\x1b[3Gz");
    // EL 0 from the right half of the second: the whole character goes.
    let s3 = emu2(fd, a, b"\x1b[4G\x1b[K");
    close(fd);
    dump(b"ow1", &s1); dump(b"ow2", &s2); dump(b"ow3", &s3);
    let ok1 = s1.as_ref().map_or(false, |s| {
        let r = &s.cells[0];
        r[0] == 0 && r[1] == b'x' as u32 && r[2] == 0x754c && r[3] == WIDE_CONT });
    let ok2 = s2.as_ref().map_or(false, |s| {
        let r = &s.cells[0];
        r[0] == 0x4e16 && r[1] == WIDE_CONT && r[2] == b'z' as u32 && r[3] == 0 });
    let ok3 = s3.as_ref().map_or(false, |s| {
        let r = &s.cells[0];
        r[0] == 0x4e16 && r[1] == WIDE_CONT && r[2] == 0 && r[3] == 0 });
    out(b"  right_half="); out_int(ok1 as c_int); out(b" left_half="); out_int(ok2 as c_int);
    out(b" erase="); out_int(ok3 as c_int); out(b"\n");
    report(b"emu_wide_overwrite", ok1 && ok2 && ok3)
}

unsafe fn t_emu_wide_wrap() -> bool {
    let fd = open_emu_vt();
    if fd < 0 { return report(b"emu_wide_wrap", false); }
    // Last column, then a double-width character: it wraps whole.
    let s = emu(fd, "\x1b[999C\u{4e16}".as_bytes());
    close(fd);
    dump(b"wrap", &s);
    let ok = match s {
        Some(s) => s.cells[1][0] == 0x4e16 && s.cells[1][1] == WIDE_CONT && s.col == 2 && s.row == 1,
        None => false,
    };
    report(b"emu_wide_wrap", ok)
}

// ── full-grid emulator tests (/proc/vtgrid) ───────────────────────────────────

const GRID_ROWS: usize = 72;
const GRID_COLS: usize = 256;
static mut GRID_BUF: [u8; 393216] = [0; 393216];
static mut GRID: [[u32; GRID_COLS]; GRID_ROWS] = [[0; GRID_COLS]; GRID_ROWS];
static mut GRIDBG: [[u8; GRID_COLS]; GRID_ROWS] = [[0; GRID_COLS]; GRID_ROWS];

struct GridCur { col: u32, row: u32 }

/// Read `/proc/vtgrid` (VT 6's whole cell grid) into `GRID`; a 0x20 cell is
/// normalised to blank (0) so a typed space equals an erased cell.
unsafe fn grid_read() -> Option<GridCur> {
    let fd = open(b"/proc/vtgrid\0".as_ptr(), 0);
    if fd < 0 { return None; }
    let buf = &mut *core::ptr::addr_of_mut!(GRID_BUF);
    let mut n = 0usize;
    loop {
        let r = read(fd, buf.as_mut_ptr().add(n) as *mut c_void, buf.len() - n);
        if r <= 0 { break; }
        n += r as usize;
    }
    close(fd);
    let g = &mut *core::ptr::addr_of_mut!(GRID);
    for r in 0..GRID_ROWS { for c in 0..GRID_COLS { g[r][c] = 0; } }
    let gb = &mut *core::ptr::addr_of_mut!(GRIDBG);
    for r in 0..GRID_ROWS { for c in 0..GRID_COLS { gb[r][c] = 0; } }
    let mut cur = GridCur { col: 0, row: 0 };
    let mut seen = false;
    for line in buf[..n].split(|&b| b == b'\n') {
        let mut i;
        if line.starts_with(b"vtgrid ") {
            i = 7;
            parse_num(line, &mut i, 10);
            i += 1; cur.col = parse_num(line, &mut i, 10);
            i += 1; cur.row = parse_num(line, &mut i, 10);
            seen = true;
        } else if line.starts_with(b"bg ") {
            i = 3;
            let r = parse_num(line, &mut i, 10) as usize;
            i += 1;
            let mut c = 0usize;
            while i < line.len() && c < GRID_COLS && r < GRID_ROWS {
                i += 1;
                gb[r][c] = parse_num(line, &mut i, 16) as u8;
                c += 1;
            }
        } else if line.starts_with(b"row ") {
            i = 4;
            let r = parse_num(line, &mut i, 10) as usize;
            i += 1; // ':'
            let mut c = 0usize;
            while i < line.len() && c < GRID_COLS && r < GRID_ROWS {
                i += 1;
                let v = parse_num(line, &mut i, 16);
                g[r][c] = if v == 0x20 { 0 } else { v };
                c += 1;
            }
        }
    }
    if seen { Some(cur) } else { None }
}

unsafe fn gcell(r: usize, c: usize) -> u32 { (*core::ptr::addr_of!(GRID))[r][c] }
unsafe fn gbg(r: usize, c: usize) -> u8 { (*core::ptr::addr_of!(GRIDBG))[r][c] }

/// Console size of VT 6 as (cols, rows).
unsafe fn emu_size(fd: c_int) -> (usize, usize) {
    let mut ws = [0u16; 4];
    if ioctl(fd, TIOCGWINSZ, ws.as_mut_ptr() as *mut c_void) == 0 && ws[0] > 0 && ws[1] > 0 {
        (ws[1] as usize, ws[0] as usize)
    } else { (80, 25) }
}

unsafe fn emu_grid(fd: c_int, bytes: &[u8]) -> Option<GridCur> {
    let pre = b"\x1bc\x1b[2J\x1b[H";
    write(fd, pre.as_ptr() as *const c_void, pre.len());
    write(fd, bytes.as_ptr() as *const c_void, bytes.len());
    sleep_ms(300);
    grid_read()
}

/// Writing the bottom-right cell must not scroll (deferred wrap): the cell
/// holds the character, the cursor stays on it, the screen did not move; the
/// next character then wraps to the line below.
unsafe fn t_emu_wrap_deferred() -> bool {
    let fd = open_emu_vt();
    if fd < 0 { return report(b"emu_wrap_deferred", false); }
    let (cols, rows) = emu_size(fd);
    let mut m = [0u8; 64];
    let mut n = 0usize;
    for b in b"TOP\x1b[" { m[n] = *b; n += 1; }
    n += fmt_num(&mut m[n..], rows); m[n] = b';'; n += 1;
    n += fmt_num(&mut m[n..], cols);
    for b in b"HX" { m[n] = *b; n += 1; }
    let cur = emu_grid(fd, &m[..n]);
    close(fd);
    let ok = match cur {
        Some(c) => gcell(0, 0) == b'T' as u32 && gcell(rows - 1, cols - 1) == b'X' as u32
            && c.col as usize == cols - 1 && c.row as usize == rows - 1,
        None => false,
    };
    if !ok { out(b"  wrap_deferred: cols="); out_num(cols); out(b" rows="); out_num(rows);
        out(b" top="); out_hex(gcell(0, 0)); out(b" last="); out_hex(gcell(rows - 1, cols - 1)); out(b"\n"); }
    report(b"emu_wrap_deferred", ok)
}

unsafe fn fmt_num(b: &mut [u8], mut v: usize) -> usize {
    let mut t = [0u8; 8]; let mut k = 0usize;
    if v == 0 { b[0] = b'0'; return 1; }
    while v > 0 { t[k] = b'0' + (v % 10) as u8; v /= 10; k += 1; }
    for i in 0..k { b[i] = t[k - 1 - i]; }
    k
}

/// ICH `@`, DCH `P`, ECH `X`, REP `b`.
unsafe fn t_emu_edit_chars() -> bool {
    let fd = open_emu_vt();
    if fd < 0 { return report(b"emu_edit_chars", false); }
    // Row 1: abcdef, ICH 2 at col 3 -> ab..cdef; row 2: DCH 2 at col 1 of abcdef -> cdef;
    // row 3: ECH 2 at col 3 of abcdef -> ab..ef; row 4: Q then REP 3 -> QQQQ.
    let cur = emu_grid(fd, b"abcdef\x1b[1;3H\x1b[2@\x1b[2;1Habcdef\x1b[2;1H\x1b[2P\x1b[3;1Habcdef\x1b[3;3H\x1b[2X\x1b[4;1HQ\x1b[3b");
    close(fd);
    let row = |r: usize, want: &[u8]| -> bool {
        let mut i = 0usize;
        while i < want.len() { if gcell(r, i) != want[i] as u32 { return false; } i += 1; }
        gcell(r, want.len()) == 0
    };
    let ok = cur.is_some()
        && row(0, b"ab\0\0cdef") && row(1, b"cdef") && row(2, b"ab\0\0ef")
        && row(3, b"QQQQ") && cur.as_ref().map_or(false, |c| c.col == 4 && c.row == 3);
    report(b"emu_edit_chars", ok)
}

/// IL `L` and DL `M` (whole-screen region, cursor to column 0).
unsafe fn t_emu_edit_lines() -> bool {
    let fd = open_emu_vt();
    if fd < 0 { return report(b"emu_edit_lines", false); }
    let cur = emu_grid(fd, b"A\r\nB\r\nC\x1b[2;3H\x1b[L");
    let ok1 = cur.as_ref().map_or(false, |c| c.col == 0 && c.row == 1)
        && gcell(0, 0) == b'A' as u32 && gcell(1, 0) == 0
        && gcell(2, 0) == b'B' as u32 && gcell(3, 0) == b'C' as u32;
    let b = b"\x1b[M";
    write(fd, b.as_ptr() as *const c_void, b.len());
    sleep_ms(300);
    let cur2 = grid_read();
    close(fd);
    let ok2 = cur2.is_some() && gcell(0, 0) == b'A' as u32 && gcell(1, 0) == b'B' as u32
        && gcell(2, 0) == b'C' as u32 && gcell(3, 0) == 0;
    report(b"emu_edit_lines", ok1 && ok2)
}

/// DECAWM: `CSI ? 7 l` pins a character written past the last column onto it.
unsafe fn t_emu_autowrap() -> bool {
    let fd = open_emu_vt();
    if fd < 0 { return report(b"emu_autowrap", false); }
    let (cols, _) = emu_size(fd);
    let mut m = [0u8; 64];
    let mut n = 0usize;
    for b in b"\x1b[?7l\x1b[1;" { m[n] = *b; n += 1; }
    n += fmt_num(&mut m[n..], cols);
    for b in b"Hab\x1b[?7h" { m[n] = *b; n += 1; }
    let cur = emu_grid(fd, &m[..n]);
    close(fd);
    let ok = cur.map_or(false, |c| c.row == 0) && gcell(0, cols - 1) == b'b' as u32 && gcell(1, 0) == 0;
    report(b"emu_autowrap", ok)
}

/// Every East-Asian-Wide range the glyph table draws must also advance the
/// cursor by two cells (one source of truth: drivers::wcwidth).
unsafe fn t_emu_wide_ranges() -> bool {
    let fd = open_emu_vt();
    if fd < 0 { return report(b"emu_wide_ranges", false); }
    let cps: [u32; 16] = [0x2630, 0x2637, 0x268A, 0x4DC0, 0x4DFF, 0x16FE0, 0x18800, 0x1AFF0,
                          0x1B000, 0x1D300, 0x1F004, 0x1F0CF, 0x1F18E, 0x1F191, 0x1F200, 0x1F7E0];
    let more: [u32; 2] = [0x1FA70, 0x1FAF8];
    let mut buf = [0u8; 256];
    let mut n = 0usize;
    let mut k = 0usize;
    let all = cps.len() + more.len();
    while k < all {
        let cp = if k < cps.len() { cps[k] } else { more[k - cps.len()] };
        for b in b"\x1b[" { buf[n] = *b; n += 1; }
        n += fmt_num(&mut buf[n..], k + 1);
        for b in b";1H" { buf[n] = *b; n += 1; }
        if let Some(ch) = char::from_u32(cp) {
            let mut e = [0u8; 4];
            let s = ch.encode_utf8(&mut e);
            for b in s.as_bytes() { buf[n] = *b; n += 1; }
        }
        buf[n] = b'X'; n += 1;
        k += 1;
    }
    let cur = emu_grid(fd, &buf[..n]);
    close(fd);
    let mut ok = cur.is_some();
    k = 0;
    while k < all {
        let cp = if k < cps.len() { cps[k] } else { more[k - cps.len()] };
        let good = gcell(k, 0) == cp && gcell(k, 1) == WIDE_CONT && gcell(k, 2) == b'X' as u32;
        if !good { out(b"  wide range U+"); out_hex(cp); out(b" not 2 cells\n"); ok = false; }
        k += 1;
    }
    report(b"emu_wide_ranges", ok)
}


/// DECSTBM region: LF at the bottom margin scrolls only the region, RI at the
/// top margin scrolls it down, IL stays inside it, DECOM homes to the margin.
unsafe fn t_emu_region() -> bool {
    let fd = open_emu_vt();
    if fd < 0 { return report(b"emu_region", false); }
    let cur = emu_grid(fd, b"1\r\n2\r\n3\r\n4\r\n5\r\n6\x1b[2;4r\x1b[4;1H\n\x1b[2;1H\x1bM\x1b[?6h\x1b[1;1HX\x1b[?6l\x1b[3;1H\x1b[L\x1b[r");
    close(fd);
    let want: [&[u8]; 6] = [b"1", b"X", b"", b"3", b"5", b"6"];
    let mut ok = cur.as_ref().map_or(false, |c| c.col == 0 && c.row == 0);
    for (r, w) in want.iter().enumerate() {
        let mut i = 0usize;
        while i < w.len() { if gcell(r, i) != w[i] as u32 { ok = false; } i += 1; }
        if gcell(r, w.len()) != 0 { ok = false; }
    }
    if !ok { out(b"  region rows:"); for r in 0..6 { out(b" "); out_hex(gcell(r, 0)); } out(b"\n"); }
    report(b"emu_region", ok)
}

/// SGR background, reverse, and background-colour erase.
unsafe fn t_emu_sgr() -> bool {
    let fd = open_emu_vt();
    if fd < 0 { return report(b"emu_sgr", false); }
    let (cols, _) = emu_size(fd);
    let cur = emu_grid(fd, b"\x1b[44mAB\x1b[0m C\x1b[7mD\x1b[0m\x1b[41m\x1b[K\x1b[0m");
    close(fd);
    let ok = cur.is_some() && gcell(0, 0) == b'A' as u32 && gbg(0, 0) == 4 && gbg(0, 1) == 4
        && gbg(0, 2) == 0 && gbg(0, 3) == 0 && gbg(0, 4) == 15 && gbg(0, 5) == 1 && gbg(0, cols - 1) == 1;
    if !ok { out(b"  sgr bg:"); for c in 0..7 { out(b" "); out_hex(gbg(0, c) as u32); } out(b"\n"); }
    report(b"emu_sgr", ok)
}

/// ESC ( 0 line drawing, G1 via SO/SI, and ESC ( B back to ASCII.
unsafe fn t_emu_acs() -> bool {
    let fd = open_emu_vt();
    if fd < 0 { return report(b"emu_acs", false); }
    let cur = emu_grid(fd, b"\x1b(0lqk\r\nx x\r\nmqj\x1b(B\r\nq\x1b)0\x0eq\x0fq\x1bcq");
    close(fd);
    let want: [&[u32]; 4] = [&[0x250C, 0x2500, 0x2510], &[0x2502, 0, 0x2502], &[0x2514, 0x2500, 0x2518],
                             &[b'q' as u32, 0x2500, b'q' as u32, b'q' as u32]];
    let mut ok = cur.is_some();
    for (r, w) in want.iter().enumerate() {
        for (c, &v) in w.iter().enumerate() { if gcell(r, c) != v { ok = false; } }
    }
    if !ok { out(b"  acs row0:"); for c in 0..3 { out(b" "); out_hex(gcell(0, c)); } out(b"\n"); }
    report(b"emu_acs", ok)
}

/// Bold+reverse red set on a VT survives a switch away and back: the next
/// character painted after the round trip still has the bright-red background.
unsafe fn t_attr_switch(tty0: c_int) -> bool {
    let fb = open(b"/dev/fb0\0".as_ptr(), 0);
    if fb < 0 { return report(b"attr_switch", false); }
    let mut free: c_int = -1;
    ioctl(tty0, VT_OPENQRY, &mut free as *mut c_int as *mut c_void);
    if !(2..=6).contains(&free) { free = 6; }
    let path = [b'/', b'd', b'e', b'v', b'/', b't', b't', b'y', b'0' + free as u8, 0];
    let fd = open(path.as_ptr(), O_RDWR | O_NOCTTY | O_NONBLOCK);
    if fd < 0 { close(fb); return report(b"attr_switch", false); }
    let sid = login_pause(fd);
    let mut ok = switch_to(tty0, free as usize);
    sleep_ms(300);
    let m = b"\x1b[2J\x1b[H\x1b[1;7;31mA";
    write(fd, m.as_ptr() as *const c_void, m.len());
    sleep_ms(300);
    ok &= switch_to(tty0, 1);
    sleep_ms(300);
    ok &= switch_to(tty0, free as usize);
    sleep_ms(300);
    let m2 = b"\x1b[3;1H ";
    write(fd, m2.as_ptr() as *const c_void, m2.len());
    sleep_ms(300);
    let mut info = [0u32; 8];
    if ioctl(fb, 0x4600, info.as_mut_ptr() as *mut c_void) != 0 || info[6] != 32 { ok = false; }
    let pitch = info[7] as i64;
    // Row 2 (0-based), cell 0: a space drawn with the restored pen is a solid background.
    let mut px = [0u32; 1];
    lseek(fb, 2 * 16 * pitch + 4 * 0 + 0, 0);
    read(fb, px.as_mut_ptr() as *mut c_void, 4);
    let got = px[0] & 0xFFFFFF;
    if got != 0xFF0000 { ok = false; out(b"  attr px="); out_hex(got); out(b"\n"); }
    switch_to(tty0, 1);
    sleep_ms(100);
    login_resume(sid);
    close(fd);
    close(fb);
    report(b"attr_switch", ok)
}

static BTM_RAW: &[u8] = include_bytes!("../data/btm-100x30.raw");
static BTM_GRID: &[u8] = include_bytes!("../data/btm-100x30.grid");

/// Replay a recorded `btm` frame (Alpine's bottom in the guest, 100x30) into
/// the emulator and require the cell grid to equal the pyte reference grid.
unsafe fn t_emu_btm_replay() -> bool {
    let fd = open_emu_vt();
    if fd < 0 { return report(b"emu_btm_replay", false); }
    let (cols, rows) = emu_size(fd);
    if cols < 100 || rows < 30 { close(fd); out(b"  console smaller than 100x30\n"); return report(b"emu_btm_replay", false); }
    let pre = b"\x1bc\x1b[2J\x1b[H";
    write(fd, pre.as_ptr() as *const c_void, pre.len());
    let mut off = 0usize;
    while off < BTM_RAW.len() {
        let n = (BTM_RAW.len() - off).min(1024);
        write(fd, BTM_RAW.as_ptr().add(off) as *const c_void, n);
        off += n;
    }
    sleep_ms(500);
    let cur = grid_read();
    close(fd);
    let mut ok = cur.is_some();
    let mut bad = 0usize;
    for line in BTM_GRID.split(|&b| b == b'\n') {
        if !line.starts_with(b"row ") { continue; }
        let mut i = 4;
        let r = parse_num(line, &mut i, 10) as usize;
        i += 1;
        let mut c = 0usize;
        while c < 100 {
            let want = if i < line.len() { i += 1; parse_num(line, &mut i, 16) } else { 0 };
            if gcell(r, c) != want {
                if bad < 4 { out(b"  btm diverge row "); out_num(r); out(b" col "); out_num(c);
                    out(b" ours "); out_hex(gcell(r, c)); out(b" want "); out_hex(want); out(b"\n"); }
                bad += 1;
            }
            c += 1;
        }
    }
    if bad != 0 { ok = false; out(b"  btm divergent cells: "); out_num(bad); out(b"\n"); }
    report(b"emu_btm_replay", ok)
}

/// Double-width characters must reach the framebuffer as their own glyphs, not
/// the two-cell placeholder box or a blank. Paints one sample per text row on a
/// free VT, reads the live surface back through /dev/fb0 and fingerprints each
/// 2-cell region (16x16 px at the 8x16 console cell, origin 0,0).
unsafe fn t_wide_glyphs(tty0: c_int) -> bool {
    let fb = open(b"/dev/fb0\0".as_ptr(), 0);
    if fb < 0 { out(b"  fb0 open failed\n"); return report(b"wide_glyphs", false); }
    let mut info = [0u32; 8];
    if ioctl(fb, 0x4600, info.as_mut_ptr() as *mut c_void) != 0 || info[6] != 32 {
        out(b"  FBIOGET_VSCREENINFO failed\n"); close(fb); return report(b"wide_glyphs", false);
    }
    let pitch = info[7] as i64;
    let mut free: c_int = -1;
    ioctl(tty0, VT_OPENQRY, &mut free as *mut c_int as *mut c_void);
    if !(2..=6).contains(&free) { free = 6; }
    let path = [b'/', b'd', b'e', b'v', b'/', b't', b't', b'y', b'0' + free as u8, 0];
    let fd = open(path.as_ptr(), O_RDWR | O_NOCTTY | O_NONBLOCK);
    if fd < 0 { close(fb); return report(b"wide_glyphs", false); }
    let sid = login_pause(fd);
    let mut ok = switch_to(tty0, free as usize);
    sleep_ms(300);
    // CJK ideograph x2, kana, Hangul, BMP emoji (watch), SMP emoji (grinning face), plus two
    // code points outside the original emoji ranges (mahjong red dragon, melting face).
    let msg = "\x1b[2J\x1b[1;1H\u{4e2d}\x1b[2;1H\u{6f22}\x1b[3;1H\u{3042}\x1b[4;1H\u{d55c}\x1b[5;1H\u{231a}\x1b[6;1H\u{1f600}\x1b[7;1H\u{1f004}\x1b[8;1H\u{1fae0}";
    write(fd, msg.as_ptr() as *const c_void, msg.len());
    sleep_ms(500);
    let mut hashes = [0u64; 8];
    let mut k = 0usize;
    while k < 8 {
        let (mut h, mut ink, mut same_as_box) = (0xcbf29ce484222325u64, 0usize, true);
        let mut y = 0usize;
        while y < 16 {
            let mut row = [0u32; 16];
            lseek(fb, (k * 16 + y) as i64 * pitch, 0);
            read(fb, row.as_mut_ptr() as *mut c_void, 64);
            let mut x = 0usize;
            while x < 16 {
                let px = row[x] & 0xFFFFFF;
                h = (h ^ px as u64).wrapping_mul(0x100000001b3);
                let lit = px != 0;
                if lit { ink += 1; }
                let box_px = ((y == 1 || y == 14) && (1..=14).contains(&x))
                    || ((x == 1 || x == 14) && (1..=14).contains(&y));
                if lit != box_px { same_as_box = false; }
                x += 1;
            }
            y += 1;
        }
        hashes[k] = h;
        out(b"  wide sample "); out_num(k); out(b" ink="); out_num(ink);
        out(b" box="); out_num(same_as_box as usize); out(b" hash="); out_num((h & 0xFFFFFF) as usize); out(b"\n");
        if ink == 0 || same_as_box { ok = false; }
        k += 1;
    }
    let mut a = 0usize;
    while a < 8 { let mut b = a + 1; while b < 8 { if hashes[a] == hashes[b] { ok = false; } b += 1; } a += 1; }
    switch_to(tty0, 1);
    sleep_ms(100);
    login_resume(sid);
    close(fd);
    close(fb);
    report(b"wide_glyphs", ok)
}

/// Narrow non-ASCII code points must draw their real Unifont glyph (precomposed
/// Latin, Cyrillic, Greek) or the box-drawing line, not an ASCII stand-in.
/// One sample per text row on a free VT; each 8x16 cell is read back through
/// /dev/fb0 and compared with the expected bitmap rows.
unsafe fn t_latin_glyphs(tty0: c_int) -> bool {
    let fb = open(b"/dev/fb0\0".as_ptr(), 0);
    if fb < 0 { out(b"  fb0 open failed\n"); return report(b"latin_glyphs", false); }
    let mut info = [0u32; 8];
    if ioctl(fb, 0x4600, info.as_mut_ptr() as *mut c_void) != 0 || info[6] != 32 {
        out(b"  FBIOGET_VSCREENINFO failed\n"); close(fb); return report(b"latin_glyphs", false);
    }
    let pitch = info[7] as i64;
    let mut free: c_int = -1;
    ioctl(tty0, VT_OPENQRY, &mut free as *mut c_int as *mut c_void);
    if !(2..=6).contains(&free) { free = 6; }
    let path = [b'/', b'd', b'e', b'v', b'/', b't', b't', b'y', b'0' + free as u8, 0];
    let fd = open(path.as_ptr(), O_RDWR | O_NOCTTY | O_NONBLOCK);
    if fd < 0 { close(fb); return report(b"latin_glyphs", false); }
    let sid = login_pause(fd);
    let mut ok = switch_to(tty0, free as usize);
    sleep_ms(300);
    let msg = "\x1b[2J\x1b[1;1H\u{e9}\x1b[2;1H\u{416}\x1b[3;1H\u{3a9}\x1b[4;1H\u{2500}";
    write(fd, msg.as_ptr() as *const c_void, msg.len());
    sleep_ms(500);
    // Unifont 18.0.01 rows (bit 7 = leftmost); [3] is the box line, checked below.
    let want: [[u8; 16]; 3] = [
        [0x00, 0x00, 0x0C, 0x30, 0x00, 0x00, 0x3C, 0x42, 0x42, 0x7E, 0x40, 0x40, 0x42, 0x3C, 0x00, 0x00],
        [0x00, 0x00, 0x00, 0x00, 0x49, 0x49, 0x2A, 0x2A, 0x1C, 0x1C, 0x2A, 0x2A, 0x49, 0x49, 0x00, 0x00],
        [0x00, 0x00, 0x00, 0x00, 0x3E, 0x41, 0x41, 0x41, 0x41, 0x41, 0x22, 0x14, 0x14, 0x77, 0x00, 0x00],
    ];
    let mut k = 0usize;
    while k < 4 {
        let mut got = [0u8; 16];
        let mut y = 0usize;
        while y < 16 {
            let mut row = [0u32; 8];
            lseek(fb, (k * 16 + y) as i64 * pitch, 0);
            read(fb, row.as_mut_ptr() as *mut c_void, 32);
            let mut x = 0usize;
            while x < 8 { if row[x] & 0xFFFFFF != 0 { got[y] |= 0x80 >> x; } x += 1; }
            y += 1;
        }
        let good = if k < 3 {
            got == want[k]
        } else {
            // box line: some row fully lit, every other row blank
            let mut full = 0usize; let mut other = 0usize;
            for &b in got.iter() { if b == 0xFF { full += 1 } else if b != 0 { other += 1 } }
            full >= 1 && other == 0
        };
        out(b"  latin sample "); out_num(k); out(b" match="); out_num(good as usize); out(b"\n");
        if !good { ok = false; }
        k += 1;
    }
    switch_to(tty0, 1);
    sleep_ms(100);
    login_resume(sid);
    close(fd);
    close(fb);
    report(b"latin_glyphs", ok)
}

#[no_mangle]
pub unsafe extern "C" fn main(argc_c: i32, argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let argc = argc_c as isize;
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
        } else if arg_eq(argv, 1, b"trap") {
            cmd_trap(tty0, if argc > 2 { arg_num(argv, 2) as i64 } else { 30000 })
        } else if arg_eq(argv, 1, b"state") {
            cmd_state(tty0)
        } else if arg_eq(argv, 1, b"chvt") && argc > 2 {
            // chvt <vt>: synchronous switch, for scripted VT checks (lane gamevt).
            if switch_to(tty0, arg_num(argv, 2)) { 0 } else { out(b"vttest chvt: switch failed\n"); 1 }
        } else if arg_eq(argv, 1, b"kbdrep") {
            // kbdrate: set delay/period (ms), print what is in force.
            let mut r = kbd_repeat { delay: if argc > 2 { arg_num(argv, 2) as c_int } else { 0 },
                                     period: if argc > 3 { arg_num(argv, 3) as c_int } else { 0 } };
            let rc = ioctl(tty0, KDKBDREP, &mut r as *mut kbd_repeat as *mut c_void);
            out(b"vttest kbdrep: rc="); out_int(rc); out(b" delay="); out_int(r.delay);
            out(b" period="); out_int(r.period); out(b"\n");
            if rc == 0 { 0 } else { 1 }
        } else {
            out(b"usage: vttest [hold <vt> <ms> | kbmode raw|xlate | gfx <ms> \
                  | trap <ms> | state | kbdrep <delay> <period>]\n");
            2
        };
        close(tty0);
        return rc;
    }

    out(b"vttest: virtual consoles\n");
    let mut passed = 0usize;
    let total = 52usize;

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
    if t_present_in_release_window(tty0) { passed += 1; }
    if t_master_per_vt(tty0) { passed += 1; }
    if t_grab_roundtrip() { passed += 1; }
    if t_grab_is_exclusive() { passed += 1; }
    if t_grab_dies_on_vt_switch(tty0) { passed += 1; }
    if t_grab_dies_on_close() { passed += 1; }
    if t_session_node(tty0) { passed += 1; }
    if t_tabs_wide(tty0) { passed += 1; }
    if t_cpr_esc(tty0) { passed += 1; }
    if t_revoke() { passed += 1; }
    if t_revoke_releases_grab() { passed += 1; }
    if t_kd_graphics_owner_death() { passed += 1; }
    if t_kd_graphics_survives_switch(tty0) { passed += 1; }
    if t_kbdrep() { passed += 1; }
    if t_kb_defaults() { passed += 1; }
    if t_keymap_entries() { passed += 1; }
    if t_kbled() { passed += 1; }
    if t_kbsent() { passed += 1; }
    if t_emu_tab_default() { passed += 1; }
    if t_emu_tab_stops() { passed += 1; }
    if t_emu_wide_advance() { passed += 1; }
    if t_emu_wide_overwrite() { passed += 1; }
    if t_emu_wide_wrap() { passed += 1; }
    if t_emu_wrap_deferred() { passed += 1; }
    if t_emu_edit_chars() { passed += 1; }
    if t_emu_edit_lines() { passed += 1; }
    if t_emu_autowrap() { passed += 1; }
    if t_emu_wide_ranges() { passed += 1; }
    if t_emu_btm_replay() { passed += 1; }
    if t_emu_region() { passed += 1; }
    if t_emu_sgr() { passed += 1; }
    if t_emu_acs() { passed += 1; }
    if t_attr_switch(tty0) { passed += 1; }
    if t_wide_glyphs(tty0) { passed += 1; }
    if t_latin_glyphs(tty0) { passed += 1; }

    // Whatever happened, leave the machine usable: VT 1, text mode, the
    // default keyboard mode. A suite that fails halfway through a KD_GRAPHICS
    // subtest and stops there hands back a console nobody can read.
    switch_to(tty0, 1);
    set_mode(tty0, KD_TEXT);
    ioctl(tty0, KDSKBMODE, K_UNICODE as *mut c_void);

    out(b"vttest: "); out_num(passed); out(b"/"); out_num(total); out(b"\n");
    out(b"--- vttest done ---\n");
    close(tty0);
    (total - passed) as i32
}
