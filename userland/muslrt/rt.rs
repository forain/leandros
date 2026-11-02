//! Shared runtime shim for the no_std test programs linked against static
//! musl (`<arch>-unknown-linux-musl`).
//!
//! Include with `#[path = "../../muslrt/rt.rs"] mod rt;` plus
//! `#![feature(c_variadic)]` at the crate root.
//!
//! `syscall` is the raw kernel entry: it returns the kernel's `-errno`
//! unchanged instead of musl's `-1` + errno, so call sites that test for
//! `-22` (EINVAL) and friends keep working. It is a Rust item (not
//! `#[no_mangle]`), so it never collides with musl's own `syscall`.

#![allow(dead_code)]

/// Raw syscall with up to six integer arguments, kernel ABI (`-errno`).
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub unsafe fn raw_syscall(n: i64, a: [i64; 6]) -> i64 {
    let ret: i64;
    core::arch::asm!(
        "syscall",
        inlateout("rax") n => ret,
        in("rdi") a[0],
        in("rsi") a[1],
        in("rdx") a[2],
        in("r10") a[3],
        in("r8") a[4],
        in("r9") a[5],
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack),
    );
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn raw_syscall(n: i64, a: [i64; 6]) -> i64 {
    let ret: i64;
    core::arch::asm!(
        "svc 0",
        in("x8") n,
        inlateout("x0") a[0] => ret,
        in("x1") a[1],
        in("x2") a[2],
        in("x3") a[3],
        in("x4") a[4],
        in("x5") a[5],
        options(nostack),
    );
    ret
}

/// Variadic `syscall(nr, a0..a5)`, like the C function but returning the
/// kernel's `-errno`.
pub unsafe extern "C" fn syscall(n: i64, mut ap: ...) -> i64 {
    let a = [
        ap.arg::<i64>(),
        ap.arg::<i64>(),
        ap.arg::<i64>(),
        ap.arg::<i64>(),
        ap.arg::<i64>(),
        ap.arg::<i64>(),
    ];
    raw_syscall(n, a)
}

/// The prebuilt musl `compiler_builtins` references the unwinder personality;
/// these programs are panic=abort and never unwind.
#[no_mangle]
pub extern "C" fn rust_eh_personality() {}
