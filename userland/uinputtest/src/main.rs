//! uinputtest - /dev/uinput: create a Bluetooth-style gamepad, check the
//! evdev capability answers, round-trip events, destroy and check the node
//! is gone. Prints "<name>: PASS"/"<name>: FAIL"; exit status = failures.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;

type c_int = i32;
type c_ulong = u64;
type size_t = usize;

const O_RDONLY: c_int = 0;
const O_RDWR: c_int = 2;
const O_NONBLOCK: c_int = 0o4000;

const UI_DEV_CREATE: c_ulong = 0x5501;
const UI_DEV_DESTROY: c_ulong = 0x5502;
const UI_DEV_SETUP: c_ulong = 0x405c5503;
const UI_ABS_SETUP: c_ulong = 0x401c5504;
const UI_SET_EVBIT: c_ulong = 0x40045564;
const UI_SET_KEYBIT: c_ulong = 0x40045565;
const UI_SET_ABSBIT: c_ulong = 0x40045567;

const EVIOCGID: c_ulong = 0x80084502;
const EVIOCGNAME_64: c_ulong = 0x80404506;
const EVIOCGBIT_EV: c_ulong = 0x80044520;
const EVIOCGBIT_KEY: c_ulong = 0x80604521;
const EVIOCGBIT_ABS: c_ulong = 0x80084523;
const EVIOCGABS_X: c_ulong = 0x80184540;
const EVIOCGABS_Z: c_ulong = 0x80184542;
const EVIOCGABS_HAT0X: c_ulong = 0x80184550;

const UI_SET_PROPBIT: c_ulong = 0x4004556e;
const EVIOCGPROP_8: c_ulong = 0x80084509;
const EVIOCGMTSLOTS_24: c_ulong = 0x8018450a; // len = 4 + 5*4
const EVIOCGMTSLOTS_8: c_ulong = 0x8008450a; // len = 4 + 1*4
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_TOOL_TYPE: u16 = 0x37;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const TPNAME: &[u8] = b"lapmt test touchpad";

const BTN_SOUTH: u16 = 0x130;
const BTN_THUMBR: u16 = 0x13e;
const ABS_X: u16 = 0;
const ABS_Z: u16 = 2;
const ABS_RZ: u16 = 5;
const ABS_HAT0X: u16 = 0x10;

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
}

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start", ".global _start", "_start:",
    "   xor rbp, rbp", "   mov rdi, rsp", "   mov rsi, offset ui_main",
    "   and rsp, -16", "   call relibc_start_v1", "   ud2"
);
#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".section .text._start", ".global _start", "_start:",
    "   mov x29, #0", "   mov x30, #0", "   mov x0, sp",
    "   adrp x1, ui_main", "   add x1, x1, :lo12:ui_main",
    "   and sp, x0, #-16", "   bl relibc_start_v1", "   brk #0"
);

