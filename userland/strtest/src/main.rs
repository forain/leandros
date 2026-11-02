//! strtest — relibc string-to-number and environment conformance, the
//! calls Mesa makes while parsing MESA_DEBUG, V3D_DEBUG, MESA_GLSL_VERSION_OVERRIDE
//! and /proc, /sys text (lane kmscube, openbugs L11.8). Semantics are glibc/musl's.
//! Prints "<name>: PASS|FAIL"; returns the number of failures.

#![no_std]
#![no_main]
#![feature(c_variadic)]
#![allow(non_camel_case_types)]

#[path = "../../muslrt/rt.rs"]
mod rt;

use core::ffi::{c_char, c_int, c_long, c_ulong, c_void, c_double, c_longlong, c_ulonglong};
use core::ptr::{null, null_mut};

extern "C" {
    fn _exit(status: c_int) -> !;
    fn write(fd: c_int, b: *const c_void, n: usize) -> isize;
    fn strtol(s: *const c_char, e: *mut *mut c_char, b: c_int) -> c_long;
    fn strtoul(s: *const c_char, e: *mut *mut c_char, b: c_int) -> c_ulong;
    fn strtoll(s: *const c_char, e: *mut *mut c_char, b: c_int) -> c_longlong;
    fn strtoull(s: *const c_char, e: *mut *mut c_char, b: c_int) -> c_ulonglong;
    fn strtod(s: *const c_char, e: *mut *mut c_char) -> c_double;
    fn strtof(s: *const c_char, e: *mut *mut c_char) -> f32;
    fn atoi(s: *const c_char) -> c_int;
    fn atol(s: *const c_char) -> c_long;
    fn atof(s: *const c_char) -> c_double;
    fn getenv(s: *const c_char) -> *mut c_char;
    fn secure_getenv(s: *const c_char) -> *mut c_char;
    fn setenv(n: *const c_char, v: *const c_char, o: c_int) -> c_int;
    fn unsetenv(n: *const c_char) -> c_int;
    fn putenv(s: *mut c_char) -> c_int;
    fn __errno_location() -> *mut c_int;
    static mut environ: *mut *mut c_char;
}


#[panic_handler]
fn panic(_i: &core::panic::PanicInfo) -> ! { unsafe { _exit(134) } }

const ERANGE: c_int = 34;
const EINVAL: c_int = 22;

