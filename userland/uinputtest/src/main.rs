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

unsafe fn find_node() -> Option<u32> {
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
        if r >= 0 && &nm[..NAME.len()] == NAME && nm[NAME.len()] == 0 { return Some(n); }
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

    puts(b"--- uinputtest done ---\n\0".as_ptr());
    fails
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}
