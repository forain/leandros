//! stdio: puts / putchar / getchar / fgets / write-only formatted I/O.
//!
//! `printf` and `fprintf` are implemented via a small AArch64 assembly thunk
//! that captures x1–x7 (the first 7 variadic integer arguments per AAPCS64)
//! into a stack array, then calls `printf_impl` with a pointer to that array.
//! This covers the vast majority of real printf call sites without requiring
//! Rust's unstable `#![feature(c_variadic)]`.

use crate::io::{read, write, STDIN_FILENO, STDOUT_FILENO, STDERR_FILENO};
use crate::string::strlen;

use crate::io::{c_int, size_t};

// ── Primitive output helpers ──────────────────────────────────────────────────

/// Write string `s` to stdout followed by newline.
#[no_mangle]
pub unsafe extern "C" fn puts(s: *const u8) -> c_int {
    if s.is_null() { return -1; }
    let len = strlen(s);
    write(STDOUT_FILENO, s, len);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    (len + 1) as c_int
}

/// Write a single character to stdout.
#[no_mangle]
pub unsafe extern "C" fn putchar(c: c_int) -> c_int {
    let b = c as u8;
    write(STDOUT_FILENO, &b, 1);
    c
}

/// Read one character from stdin; returns -1 on EOF.
#[no_mangle]
pub unsafe extern "C" fn getchar() -> c_int {
    let mut b = 0u8;
    if read(STDIN_FILENO, &mut b, 1) <= 0 { -1 } else { b as c_int }
}

/// Read a line from stdin into `buf` (at most `size-1` bytes + NUL).
#[no_mangle]
pub unsafe extern "C" fn fgets(buf: *mut u8, size: c_int, _stream: *mut u8) -> *mut u8 {
    let sz = (size - 1).max(0) as usize;
    let mut i = 0usize;
    loop {
        if i >= sz { break; }
        let c = getchar();
        if c < 0 { if i == 0 { return core::ptr::null_mut(); } break; }
        *buf.add(i) = c as u8; i += 1;
        if c as u8 == b'\n' { break; }
    }
    *buf.add(i) = 0;
    buf
}

// ── Internal formatter ────────────────────────────────────────────────────────

/// Format an unsigned integer in the given radix into a stack buffer.
/// Returns (bytes, length).
fn fmt_uint(mut n: u64, radix: u64, upper: bool) -> ([u8; 20], usize) {
    let digits = if upper { b"0123456789ABCDEF" } else { b"0123456789abcdef" };
    let mut buf = [0u8; 20];
    if n == 0 { buf[0] = b'0'; return (buf, 1); }
    let mut i = 20usize;
    while n > 0 { i -= 1; buf[i] = digits[(n % radix) as usize]; n /= radix; }
    let len = 20 - i;
    let mut out = [0u8; 20];
    out[..len].copy_from_slice(&buf[i..]);
    (out, len)
}

/// Sink that writes to either a file descriptor or a byte slice.
pub(crate) struct Sink<'a> {
    kind: SinkKind<'a>,
}
pub(crate) enum SinkKind<'a> {
    Fd(i32),
    Buf { data: &'a mut [u8], pos: usize },
}

impl<'a> Sink<'a> {
    fn emit_bytes(&mut self, b: &[u8]) {
        match &mut self.kind {
            SinkKind::Fd(fd) => unsafe { write(*fd, b.as_ptr(), b.len()); }
            SinkKind::Buf { data, pos } => {
                let avail = data.len().saturating_sub(*pos);
                let n = b.len().min(avail);
                data[*pos..*pos+n].copy_from_slice(&b[..n]);
                *pos += n;
            }
        }
    }
    fn emit_byte(&mut self, b: u8) { self.emit_bytes(core::slice::from_ref(&b)); }
    fn written(&self) -> usize {
        match &self.kind { SinkKind::Buf { pos, .. } => *pos, _ => 0 }
    }
}