fn report(name: &[u8], ok: bool) -> bool {
    unsafe {
        write(1, name.as_ptr() as *const c_void, name.len());
        write(1, if ok { b": PASS\n".as_ptr() } else { b": FAIL\n".as_ptr() } as *const c_void, 7);
    }
    ok
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Ev { sec: i64, usec: i64, t: u16, c: u16, v: i32 }

fn ev(t: u16, c: u16, v: i32) -> Ev { Ev { sec: 0, usec: 0, t, c, v } }

#[repr(C)]
struct UiSetup { bustype: u16, vendor: u16, product: u16, version: u16, name: [u8; 80], ff: u32 }
#[repr(C)]
struct UiAbs { code: u16, pad: u16, value: i32, min: i32, max: i32, fuzz: i32, flat: i32, res: i32 }

const NAME: &[u8] = b"bthid test pad";

unsafe fn find_node() -> Option<u32> { find_named(NAME) }

unsafe fn find_named(want: &[u8]) -> Option<u32> {
    let mut path = *b"/dev/input/event00\0";
    for n in 2..16u32 {
        let len;
        if n >= 10 { path[16] = b'0' + (n / 10) as u8; path[17] = b'0' + (n % 10) as u8; len = 18; }
        else { path[16] = b'0' + n as u8; path[17] = 0; len = 17; }
        path[len] = 0;
        let fd = open(path.as_ptr(), O_RDONLY | O_NONBLOCK);
        if fd < 0 { continue; }
        let mut nm = [0u8; 64];
        let r = ioctl(fd, EVIOCGNAME_64, nm.as_mut_ptr());
        close(fd);
        if r >= 0 && &nm[..want.len()] == want && nm[want.len()] == 0 { return Some(n); }
    }
    None
}

fn path_of(n: u32, p: &mut [u8; 20]) {
    let s = b"/dev/input/event";
    p[..16].copy_from_slice(s);
    let mut l = 16;
    if n >= 10 { p[l] = b'0' + (n / 10) as u8; l += 1; }
    p[l] = b'0' + (n % 10) as u8; l += 1;
    p[l] = 0;
}

#[no_mangle]
pub unsafe extern "C" fn ui_main(_argc: isize, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let mut fails = 0;
    macro_rules! chk { ($n:expr, $c:expr) => { if !report($n, $c) { fails += 1; } } }
    puts(b"--- uinputtest ---\n\0".as_ptr());

    let ufd = open(b"/dev/uinput\0".as_ptr(), O_RDWR);
    chk!(b"open_uinput", ufd >= 0);
    if ufd < 0 { return fails; }

    let mut ok = true;
    chk!(b"set_evbit", ioctl(ufd, UI_SET_EVBIT, 1) == 0);
    ok &= ioctl(ufd, UI_SET_EVBIT, 1) == 0;
    ok &= ioctl(ufd, UI_SET_EVBIT, 3) == 0;
    chk!(b"set_keybit", ioctl(ufd, UI_SET_KEYBIT, 0x130) == 0);
    { let ab = UiAbs { code: 0, pad: 0, value: 0, min: -5, max: 5, fuzz: 0, flat: 0, res: 0 };
      chk!(b"abs_setup", ioctl(ufd, UI_ABS_SETUP, &ab as *const UiAbs) == 0); }
    for k in BTN_SOUTH..=BTN_THUMBR { ok &= ioctl(ufd, UI_SET_KEYBIT, k as c_int) == 0; }
    for a in [ABS_X, 1, ABS_Z, 3, 4, ABS_RZ, ABS_HAT0X, 0x11] {
        ok &= ioctl(ufd, UI_SET_ABSBIT, a as c_int) == 0;
        let (min, max) = match a { 2 | 5 => (0, 255), 0x10 | 0x11 => (-1, 1), _ => (-32768, 32767) };
        let ab = UiAbs { code: a, pad: 0, value: 0, min, max, fuzz: 0, flat: 0, res: 0 };
        ok &= ioctl(ufd, UI_ABS_SETUP, &ab as *const UiAbs) == 0;
    }
    let mut su = UiSetup { bustype: 5, vendor: 0x2dc8, product: 0x6001, version: 1, name: [0; 80], ff: 0 };
    su.name[..NAME.len()].copy_from_slice(NAME);
    ok &= ioctl(ufd, UI_DEV_SETUP, &su as *const UiSetup) == 0;
    chk!(b"setup_ioctls", ok);
    chk!(b"dev_create", ioctl(ufd, UI_DEV_CREATE) == 0);

    let node = find_node();
    chk!(b"node_appears", node.is_some());
    let node = match node { Some(n) => n, None => return fails };
    let mut p = [0u8; 20];
    path_of(node, &mut p);
    let efd = open(p.as_ptr(), O_RDONLY | O_NONBLOCK);
    chk!(b"open_node", efd >= 0);

    let mut id = [0u16; 4];
    chk!(b"eviocgid", ioctl(efd, EVIOCGID, id.as_mut_ptr()) >= 0 && id[0] == 5 && id[1] == 0x2dc8 && id[2] == 0x6001);

    let mut evb = [0u8; 4];
    ioctl(efd, EVIOCGBIT_EV, evb.as_mut_ptr());
    chk!(b"gbit_ev", evb[0] & 0x0a == 0x0a && evb[0] & 0x04 == 0);
    let mut kb = [0u8; 96];
    ioctl(efd, EVIOCGBIT_KEY, kb.as_mut_ptr());
    let has = |k: u16| kb[k as usize / 8] & (1 << (k % 8)) != 0;
    chk!(b"gbit_key", has(BTN_SOUTH) && has(BTN_THUMBR) && !has(BTN_THUMBR + 1) && !has(30));
    let mut ab = [0u8; 8];
    ioctl(efd, EVIOCGBIT_ABS, ab.as_mut_ptr());
    let hasa = |a: u16| ab[a as usize / 8] & (1 << (a % 8)) != 0;
    chk!(b"gbit_abs", hasa(0) && hasa(1) && hasa(2) && hasa(5) && hasa(0x10) && hasa(0x11) && !hasa(6));

    let mut ai = [0i32; 6];
    chk!(b"gabs_x", ioctl(efd, EVIOCGABS_X, ai.as_mut_ptr()) >= 0 && ai[1] == -32768 && ai[2] == 32767);
    chk!(b"gabs_z", ioctl(efd, EVIOCGABS_Z, ai.as_mut_ptr()) >= 0 && ai[1] == 0 && ai[2] == 255);
    chk!(b"gabs_hat", ioctl(efd, EVIOCGABS_HAT0X, ai.as_mut_ptr()) >= 0 && ai[1] == -1 && ai[2] == 1);

    let evs = [ev(1, BTN_SOUTH, 1), ev(3, ABS_X, 1234), ev(0, 0, 0)];
    let w = write(ufd, evs.as_ptr() as *const c_void, core::mem::size_of_val(&evs));
    chk!(b"write_events", w == 72);
    let mut got = [ev(9, 9, 9); 4];
    let r = read(efd, got.as_mut_ptr() as *mut c_void, 24 * 4);
    chk!(b"read_back", r == 72
        && got[0].t == 1 && got[0].c == BTN_SOUTH && got[0].v == 1
        && got[1].t == 3 && got[1].c == ABS_X && got[1].v == 1234
        && got[2].t == 0 && got[2].c == 0);
    chk!(b"gabs_current", ioctl(efd, EVIOCGABS_X, ai.as_mut_ptr()) >= 0 && ai[0] == 1234);

    chk!(b"dev_destroy", ioctl(ufd, UI_DEV_DESTROY) == 0);
    let r = read(efd, got.as_mut_ptr() as *mut c_void, 24);
    chk!(b"read_enodev", r < 0);
    close(efd);
    chk!(b"node_gone", find_node().is_none() && open(p.as_ptr(), O_RDONLY) < 0);

    // close of the uinput fd also removes a created device
    let mut ok2 = ioctl(ufd, UI_SET_EVBIT, 1) == 0 && ioctl(ufd, UI_SET_KEYBIT, 30) == 0;
    ok2 &= ioctl(ufd, UI_DEV_SETUP, &su as *const UiSetup) == 0;
    ok2 &= ioctl(ufd, UI_DEV_CREATE) == 0;
    chk!(b"recreate", ok2 && find_node().is_some());
    close(ufd);
    chk!(b"close_removes", find_node().is_none());

    fails += touchpad_test();

    puts(b"--- uinputtest done ---\n\0".as_ptr());
    fails
}

fn mtab(slot: i32, id: i32, x: i32, y: i32) -> [Ev; 4] {
    [ev(3, ABS_MT_SLOT, slot), ev(3, ABS_MT_TRACKING_ID, id), ev(3, ABS_MT_POSITION_X, x), ev(3, ABS_MT_POSITION_Y, y)]
}

/// Protocol-B 5-slot touchpad (the 06cb:82f1 shape): caps, props, MT slots, two-finger scroll, tap.
unsafe fn touchpad_test() -> i32 {
    let mut fails = 0;
    macro_rules! chk { ($n:expr, $c:expr) => { if !report($n, $c) { fails += 1; } } }
    let ufd = open(b"/dev/uinput\0".as_ptr(), O_RDWR);
    chk!(b"tp_open_uinput", ufd >= 0);
    if ufd < 0 { return fails; }
    let mut ok = true;
    for e in [1, 3] { ok &= ioctl(ufd, UI_SET_EVBIT, e) == 0; }
    for k in [0x110, 0x145, 0x14a, 0x148, 0x14d, 0x14e, 0x14f] { ok &= ioctl(ufd, UI_SET_KEYBIT, k) == 0; }
    ok &= ioctl(ufd, UI_SET_PROPBIT, 0) == 0 && ioctl(ufd, UI_SET_PROPBIT, 2) == 0;
    for (a, min, max, res) in [(0u16, 0, 1273, 12), (1, 0, 672, 12), (ABS_MT_SLOT, 0, 4, 0),
                               (ABS_MT_TRACKING_ID, 0, 65535, 0), (ABS_MT_POSITION_X, 0, 1273, 12),
                               (ABS_MT_POSITION_Y, 0, 672, 12), (ABS_MT_TOOL_TYPE, 0, 2, 0)] {
        ok &= ioctl(ufd, UI_SET_ABSBIT, a as c_int) == 0;
        let ab = UiAbs { code: a, pad: 0, value: 0, min, max, fuzz: 0, flat: 0, res };
        ok &= ioctl(ufd, UI_ABS_SETUP, &ab as *const UiAbs) == 0;
    }
    let mut su = UiSetup { bustype: 0x18, vendor: 0x06cb, product: 0x82f1, version: 1, name: [0; 80], ff: 0 };
    su.name[..TPNAME.len()].copy_from_slice(TPNAME);
    ok &= ioctl(ufd, UI_DEV_SETUP, &su as *const UiSetup) == 0;
    chk!(b"tp_setup", ok);
    chk!(b"tp_create", ioctl(ufd, UI_DEV_CREATE) == 0);
    let node = match find_named(TPNAME) { Some(n) => n, None => { chk!(b"tp_node", false); return fails; } };
    let mut p = [0u8; 20];
    path_of(node, &mut p);
    let efd = open(p.as_ptr(), O_RDONLY | O_NONBLOCK);
    chk!(b"tp_open_node", efd >= 0);

    let mut id = [0u16; 4];
    chk!(b"tp_id_i2c", ioctl(efd, EVIOCGID, id.as_mut_ptr()) >= 0 && id[0] == 0x18 && id[1] == 0x06cb && id[2] == 0x82f1);
    let mut pr = [0u8; 8];
    chk!(b"tp_prop", ioctl(efd, EVIOCGPROP_8, pr.as_mut_ptr()) >= 0 && pr[0] & 5 == 5 && pr[0] & 2 == 0);
    let mut kb = [0u8; 96];
    ioctl(efd, EVIOCGBIT_KEY, kb.as_mut_ptr());
    let has = |k: usize| kb[k / 8] & (1 << (k % 8)) != 0;
    chk!(b"tp_keys", has(0x110) && has(0x145) && has(0x14a) && has(0x148) && !has(0x111));
    let mut ab = [0u8; 8];
    ioctl(efd, EVIOCGBIT_ABS, ab.as_mut_ptr());
    let hasa = |a: u16| ab[a as usize / 8] & (1 << (a % 8)) != 0;
    chk!(b"tp_abs_bits", hasa(0) && hasa(1) && hasa(ABS_MT_SLOT) && hasa(ABS_MT_POSITION_X)
        && hasa(ABS_MT_POSITION_Y) && hasa(ABS_MT_TRACKING_ID) && hasa(ABS_MT_TOOL_TYPE));
    let mut ai = [0i32; 6];
    chk!(b"tp_gabs_slot", ioctl(efd, 0x80184540 + ABS_MT_SLOT as c_ulong, ai.as_mut_ptr()) >= 0 && ai[1] == 0 && ai[2] == 4);
    chk!(b"tp_gabs_mtx", ioctl(efd, 0x80184540 + ABS_MT_POSITION_X as c_ulong, ai.as_mut_ptr()) >= 0 && ai[2] == 1273 && ai[5] == 12);
    chk!(b"tp_gabs_mty", ioctl(efd, 0x80184540 + ABS_MT_POSITION_Y as c_ulong, ai.as_mut_ptr()) >= 0 && ai[2] == 672 && ai[5] == 12);

    // Idle slots read tracking id -1 (code=ABS_MT_TRACKING_ID).
    let mut sl = [0i32; 6];
    sl[0] = ABS_MT_TRACKING_ID as i32;
    chk!(b"tp_mtslots_idle", ioctl(efd, EVIOCGMTSLOTS_24, sl.as_mut_ptr()) >= 0
        && sl[0] == ABS_MT_TRACKING_ID as i32 && sl[1..6] == [-1; 5]);

    // Two-finger scroll: down at (400,300)/(520,300), move down 3 steps, lift.
    let mut seq = [ev(0, 0, 0); 64];
    let mut n = 0;
    let mut put = |seq: &mut [Ev; 64], n: &mut usize, e: &[Ev]| { for x in e { seq[*n] = *x; *n += 1; } };
    put(&mut seq, &mut n, &mtab(0, 10, 400, 300)); put(&mut seq, &mut n, &mtab(1, 11, 520, 300));
    put(&mut seq, &mut n, &[ev(1, 0x14a, 1), ev(1, 0x14d, 1), ev(0, 0, 0)]);
    put(&mut seq, &mut n, &mtab(0, 10, 400, 340)); put(&mut seq, &mut n, &mtab(1, 11, 520, 340));
    put(&mut seq, &mut n, &[ev(0, 0, 0)]);
    let w = write(ufd, seq.as_ptr() as *const c_void, n * 24);
    chk!(b"tp_write_scroll", w == (n * 24) as isize);
    sl[0] = ABS_MT_TRACKING_ID as i32;
    chk!(b"tp_mtslots_ids", ioctl(efd, EVIOCGMTSLOTS_24, sl.as_mut_ptr()) >= 0
        && sl[1] == 10 && sl[2] == 11 && sl[3] == -1 && sl[4] == -1 && sl[5] == -1);
    sl[0] = ABS_MT_POSITION_Y as i32;
    chk!(b"tp_mtslots_y", ioctl(efd, EVIOCGMTSLOTS_24, sl.as_mut_ptr()) >= 0 && sl[1] == 340 && sl[2] == 340);
    sl[0] = ABS_MT_POSITION_X as i32;
    chk!(b"tp_mtslots_x", ioctl(efd, EVIOCGMTSLOTS_24, sl.as_mut_ptr()) >= 0 && sl[1] == 400 && sl[2] == 520);
    let mut sm = [ABS_MT_POSITION_X as i32, 0];
    chk!(b"tp_mtslots_short", ioctl(efd, EVIOCGMTSLOTS_8, sm.as_mut_ptr()) >= 0 && sm[1] == 400);
    sm[0] = 0x30;
    chk!(b"tp_mtslots_badcode", ioctl(efd, EVIOCGMTSLOTS_8, sm.as_mut_ptr()) < 0);
    chk!(b"tp_gabs_slot_cur", ioctl(efd, 0x80184540 + ABS_MT_SLOT as c_ulong, ai.as_mut_ptr()) >= 0 && ai[0] == 1);
    // Events arrive in order, starting with ABS_MT_SLOT 0 / TRACKING_ID 10.
    let mut got = [ev(9, 9, 9); 8];
    let r = read(efd, got.as_mut_ptr() as *mut c_void, 24 * 8);
    chk!(b"tp_read_back", r == 192 && got[0].t == 3 && got[0].c == ABS_MT_SLOT && got[0].v == 0
        && got[1].c == ABS_MT_TRACKING_ID && got[1].v == 10 && got[2].v == 400 && got[3].v == 300);

    // Lift both, then a one-finger tap (down, up) and check slots return to idle.
    let lift = [ev(3, ABS_MT_SLOT, 0), ev(3, ABS_MT_TRACKING_ID, -1), ev(3, ABS_MT_SLOT, 1),
                ev(3, ABS_MT_TRACKING_ID, -1), ev(1, 0x14a, 0), ev(1, 0x14d, 0), ev(0, 0, 0)];
    write(ufd, lift.as_ptr() as *const c_void, core::mem::size_of_val(&lift));
    let tap = [ev(3, ABS_MT_SLOT, 0), ev(3, ABS_MT_TRACKING_ID, 12), ev(3, ABS_MT_POSITION_X, 600),
               ev(3, ABS_MT_POSITION_Y, 200), ev(1, 0x14a, 1), ev(1, 0x145, 1), ev(0, 0, 0)];
    write(ufd, tap.as_ptr() as *const c_void, core::mem::size_of_val(&tap));
    sl[0] = ABS_MT_TRACKING_ID as i32;
    chk!(b"tp_tap_down", ioctl(efd, EVIOCGMTSLOTS_24, sl.as_mut_ptr()) >= 0 && sl[1] == 12 && sl[2] == -1);
    let up = [ev(3, ABS_MT_TRACKING_ID, -1), ev(1, 0x14a, 0), ev(1, 0x145, 0), ev(0, 0, 0)];
    write(ufd, up.as_ptr() as *const c_void, core::mem::size_of_val(&up));
    chk!(b"tp_tap_up", ioctl(efd, EVIOCGMTSLOTS_24, sl.as_mut_ptr()) >= 0 && sl[1..6] == [-1; 5]);

    close(efd);
    close(ufd);
    chk!(b"tp_node_gone", find_named(TPNAME).is_none());
    fails
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}