fn out(s: &str) { unsafe { write(1, s.as_ptr() as *const c_void, s.len()); } }
fn pnum(mut v: i128) {
    let mut b = [0u8; 44]; let mut i = 44; let neg = v < 0; if neg { v = -v; }
    if v == 0 { i -= 1; b[i] = b'0'; }
    while v > 0 { i -= 1; b[i] = b'0' + (v % 10) as u8; v /= 10; }
    if neg { i -= 1; b[i] = b'-'; }
    unsafe { write(1, b.as_ptr().add(i) as *const c_void, 44 - i); }
}
fn dbg(v: i128, n: isize, e: c_int) { out("  got "); pnum(v); out(" len "); pnum(n as i128); out(" errno "); pnum(e as i128); out("\n"); }
fn errno() -> c_int { unsafe { *__errno_location() } }
fn set_errno(v: c_int) { unsafe { *__errno_location() = v } }
fn cs(s: &'static [u8]) -> *const c_char { s.as_ptr() as *const c_char }

static mut FAILS: i32 = 0;
fn check(name: &str, ok: bool) {
    out(name);
    if ok { out(": PASS\n"); } else { out(": FAIL\n"); unsafe { FAILS += 1; } }
}

/// Run strtol; returns (value, consumed bytes, errno).
fn tl(s: &'static [u8], base: c_int) -> (c_long, isize, c_int) {
    let mut e: *mut c_char = null_mut();
    set_errno(0);
    let v = unsafe { strtol(cs(s), &mut e, base) };
    let n = e as isize - s.as_ptr() as isize;
    // musl sets EINVAL when no digits convert; glibc leaves errno; both allowed.
    let er = if n == 0 && errno() == EINVAL && (base == 0 || (2..=36).contains(&base)) { 0 } else { errno() };
    dbg(v as i128, n, er);
    (v, n, er)
}
fn tul(s: &'static [u8], base: c_int) -> (c_ulong, isize, c_int) {
    let mut e: *mut c_char = null_mut();
    set_errno(0);
    let v = unsafe { strtoul(cs(s), &mut e, base) };
    let n = e as isize - s.as_ptr() as isize;
    let er = if n == 0 && errno() == EINVAL { 0 } else { errno() };
    dbg(v as i128, n, er);
    (v, n, er)
}
fn td(s: &'static [u8]) -> (c_double, isize, c_int) {
    let mut e: *mut c_char = null_mut();
    set_errno(0);
    let v = unsafe { strtod(cs(s), &mut e) };
    dbg((v * 1e6) as i128, e as isize - s.as_ptr() as isize, errno());
    (v, e as isize - s.as_ptr() as isize, errno())
}

fn genv(n: &'static [u8]) -> Option<&'static [u8]> {
    let p = unsafe { getenv(cs(n)) };
    if p.is_null() { return None; }
    let mut l = 0;
    unsafe { while *p.add(l) != 0 { l += 1; } }
    Some(unsafe { core::slice::from_raw_parts(p as *const u8, l) })
}

#[no_mangle]
pub unsafe extern "C" fn main(_a: i32, _v: *mut *mut u8, _e: *mut *mut u8) -> i32 {
    // ---- strtol ----
    check("strtol_null_endptr", unsafe { strtol(cs(b"123\0"), null_mut(), 10) } == 123);
    check("strtol_null_endptr_junk", unsafe { strtol(cs(b"12x\0"), null_mut(), 0) } == 12);
    check("strtol_hex_base0", tl(b"0x1F\0", 0) == (31, 4, 0));
    check("strtol_hex_upper_X", tl(b"0X1f\0", 0) == (31, 4, 0));
    check("strtol_hex_base16_prefix", tl(b"0x1F\0", 16) == (31, 4, 0));
    check("strtol_octal_base0", tl(b"017\0", 0) == (15, 3, 0));
    check("strtol_zero_base0", tl(b"0\0", 0) == (0, 1, 0));
    check("strtol_dec_base0", tl(b"17\0", 0) == (17, 2, 0));
    check("strtol_08_base0", tl(b"08\0", 0) == (0, 1, 0));
    check("strtol_ws_sign", tl(b" \t\n -42x\0", 10) == (-42, 7, 0));
    check("strtol_plus", tl(b"+7\0", 10) == (7, 2, 0));
    check("strtol_empty", tl(b"\0", 10) == (0, 0, 0));
    check("strtol_nodigits", tl(b"abc\0", 10) == (0, 0, 0));
    check("strtol_only_ws", tl(b"   \0", 10) == (0, 0, 0));
    check("strtol_sign_only", tl(b"-\0", 10) == (0, 0, 0));
    check("strtol_0x_alone", tl(b"0x\0", 0) == (0, 1, 0));
    check("strtol_0x_g", tl(b"0xg\0", 16) == (0, 1, 0));
    {
        let r = tl(b"  -0x\0", 0);
        // glibc/musl: consumes "  -0" (4 bytes), value 0.
        check("strtol_ws_neg_0x_partial", r.0 == 0 && r.1 == 4 && r.2 == 0);
    }
    check("strtol_neg_hex", tl(b"-0x10\0", 0) == (-16, 5, 0));
    check("strtol_base36", tl(b"zz\0", 36) == (1295, 2, 0));
    check("strtol_bad_base1", { let r = tl(b"1\0", 1); r.0 == 0 && r.2 == EINVAL });
    check("strtol_bad_base37", { let r = tl(b"1\0", 37); r.0 == 0 && r.2 == EINVAL });
    check("strtol_overflow", tl(b"99999999999999999999\0", 10) == (c_long::MAX, 20, ERANGE));
    check("strtol_underflow", tl(b"-99999999999999999999\0", 10) == (c_long::MIN, 21, ERANGE));
    check("strtol_max", tl(b"9223372036854775807\0", 10) == (c_long::MAX, 19, 0));
    check("strtol_min", tl(b"-9223372036854775808\0", 10) == (c_long::MIN, 20, 0));
    check("strtol_max_plus1", tl(b"9223372036854775808\0", 10) == (c_long::MAX, 19, ERANGE));
    check("strtol_errno_untouched_on_ok", {
        set_errno(77);
        let v = unsafe { strtol(cs(b"5\0"), null_mut(), 10) };
        v == 5 && errno() == 77
    });
    // ---- strtoul ----
    check("strtoul_max", tul(b"18446744073709551615\0", 10) == (c_ulong::MAX, 20, 0));
    check("strtoul_overflow", tul(b"18446744073709551616\0", 10) == (c_ulong::MAX, 20, ERANGE));
    check("strtoul_neg", tul(b"-1\0", 10) == (c_ulong::MAX, 2, 0));
    check("strtoul_hex_base0", tul(b"0xdeadbeef\0", 0) == (0xdeadbeef, 10, 0));
    check("strtoul_empty", tul(b"\0", 0) == (0, 0, 0));
    check("strtoul_ws_hex_partial", { let r = tul(b" 0x\0", 0); r.0 == 0 && r.1 == 2 });
    check("strtoul_null_endptr", unsafe { strtoul(cs(b"0x20\0"), null_mut(), 0) } == 32);
    // ---- strtoll/strtoull ----
    check("strtoll_overflow", unsafe {
        set_errno(0);
        let v = strtoll(cs(b"99999999999999999999\0"), null_mut(), 10);
        v == i64::MAX && errno() == ERANGE
    });
    check("strtoull_hex", unsafe { strtoull(cs(b"0xffffffffffffffff\0"), null_mut(), 0) } == u64::MAX);
    // ---- ato* ----
    check("atoi_basic", unsafe { atoi(cs(b"  -12abc\0")) } == -12);
    check("atoi_empty", unsafe { atoi(cs(b"\0")) } == 0);
    check("atoi_hex_is_zero", unsafe { atoi(cs(b"0x10\0")) } == 0);
    check("atol_basic", unsafe { atol(cs(b"123456789012\0")) } == 123456789012);
    check("atof_basic", unsafe { atof(cs(b"2.5e1\0")) } == 25.0);
    // ---- strtod ----
    check("strtod_basic", td(b"1.5\0") == (1.5, 3, 0));
    check("strtod_null_endptr", unsafe { strtod(cs(b"4.25xyz\0"), null_mut()) } == 4.25);
    check("strtod_empty", td(b"\0") == (0.0, 0, 0));
    check("strtod_junk", td(b"xyz\0") == (0.0, 0, 0));
    check("strtod_ws_sign", td(b"  -2.5e2z\0") == (-250.0, 8, 0));
    check("strtod_dot_only", { let r = td(b".\0"); r.1 == 0 });
    check("strtod_dot5", td(b".5\0") == (0.5, 2, 0));
    check("strtod_5dot", td(b"5.\0") == (5.0, 2, 0));
    check("strtod_exp_partial", { let r = td(b"1e\0"); r.0 == 1.0 && r.1 == 1 });
    check("strtod_exp_sign_partial", { let r = td(b"1e+\0"); r.0 == 1.0 && r.1 == 1 });
    check("strtod_hex", td(b"0x1p4\0") == (16.0, 5, 0));
    check("strtod_hex_frac", td(b"0x1.8\0") == (1.5, 5, 0));
    check("strtod_inf", { let r = td(b"inf\0"); r.0 == f64::INFINITY && r.1 == 3 });
    check("strtod_INF_neg", { let r = td(b"-INF\0"); r.0 == f64::NEG_INFINITY && r.1 == 4 });
    check("strtod_infinity", { let r = td(b"infinity!\0"); r.0 == f64::INFINITY && r.1 == 8 });
    check("strtod_nan", { let r = td(b"nan\0"); r.0.is_nan() && r.1 == 3 });
    check("strtod_nan_payload", { let r = td(b"nan(0x1)x\0"); r.0.is_nan() && r.1 == 8 });
    check("strtod_denormal_1e-310", { let r = td(b"1e-310\0"); r.0 > 0.0 && r.0 < 2.3e-308 && r.1 == 6 });
    check("strtod_underflow_1e-400", { let r = td(b"1e-400\0"); r.0 == 0.0 && r.1 == 6 && r.2 == ERANGE });
    check("strtod_overflow_1e400", { let r = td(b"1e400\0"); r.0 == f64::INFINITY && r.1 == 5 && r.2 == ERANGE });
    check("strtod_huge_digits", { let r = td(b"123456789012345678901234567890\0"); r.0 > 1.2345e29 && r.0 < 1.2346e29 });
    check("strtod_long_frac", { let r = td(b"0.1000000000000000055511151231257827\0"); r.0 == 0.1 });
    check("strtof_basic", unsafe { strtof(cs(b"1.5\0"), null_mut()) } == 1.5);
    // ---- getenv & friends ----
    check("getenv_unset", genv(b"STRTEST_UNSET\0").is_none());
    check("getenv_null_name_empty", genv(b"\0").is_none());
    unsafe { setenv(cs(b"STRTEST_EMPTY\0"), cs(b"\0"), 1); }
    check("getenv_empty_value", genv(b"STRTEST_EMPTY\0") == Some(b""));
    unsafe { setenv(cs(b"STRTEST_A\0"), cs(b"one\0"), 1); }
    check("getenv_set", genv(b"STRTEST_A\0") == Some(b"one"));
    check("getenv_prefix_not_match", genv(b"STRTEST\0").is_none() && genv(b"STRTEST_AB\0").is_none());
    unsafe { setenv(cs(b"STRTEST_A\0"), cs(b"two\0"), 0); }
    check("setenv_no_overwrite", genv(b"STRTEST_A\0") == Some(b"one"));
    unsafe { setenv(cs(b"STRTEST_A\0"), cs(b"three\0"), 1); }
    check("setenv_overwrite", genv(b"STRTEST_A\0") == Some(b"three"));
    check("setenv_bad_name_eq", unsafe { set_errno(0); let r = setenv(cs(b"A=B\0"), cs(b"x\0"), 1); r == -1 && errno() == EINVAL });
    check("setenv_bad_name_empty", unsafe { set_errno(0); let r = setenv(cs(b"\0"), cs(b"x\0"), 1); r == -1 && errno() == EINVAL });
    static mut PE: [u8; 16] = *b"STRTEST_P=putv\0\0";
    unsafe { putenv(core::ptr::addr_of_mut!(PE) as *mut c_char); }
    check("putenv_visible", genv(b"STRTEST_P\0") == Some(b"putv"));
    // "NAME" with no '=' in putenv removes NAME (glibc)
    static mut PN: [u8; 16] = *b"STRTEST_P\0\0\0\0\0\0\0";
    unsafe { putenv(core::ptr::addr_of_mut!(PN) as *mut c_char); }
    check("putenv_noeq_unsets", genv(b"STRTEST_P\0").is_none());
    check("unsetenv_unset_ok", unsafe { unsetenv(cs(b"STRTEST_NEVER\0")) } == 0);
    check("unsetenv_removes", { unsafe { unsetenv(cs(b"STRTEST_A\0")); } genv(b"STRTEST_A\0").is_none() });
    check("unsetenv_bad_name", unsafe { set_errno(0); let r = unsetenv(cs(b"\0")); r == -1 && errno() == EINVAL });
    check("secure_getenv_set", {
        unsafe { setenv(cs(b"STRTEST_S\0"), cs(b"sec\0"), 1); }
        let p = unsafe { secure_getenv(cs(b"STRTEST_S\0")) };
        !p.is_null() && unsafe { *p } == b's' as c_char
    });
    check("secure_getenv_unset", unsafe { secure_getenv(cs(b"STRTEST_NOPE\0")) }.is_null());
    // Value pointer survives unrelated setenv (Mesa caches getenv results).
    check("getenv_ptr_stable_across_setenv", {
        unsafe { setenv(cs(b"STRTEST_K\0"), cs(b"keep\0"), 1); }
        let p = unsafe { getenv(cs(b"STRTEST_K\0")) };
        for i in 0..64u8 {
            let mut nm = *b"STRTEST_X00\0";
            nm[9] = b'0' + i / 10; nm[10] = b'0' + i % 10;
            unsafe { setenv(nm.as_ptr() as *const c_char, cs(b"v\0"), 1); }
        }
        let q = unsafe { getenv(cs(b"STRTEST_K\0")) };
        !p.is_null() && !q.is_null() && unsafe { core::slice::from_raw_parts(p as *const u8, 4) } == b"keep"
            && unsafe { core::slice::from_raw_parts(q as *const u8, 4) } == b"keep"
    });
    // Iterate environ while mutating via setenv/unsetenv: must not fault, and
    // every visited entry must be a NUL-terminated "NAME=..." or at least valid.
    check("environ_iterate_mutate", {
        let mut n = 0usize;
        let mut ok = true;
        unsafe {
            let mut i = 0usize;
            while !(*environ.add(i)).is_null() && n < 4096 {
                let p = *environ.add(i);
                if *p == 0 { ok = false; }
                if i == 1 { setenv(cs(b"STRTEST_M1\0"), cs(b"1\0"), 1); }
                if i == 2 { unsetenv(cs(b"STRTEST_K\0")); }
                i += 1; n += 1;
            }
        }
        ok && n < 4096
    });
    check("environ_null_terminated_after", unsafe {
        let mut i = 0usize;
        while !(*environ.add(i)).is_null() && i < 4096 { i += 1; }
        i < 4096
    });
    // Mesa idiom: getenv -> strtol on value, e.g. V3D_DEBUG-style number.
    check("env_to_strtol", {
        unsafe { setenv(cs(b"STRTEST_NUM\0"), cs(b"0x2f\0"), 1); }
        let p = unsafe { getenv(cs(b"STRTEST_NUM\0")) };
        !p.is_null() && unsafe { strtoul(p, null_mut(), 0) } == 0x2f
    });
    check("env_empty_to_strtol", {
        let p = unsafe { getenv(cs(b"STRTEST_EMPTY\0")) };
        let mut e: *mut c_char = null_mut();
        let v = unsafe { strtol(p, &mut e, 0) };
        v == 0 && e == p
    });
    let _ = null::<u8>();
    unsafe { FAILS }
}