/// Core formatter. `argv` points to an array of `u64` variadic arguments.
///
/// # Safety
/// `fmt` must be a NUL-terminated format string.
/// `argv` must contain at least as many `u64` values as there are conversion
/// specifiers in `fmt`.
pub(crate) unsafe fn do_format(sink: &mut Sink<'_>, fmt: *const u8, argv: *const u64) -> c_int {
    let mut count = 0i32;
    let mut fi    = 0usize; // index into fmt
    let mut ai    = 0usize; // index into argv

    macro_rules! next_arg {
        () => {{ let v = *argv.add(ai); ai += 1; v }}
    }

    loop {
        let c = *fmt.add(fi); fi += 1;
        if c == 0 { break; }
        if c != b'%' { sink.emit_byte(c); count += 1; continue; }

        // Parse flags.
        let left_align = *fmt.add(fi) == b'-'; if left_align { fi += 1; }
        let mut zero_pad = !left_align && *fmt.add(fi) == b'0'; if zero_pad { fi += 1; }

        // Parse width.
        let mut width = 0usize;
        while { let d = *fmt.add(fi); d >= b'0' && d <= b'9' } {
            width = width * 10 + (*fmt.add(fi) - b'0') as usize; fi += 1;
        }

        // Skip precision (with basic support for zero-padding)
        if *fmt.add(fi) == b'.' {
            fi += 1;
            let mut prec = 0usize;
            let mut has_prec = false;
            while { let d = *fmt.add(fi); d >= b'0' && d <= b'9' } {
                prec = prec * 10 + (*fmt.add(fi) - b'0') as usize;
                fi += 1;
                has_prec = true;
            }
            if has_prec && !zero_pad && width == 0 {
                zero_pad = true;
                width = prec;
            }
        }

        // Length modifier.
        let mut is_long = false;
        if *fmt.add(fi) == b'l' { is_long = true; fi += 1; if *fmt.add(fi) == b'l' { fi += 1; } }
        else if *fmt.add(fi) == b'z' { is_long = true; fi += 1; }
        let _ = is_long;

        let spec = *fmt.add(fi); fi += 1;
        let pad  = if zero_pad { b'0' } else { b' ' };

        // Emit with padding.
        let emit_padded = |sink: &mut Sink<'_>, data: &[u8], count: &mut i32| {
            let len = data.len();
            if !left_align && width > len {
                for _ in 0..(width-len) { sink.emit_byte(pad); *count += 1; }
            }
            sink.emit_bytes(data); *count += len as i32;
            if left_align && width > len {
                for _ in 0..(width-len) { sink.emit_byte(b' '); *count += 1; }
            }
        };

        match spec {
            b's' => {
                let p = next_arg!() as *const u8;
                if p.is_null() { emit_padded(sink, b"(null)", &mut count); }
                else { emit_padded(sink, core::slice::from_raw_parts(p, strlen(p)), &mut count); }
            }
            b'd' | b'i' => {
                let v = next_arg!() as i64;
                let (neg, abs) = if v < 0 { (true, (-v) as u64) } else { (false, v as u64) };
                let (nb, nl) = fmt_uint(abs, 10, false);
                let mut tmp = [0u8; 22]; let (s, tl);
                if neg { tmp[0] = b'-'; tmp[1..nl+1].copy_from_slice(&nb[..nl]); s = 0; tl = nl+1; }
                else   { tmp[..nl].copy_from_slice(&nb[..nl]); s = 0; tl = nl; }
                emit_padded(sink, &tmp[s..s+tl], &mut count);
            }
            b'u' => {
                let v = next_arg!();
                let (nb, nl) = fmt_uint(v, 10, false);
                emit_padded(sink, &nb[..nl], &mut count);
            }
            b'x' => {
                let v = next_arg!();
                let (nb, nl) = fmt_uint(v, 16, false);
                emit_padded(sink, &nb[..nl], &mut count);
            }
            b'X' => {
                let v = next_arg!();
                let (nb, nl) = fmt_uint(v, 16, true);
                emit_padded(sink, &nb[..nl], &mut count);
            }
            b'p' => {
                let v = next_arg!();
                let (nb, nl) = fmt_uint(v, 16, false);
                sink.emit_bytes(b"0x"); count += 2;
                emit_padded(sink, &nb[..nl], &mut count);
            }
            b'c' => {
                let v = next_arg!() as u8;
                emit_padded(sink, core::slice::from_ref(&v), &mut count);
            }
            b'%' => { sink.emit_byte(b'%'); count += 1; }
            _    => { sink.emit_byte(b'%'); sink.emit_byte(spec); count += 2; }
        }
    }
    count
}

