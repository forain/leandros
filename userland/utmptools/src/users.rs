//! users - names of the logged-in users, sorted, on one line (coreutils).
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

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let mut names = [[0u8; 32]; 256];
    let mut n = 0;
    each_utmp(|r| {
        if r.ut_type == USER_PROCESS && n < names.len() && r.ut_user[0] != 0 {
            names[n] = r.ut_user;
            n += 1;
        }
    });
    let v = &mut names[..n];
    v.sort_unstable();
    for (i, nm) in v.iter().enumerate() {
        if i > 0 { write_all(1, b" "); }
        write_all(1, cstr(nm));
    }
    write_all(1, b"\n");
    0
}
