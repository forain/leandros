//! lastlog - last login of every account, from /var/log/lastlog
//! (`struct lastlog` at uid * 292) and /etc/passwd; util-linux layout.
//!   lastlog [-u user]
#![no_std]
#![no_main]
extern crate leandros_libc;
#[macro_use]
#[path = "../../ps/src/common.rs"]
mod common;
#[path = "ut.rs"]
mod ut;
use common::*;
use ut::*;

static mut PW: [u8; 16384] = [0; 16384];

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let mut only: Option<&[u8]> = None;
    let mut i = 1;
    while (i as i32) < argc {
        let a = arg(argv, i);
        if a == b"-u" || a == b"--user" {
            i += 1;
            if (i as i32) >= argc { eprintln!("lastlog: option requires an argument -- 'u'"); return 1; }
            only = Some(arg(argv, i));
        } else if a.starts_with(b"-u") {
            only = Some(&a[2..]);
        } else if a.starts_with(b"--user=") {
            only = Some(&a[7..]);
        } else {
            eprintln!("usage: lastlog [-u user]");
            return 1;
        }
        i += 1;
    }
    let fd = sys_open(LASTLOG, O_RDONLY).ok();
    let mut found = false;
    let mut first = true;
    each_passwd(&mut *core::ptr::addr_of_mut!(PW), |name, uid| {
        if let Some(o) = only { if o != name { return; } }
        found = true;
        if first { sayln!("Username         Port     From             Latest"); first = false; }
        let mut rec = [0u8; LL_REC];
        let mut got = 0;
        if let Some(fd) = fd {
            let r = syscall_pread(fd, &mut rec, uid as i64 * LL_REC as i64);
            if r == LL_REC as isize { got = LL_REC; }
        }
        let t = if got == LL_REC { i32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]) as i64 } else { 0 };
        let mut nm = common::Buf::<16>::new();
        nm.push(name);
        if t == 0 {
            say!("{:<16} ", core::str::from_utf8(nm.bytes()).unwrap_or("?"));
            sayln!("**Never logged in**");
        } else {
            let line = core::str::from_utf8(&cstr(&rec[4..36])[..cstr(&rec[4..36]).len().min(8)]).unwrap_or("?");
            let host = core::str::from_utf8(&cstr(&rec[36..])[..cstr(&rec[36..]).len().min(16)]).unwrap_or("?");
            let mut ts = common::Buf::<64>::new();
            full_time(t, &mut ts);
            sayln!("{:<16} {:<8} {:<16} {}", core::str::from_utf8(nm.bytes()).unwrap_or("?"), line, host,
                core::str::from_utf8(ts.bytes()).unwrap_or(""));
        }
    });
    if let Some(fd) = fd { sys_close(fd); }
    if only.is_some() && !found { eprintln!("lastlog: user not found"); return 1; }
    0
}

#[cfg(target_arch = "aarch64")] const PREAD64: usize = 67;
#[cfg(target_arch = "x86_64")] const PREAD64: usize = 17;
fn syscall_pread(fd: i32, b: &mut [u8], off: i64) -> isize {
    unsafe { leandros_libc::syscall::syscall4(PREAD64, fd as usize, b.as_mut_ptr() as usize, b.len(), off as usize) }
}