// ── Public formatted I/O ──────────────────────────────────────────────────────
//
// On AArch64, AAPCS64 passes integer variadic args in x1–x7 then the stack.
// The assembly thunks below save x1–x7 to a `[u64; 7]` on the stack and
// then call the `_impl` function with a pointer to that array.
// This is sufficient for up to 7 formatted arguments — more than enough for
// typical printf calls.

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    // printf(fmt, ...) → printf_asm_impl(fmt, &args[7])
    ".global printf",
    ".type printf, %function",
    "printf:",
    "   sub  sp,  sp, #64",
    "   stp  x1,  x2, [sp, #0]",
    "   stp  x3,  x4, [sp, #16]",
    "   stp  x5,  x6, [sp, #32]",
    "   str  x7,      [sp, #48]",
    "   mov  x1,  sp",          // x1 = pointer to saved args
    "   bl   printf_impl",
    "   add  sp,  sp, #64",
    "   ret",

    // fprintf(fd_as_ptr, fmt, ...) → same idea
    ".global fprintf",
    ".type fprintf, %function",
    "fprintf:",
    "   sub  sp,  sp, #64",
    "   stp  x2,  x3, [sp, #0]",
    "   stp  x4,  x5, [sp, #16]",
    "   stp  x6,  x7, [sp, #32]",
    "   str  xzr,     [sp, #48]",
    "   mov  x2,  sp",
    "   bl   fprintf_impl",
    "   add  sp,  sp, #64",
    "   ret",

    // sprintf(buf, fmt, ...)
    ".global sprintf",
    ".type sprintf, %function",
    "sprintf:",
    "   sub  sp,  sp, #64",
    "   stp  x2,  x3, [sp, #0]",
    "   stp  x4,  x5, [sp, #16]",
    "   stp  x6,  x7, [sp, #32]",
    "   str  xzr,     [sp, #48]",
    "   mov  x2,  sp",
    "   bl   sprintf_impl",
    "   add  sp,  sp, #64",
    "   ret",

    // snprintf(buf, size, fmt, ...)
    ".global snprintf",
    ".type snprintf, %function",
    "snprintf:",
    "   sub  sp,  sp, #64",
    "   stp  x3,  x4, [sp, #0]",
    "   stp  x5,  x6, [sp, #16]",
    "   stp  x7, xzr, [sp, #32]",
    "   str  xzr,     [sp, #48]",
    "   mov  x3,  sp",
    "   bl   snprintf_impl",
    "   add  sp,  sp, #64",
    "   ret",
);

