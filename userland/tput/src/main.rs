#![no_std]
#![no_main]

extern crate leandros_libc;

use leandros_libc::{write, ioctl, atoi, STDOUT_FILENO, STDERR_FILENO};

// servers/tty/src/lib.rs handles this ioctl on any fd already wired to the
// console (fd 0/1/2); /dev/tty itself is a dead RamFS stub in this OS, so
// query the size through stdout rather than trying to open a tty device.
const TIOCGWINSZ: usize = 0x5413;

#[repr(C)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

unsafe fn out(s: &[u8]) {
    write(STDOUT_FILENO, s.as_ptr(), s.len());
}

unsafe fn err(s: &[u8]) {
    write(STDERR_FILENO, s.as_ptr(), s.len());
}

unsafe fn out_dec(mut n: u32) {
    let mut buf = [0u8; 10];
    if n == 0 { out(b"0"); return; }
    let mut i = 10usize;
    while n > 0 { i -= 1; buf[i] = b'0' + (n % 10) as u8; n /= 10; }
    out(&buf[i..]);
}

unsafe fn winsize() -> Winsize {
    let mut ws = Winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    ioctl(STDOUT_FILENO, TIOCGWINSZ, &mut ws as *mut Winsize as usize);
    ws
}

/// Compare a NUL-terminated argv string against a literal that includes its
/// own trailing `\0` (e.g. `b"cols\0"`).
unsafe fn ceq(a: *const u8, lit: &[u8]) -> bool {
    let mut i = 0usize;
    loop {
        let ac = *a.add(i);
        let bc = lit[i];
        if ac != bc { return false; }
        if ac == 0 { return true; }
        i += 1;
    }
}

unsafe fn arg(argv: *const *const u8, argc: i32, i: i32) -> *const u8 {
    if i >= argc { core::ptr::null() } else { *argv.offset(i as isize) }
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    if argc < 2 {
        err(b"usage: tput capname [params...]\n");
        return 2;
    }
    let cap = arg(argv, argc, 1);

    if ceq(cap, b"cols\0") {
        out_dec(winsize().ws_col as u32);
        out(b"\n");
        return 0;
    }
    if ceq(cap, b"lines\0") {
        out_dec(winsize().ws_row as u32);
        out(b"\n");
        return 0;
    }

    // Capabilities below are fixed ANSI/VT100 escapes: this OS has no
    // terminfo/termcap database and never propagates TERM to children
    // (userland/shell hardcodes an empty envp on exec), so there is no
    // per-terminal-type lookup to do.
    if ceq(cap, b"clear\0")  { out(b"\x1b[H\x1b[2J"); return 0; }
    if ceq(cap, b"sgr0\0")   { out(b"\x1b[0m");       return 0; }
    if ceq(cap, b"bold\0")   { out(b"\x1b[1m");       return 0; }
    if ceq(cap, b"dim\0")    { out(b"\x1b[2m");       return 0; }
    if ceq(cap, b"rev\0")    { out(b"\x1b[7m");       return 0; }
    if ceq(cap, b"smul\0")   { out(b"\x1b[4m");       return 0; }
    if ceq(cap, b"rmul\0")   { out(b"\x1b[24m");      return 0; }
    if ceq(cap, b"blink\0")  { out(b"\x1b[5m");       return 0; }
    if ceq(cap, b"civis\0")  { out(b"\x1b[?25l");     return 0; }
    if ceq(cap, b"cnorm\0")  { out(b"\x1b[?25h");     return 0; }
    if ceq(cap, b"smcup\0")  { out(b"\x1b[?1049h");   return 0; }
    if ceq(cap, b"rmcup\0")  { out(b"\x1b[?1049l");   return 0; }
    if ceq(cap, b"el\0")     { out(b"\x1b[K");        return 0; }
    if ceq(cap, b"ed\0")     { out(b"\x1b[J");        return 0; }
    if ceq(cap, b"home\0")   { out(b"\x1b[H");        return 0; }

    if ceq(cap, b"setaf\0") {
        if argc < 3 { err(b"tput: setaf requires a color argument\n"); return 2; }
        let n = atoi(arg(argv, argc, 2)) as u32;
        if n < 8 { out(b"\x1b[3"); out_dec(n); out(b"m"); }
        else     { out(b"\x1b[38;5;"); out_dec(n); out(b"m"); }
        return 0;
    }
    if ceq(cap, b"setab\0") {
        if argc < 3 { err(b"tput: setab requires a color argument\n"); return 2; }
        let n = atoi(arg(argv, argc, 2)) as u32;
        if n < 8 { out(b"\x1b[4"); out_dec(n); out(b"m"); }
        else     { out(b"\x1b[48;5;"); out_dec(n); out(b"m"); }
        return 0;
    }
    if ceq(cap, b"cup\0") {
        if argc < 4 { err(b"tput: cup requires row and column arguments\n"); return 2; }
        let row = atoi(arg(argv, argc, 2)) as u32;
        let col = atoi(arg(argv, argc, 3)) as u32;
        out(b"\x1b[");    out_dec(row + 1);
        out(b";");        out_dec(col + 1);
        out(b"H");
        return 0;
    }

    err(b"tput: unknown terminfo capability\n");
    1
}
