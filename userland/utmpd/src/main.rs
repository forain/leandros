//! utmpd - serves /run/utmps/utmp over the `utmps` protocol.
//!
//! Alpine's busybox (`who`, `w`) reads utmp through libutmps, which talks to a
//! daemon on /run/utmps/.utmpd-socket instead of opening the file. This is a
//! small, sequential implementation of that daemon (skarnet utmps-utmpd):
//! commands 'e' getent, 'i' getid, 'l' getline, 'E' putline, 'r' rewind. Each
//! connection has its own file position, like the original's per-connection
//! process. Writers (login, sshd, greetd, init) update the file directly.

#![no_std]
#![no_main]

extern crate leandros_libc;

#[path = "../../../lib/utmpx/utmpx.rs"]
mod utmpx;

use utmpx::*;

#[cfg(target_arch = "aarch64")]
mod snr { pub const SOCKET: usize = 198; pub const BIND: usize = 200; pub const LISTEN: usize = 201;
    pub const ACCEPT: usize = 202; pub const UNLINKAT: usize = 35; pub const SETSOCKOPT: usize = 208;
    pub const GETSOCKOPT: usize = 209; pub const UMASK: usize = 166; }
#[cfg(target_arch = "x86_64")]
mod snr { pub const SOCKET: usize = 41; pub const BIND: usize = 49; pub const LISTEN: usize = 50;
    pub const ACCEPT: usize = 43; pub const UNLINKAT: usize = 263; pub const SETSOCKOPT: usize = 54;
    pub const GETSOCKOPT: usize = 55; pub const UMASK: usize = 95; }

const ESRCH: u8 = 3;
const EPERM: u8 = 1;

unsafe fn serve(c: isize) {
    // Stop a stalled client from wedging the (sequential) server.
    let tv = [5i64, 0i64];
    for opt in [20usize, 21] {
        sys(snr::SETSOCKOPT, [c as usize, 1, opt, tv.as_ptr() as usize, 16, 0]);
    }
    let mut cred = [0u32; 3];
    let mut clen = 12u32;
    let pc = sys(snr::GETSOCKOPT, [c as usize, 1, 17, cred.as_mut_ptr() as usize, &mut clen as *mut u32 as usize, 0]);
    let peer_root = pc == 0 && cred[1] == 0;

    let fd = open(UTMP_PATH, 2 | 0o100, 0o644);
    if fd < 0 { return; }
    loop {
        let mut cmd = [0u8; 1];
        if read(c, &mut cmd) != 1 { break; }
        match cmd[0] {
            b'e' | b'i' | b'l' => {
                let mut want_type = 0u16;
                let mut want = [0u8; 32];
                if cmd[0] == b'i' {
                    let mut a = [0u8; 6];
                    if !full(c, &mut a) { break; }
                    want_type = u16::from_be_bytes([a[0], a[1]]);
                    want[..4].copy_from_slice(&a[2..6]);
                } else if cmd[0] == b'l' {
                    if !full(c, &mut want) { break; }
                }
                let mut found: Option<[u8; REC]> = None;
                loop {
                    let mut b = [0u8; REC];
                    if !fill(fd, &mut b) { break; }
                    let u = Utmpx::from_bytes(&b);
                    let ok = match cmd[0] {
                        b'e' => true,
                        b'i' => {
                            let t = u.ut_type;
                            let proc_ = |x: i16| matches!(x, 5 | 6 | 7 | 8);
                            if matches!(want_type, 2 | 3 | 4) { want_type as i16 == t }
                            else { proc_(want_type as i16) && proc_(t) && u.ut_id == [want[0], want[1], want[2], want[3]] }
                        }
                        _ => (u.ut_type == LOGIN_PROCESS || u.ut_type == USER_PROCESS) && u.ut_line == want,
                    };
                    if ok { found = Some(b); break; }
                }
                match found {
                    Some(b) => { let mut out = [0u8; REC + 1]; out[1..].copy_from_slice(&b); write(c, &out); }
                    None => { write(c, &[ESRCH]); }
                }
            }
            b'E' => {
                let mut b = [0u8; REC];
                if !full(c, &mut b) { break; }
                if !peer_root { write(c, &[EPERM]); continue; }
                utmp_put(&Utmpx::from_bytes(&b));
                write(c, &[0]);
            }
            b'r' => { lseek(fd, 0, 0); write(c, &[0]); }
            _ => break,
        }
    }
    close(fd);
}

unsafe fn full(c: isize, b: &mut [u8]) -> bool {
    let mut got = 0;
    while got < b.len() {
        let r = read(c, &mut b[got..]);
        if r <= 0 { return false; }
        got += r as usize;
    }
    true
}
unsafe fn fill(fd: isize, b: &mut [u8]) -> bool { full(fd, b) }

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    sys(nr_mkdirat(), [-100isize as usize, UTMP_DIR.as_ptr() as usize, 0o755, 0, 0, 0]);
    sys(snr::UMASK, [0, 0, 0, 0, 0, 0]);
    let s = sys(snr::SOCKET, [1, 1 | 0o2000000, 0, 0, 0, 0]);
    if s < 0 { return 1; }
    sys(snr::UNLINKAT, [-100isize as usize, UTMP_SOCK.as_ptr() as usize, 0, 0, 0, 0]);
    let mut addr = [0u8; 110];
    addr[0] = 1; // AF_UNIX
    let n = UTMP_SOCK.len();
    addr[2..2 + n].copy_from_slice(UTMP_SOCK);
    if sys(snr::BIND, [s as usize, addr.as_ptr() as usize, 2 + n, 0, 0, 0]) < 0 { return 1; }
    if sys(snr::LISTEN, [s as usize, 8, 0, 0, 0, 0]) < 0 { return 1; }
    loop {
        let c = sys(snr::ACCEPT, [s as usize, 0, 0, 0, 0, 0]);
        if c < 0 { continue; }
        serve(c);
        close(c);
    }
}

#[cfg(target_arch = "aarch64")]
fn nr_mkdirat() -> usize { 34 }
#[cfg(target_arch = "x86_64")]
fn nr_mkdirat() -> usize { 258 }