// x86_64 assembly for printf functions
// On x86_64, System V ABI passes integer args in rdi, rsi, rdx, rcx, r8, r9, then stack
#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    // printf(fmt, ...) → printf_impl(fmt, &args[6])
    ".global printf",
    ".type printf, @function",
    "printf:",
    "   sub  rsp, 56",          // 8 bytes alignment + 6*8 for args
    "   mov  [rsp+0],  rsi",    // save rsi (arg 1)
    "   mov  [rsp+8],  rdx",    // save rdx (arg 2)
    "   mov  [rsp+16], rcx",    // save rcx (arg 3)
    "   mov  [rsp+24], r8",     // save r8  (arg 4)
    "   mov  [rsp+32], r9",     // save r9  (arg 5)
    "   mov  qword ptr [rsp+40], 0", // padding
    "   mov  rsi, rsp",         // rsi = pointer to saved args
    "   call printf_impl",
    "   add  rsp, 56",
    "   ret",

    // fprintf(fd, fmt, ...) → fprintf_impl(fd, fmt, &args)
    ".global fprintf",
    ".type fprintf, @function",
    "fprintf:",
    "   sub  rsp, 56",
    "   mov  [rsp+0],  rdx",    // save rdx (arg 2, first vararg)
    "   mov  [rsp+8],  rcx",    // save rcx (arg 3)
    "   mov  [rsp+16], r8",     // save r8  (arg 4)
    "   mov  [rsp+24], r9",     // save r9  (arg 5)
    "   mov  qword ptr [rsp+32], 0",
    "   mov  qword ptr [rsp+40], 0",
    "   mov  rdx, rsp",         // rdx = pointer to saved args
    "   call fprintf_impl",
    "   add  rsp, 56",
    "   ret",

    // sprintf(buf, fmt, ...) → sprintf_impl(buf, fmt, &args)
    ".global sprintf",
    ".type sprintf, @function",
    "sprintf:",
    "   sub  rsp, 56",
    "   mov  [rsp+0],  rdx",    // save rdx (arg 2, first vararg)
    "   mov  [rsp+8],  rcx",    // save rcx (arg 3)
    "   mov  [rsp+16], r8",     // save r8  (arg 4)
    "   mov  [rsp+24], r9",     // save r9  (arg 5)
    "   mov  qword ptr [rsp+32], 0",
    "   mov  qword ptr [rsp+40], 0",
    "   mov  rdx, rsp",         // rdx = pointer to saved args
    "   call sprintf_impl",
    "   add  rsp, 56",
    "   ret",

    // snprintf(buf, size, fmt, ...) → snprintf_impl(buf, size, fmt, &args)
    ".global snprintf",
    ".type snprintf, @function",
    "snprintf:",
    "   sub  rsp, 56",
    "   mov  [rsp+0],  rcx",    // save rcx (arg 3, first vararg)
    "   mov  [rsp+8],  r8",     // save r8  (arg 4)
    "   mov  [rsp+16], r9",     // save r9  (arg 5)
    "   mov  qword ptr [rsp+24], 0",
    "   mov  qword ptr [rsp+32], 0",
    "   mov  qword ptr [rsp+40], 0",
    "   mov  rcx, rsp",         // rcx = pointer to saved args
    "   call snprintf_impl",
    "   add  rsp, 56",
    "   ret",
);

#[cfg(target_arch = "aarch64")]
#[no_mangle]
pub unsafe extern "C" fn printf_impl(fmt: *const u8, argv: *const u64) -> c_int {
    let mut sink = Sink { kind: SinkKind::Fd(STDOUT_FILENO) };
    do_format(&mut sink, fmt, argv)
}

#[cfg(target_arch = "aarch64")]
#[no_mangle]
pub unsafe extern "C" fn fprintf_impl(fd_ptr: *mut u8, fmt: *const u8, argv: *const u64) -> c_int {
    let mut sink = Sink { kind: SinkKind::Fd(fd_ptr as i32) };
    do_format(&mut sink, fmt, argv)
}

#[cfg(target_arch = "aarch64")]
#[no_mangle]
pub unsafe extern "C" fn sprintf_impl(buf: *mut u8, fmt: *const u8, argv: *const u64) -> c_int {
    let slice = core::slice::from_raw_parts_mut(buf, usize::MAX / 2);
    let mut sink = Sink { kind: SinkKind::Buf { data: slice, pos: 0 } };
    let n = do_format(&mut sink, fmt, argv);
    let pos = sink.written();
    if let SinkKind::Buf { data, .. } = sink.kind { data[pos] = 0; }
    n
}

#[cfg(target_arch = "aarch64")]
#[no_mangle]
pub unsafe extern "C" fn snprintf_impl(
    buf: *mut u8, size: size_t, fmt: *const u8, argv: *const u64,
) -> c_int {
    if size == 0 { return 0; }
    let slice = core::slice::from_raw_parts_mut(buf, size);
    let mut sink = Sink { kind: SinkKind::Buf { data: slice, pos: 0 } };
    let n = do_format(&mut sink, fmt, argv);
    let pos = sink.written().min(size - 1);
    if let SinkKind::Buf { data, .. } = sink.kind { data[pos] = 0; }
    n
}

#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn printf_impl(fmt: *const u8, argv: *const u64) -> c_int {
    let mut sink = Sink { kind: SinkKind::Fd(STDOUT_FILENO) };
    do_format(&mut sink, fmt, argv)
}

#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn fprintf_impl(fd_ptr: *mut u8, fmt: *const u8, argv: *const u64) -> c_int {
    let mut sink = Sink { kind: SinkKind::Fd(fd_ptr as i32) };
    do_format(&mut sink, fmt, argv)
}

#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn sprintf_impl(buf: *mut u8, fmt: *const u8, argv: *const u64) -> c_int {
    let slice = core::slice::from_raw_parts_mut(buf, usize::MAX / 2);
    let mut sink = Sink { kind: SinkKind::Buf { data: slice, pos: 0 } };
    let n = do_format(&mut sink, fmt, argv);
    let pos = sink.written();
    if let SinkKind::Buf { data, .. } = sink.kind { data[pos] = 0; }
    n
}

#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn snprintf_impl(
    buf: *mut u8, size: size_t, fmt: *const u8, argv: *const u64,
) -> c_int {
    if size == 0 { return 0; }
    let slice = core::slice::from_raw_parts_mut(buf, size);
    let mut sink = Sink { kind: SinkKind::Buf { data: slice, pos: 0 } };
    let n = do_format(&mut sink, fmt, argv);
    let pos = sink.written().min(size - 1);
    if let SinkKind::Buf { data, .. } = sink.kind { data[pos] = 0; }
    n
}

// Stub functions for other architectures (e.g., when running cargo check on host)
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub unsafe extern "C" fn printf_impl(_fmt: *const u8, _argv: *const u64) -> c_int { 0 }
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub unsafe extern "C" fn fprintf_impl(_s: *mut u8, _fmt: *const u8, _argv: *const u64) -> c_int { 0 }
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub unsafe extern "C" fn sprintf_impl(_b: *mut u8, _fmt: *const u8, _argv: *const u64) -> c_int { 0 }
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub unsafe extern "C" fn snprintf_impl(_b: *mut u8, _n: size_t, _fmt: *const u8, _a: *const u64) -> c_int { 0 }

/// `perror` — print `prefix: errno N` to stderr.
#[no_mangle]
pub unsafe extern "C" fn perror(msg: *const u8) {
    let fd = STDERR_FILENO;
    if !msg.is_null() && *msg != 0 {
        write(fd, msg, strlen(msg));
        write(fd, b": errno ".as_ptr(), 8);
    }
    let e = crate::errno::get_errno();
    let (eb, el) = int_dec(e);
    write(fd, eb.as_ptr(), el);
    write(fd, b"\n".as_ptr(), 1);
}

fn int_dec(mut n: i32) -> ([u8; 12], usize) {
    let mut buf = [0u8; 12];
    if n == 0 { buf[0] = b'0'; return (buf, 1); }
    let neg = n < 0; if neg { n = n.wrapping_neg(); }
    let mut i = 12usize;
    let mut m = n as u32;
    while m > 0 { i -= 1; buf[i] = b'0' + (m % 10) as u8; m /= 10; }
    if neg { i -= 1; buf[i] = b'-'; }
    let len = 12 - i;
    let mut out = [0u8; 12];
    out[..len].copy_from_slice(&buf[i..]);
    (out, len)
}
