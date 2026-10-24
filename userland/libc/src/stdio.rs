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

/// Integer / FP register slots the printf thunks save (see do_format).
pub(crate) const ARGV_INT: usize = 8;
pub(crate) const ARGV_FP: usize = 8;

/// Small stack buffer for float formatting (output past its end is cut).
struct FBuf { b: [u8; 352], n: usize }

impl FBuf {
    fn new() -> Self { FBuf { b: [0; 352], n: 0 } }
    fn as_bytes(&self) -> &[u8] { &self.b[..self.n] }
    fn push(&mut self, c: u8) { if self.n < self.b.len() { self.b[self.n] = c; self.n += 1; } }
}

impl core::fmt::Write for FBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &c in s.as_bytes() { self.push(c); }
        Ok(())
    }
}

/// C `%e` text: Rust prints `1.5e-7`, C wants `1.5e-07`.
fn fmt_exp(v: f64, prec: usize) -> (FBuf, i32) {
    use core::fmt::Write;
    let mut raw = FBuf::new();
    let _ = write!(raw, "{:.*e}", prec, v);
    let r = raw.as_bytes();
    let Some(epos) = r.iter().position(|&c| c == b'e') else { return (raw, 0) };
    let mut exp: i32 = 0;
    let (neg, digits) = if r.get(epos + 1) == Some(&b'-') { (true, &r[epos + 2..]) } else { (false, &r[epos + 1..]) };
    for &d in digits { exp = exp * 10 + (d - b'0') as i32; }
    if neg { exp = -exp; }
    let mut out = FBuf::new();
    for &c in &r[..epos] { out.push(c); }
    let _ = write!(out, "e{}{:02}", if neg { '-' } else { '+' }, exp.unsigned_abs());
    (out, exp)
}

/// Drop trailing fractional zeros (and a bare '.') before any exponent (%g).
fn strip_zeros(b: &FBuf) -> FBuf {
    let s = b.as_bytes();
    let epos = s.iter().position(|&c| c == b'e').unwrap_or(s.len());
    let mut end = epos;
    if s[..epos].contains(&b'.') {
        while end > 0 && s[end - 1] == b'0' { end -= 1; }
        if end > 0 && s[end - 1] == b'.' { end -= 1; }
    }
    let mut out = FBuf::new();
    for &c in &s[..end] { out.push(c); }
    for &c in &s[epos..] { out.push(c); }
    out
}

/// `%f %e %g` (and upper-case forms) with C's default precision of 6.
fn fmt_float(spec: u8, v: f64, prec: Option<usize>) -> FBuf {
    use core::fmt::Write;
    let mut out = if v.is_nan() || v.is_infinite() {
        let mut o = FBuf::new();
        let _ = o.write_str(if v.is_nan() { "nan" } else if v < 0.0 { "-inf" } else { "inf" });
        o
    } else {
        match spec | 0x20 {
            b'f' => { let mut o = FBuf::new(); let _ = write!(o, "{:.*}", prec.unwrap_or(6), v); o }
            b'e' => fmt_exp(v, prec.unwrap_or(6)).0,
            _ => {
                let p = match prec { Some(0) => 1, Some(p) => p, None => 6 };
                let x = if v == 0.0 { 0 } else { fmt_exp(v, p - 1).1 };
                if (p as i32) > x && x >= -4 {
                    let mut o = FBuf::new();
                    let _ = write!(o, "{:.*}", (p as i32 - 1 - x) as usize, v);
                    strip_zeros(&o)
                } else {
                    strip_zeros(&fmt_exp(v, p - 1).0)
                }
            }
        }
    };
    if spec.is_ascii_uppercase() { for c in &mut out.b[..out.n] { *c = c.to_ascii_uppercase(); } }
    out
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

    // argv layout (filled by the printf thunks): [0..ARGV_INT) integer
    // register args, [ARGV_INT..ARGV_INT+ARGV_FP) the FP/SIMD argument
    // registers (xmm0-7 / d0-d7), where C passes variadic doubles.
    let fpv = argv.add(ARGV_INT);
    let mut fai = 0usize;
    macro_rules! next_arg {
        () => {{ let v = if ai < ARGV_INT { *argv.add(ai) } else { 0 }; ai += 1; v }}
    }
    macro_rules! next_fp_arg {
        () => {{ let v = if fai < ARGV_FP { f64::from_bits(*fpv.add(fai)) } else { 0.0 }; fai += 1; v }}
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

        let (fwidth, fzero) = (width, zero_pad); // floats: before the int hack
        let mut fprec: Option<usize> = None;
        // Skip precision (with basic support for zero-padding)
        if *fmt.add(fi) == b'.' {
            fi += 1;
            fprec = Some(0);
            let mut prec = 0usize;
            let mut has_prec = false;
            while { let d = *fmt.add(fi); d >= b'0' && d <= b'9' } {
                prec = prec * 10 + (*fmt.add(fi) - b'0') as usize;
                fi += 1;
                has_prec = true;
            }
            if has_prec { fprec = Some(prec); }
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
            b'f' | b'F' | b'e' | b'E' | b'g' | b'G' => {
                let v = next_fp_arg!();
                let out = fmt_float(spec, v, fprec);
                let data = out.as_bytes();
                let len = data.len();
                let padn = if fwidth > len { fwidth - len } else { 0 };
                let finite = v.is_finite();
                if left_align {
                    sink.emit_bytes(data);
                    for _ in 0..padn { sink.emit_byte(b' '); }
                } else if fzero && finite {
                    let sign = if !data.is_empty() && data[0] == b'-' { 1 } else { 0 };
                    sink.emit_bytes(&data[..sign]);
                    for _ in 0..padn { sink.emit_byte(b'0'); }
                    sink.emit_bytes(&data[sign..]);
                } else {
                    for _ in 0..padn { sink.emit_byte(b' '); }
                    sink.emit_bytes(data);
                }
                count += (len + padn) as i32;
            }
            b'%' => { sink.emit_byte(b'%'); count += 1; }
            _    => { sink.emit_byte(b'%'); sink.emit_byte(spec); count += 2; }
        }
    }
    count
}

// ── Public formatted I/O ──────────────────────────────────────────────────────
//
// Variadic thunks: save the integer argument registers after the fixed ones
// to argv[0..8) and the FP/SIMD argument registers (where AAPCS64 and SysV
// pass variadic doubles; SysV's `al` count is an upper bound, saving all 8 is
// always safe) to argv[8..16), then call the `_impl` with that array.
// Arguments that spill to the stack are not supported.
#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".global printf",
    ".type printf, %function",
    "printf:",
    "   sub  sp,  sp, #144",
    "   stp  x29, x30, [sp, #128]",
    "   str  x1, [sp, #0]",
    "   str  x2, [sp, #8]",
    "   str  x3, [sp, #16]",
    "   str  x4, [sp, #24]",
    "   str  x5, [sp, #32]",
    "   str  x6, [sp, #40]",
    "   str  x7, [sp, #48]",
    "   stp  d0, d1, [sp, #64]",
    "   stp  d2, d3, [sp, #80]",
    "   stp  d4, d5, [sp, #96]",
    "   stp  d6, d7, [sp, #112]",
    "   mov  x1,  sp",
    "   bl   printf_impl",
    "   ldp  x29, x30, [sp, #128]",
    "   add  sp,  sp, #144",
    "   ret",

    ".global fprintf",
    ".type fprintf, %function",
    "fprintf:",
    "   sub  sp,  sp, #144",
    "   stp  x29, x30, [sp, #128]",
    "   str  x2, [sp, #0]",
    "   str  x3, [sp, #8]",
    "   str  x4, [sp, #16]",
    "   str  x5, [sp, #24]",
    "   str  x6, [sp, #32]",
    "   str  x7, [sp, #40]",
    "   stp  d0, d1, [sp, #64]",
    "   stp  d2, d3, [sp, #80]",
    "   stp  d4, d5, [sp, #96]",
    "   stp  d6, d7, [sp, #112]",
    "   mov  x2,  sp",
    "   bl   fprintf_impl",
    "   ldp  x29, x30, [sp, #128]",
    "   add  sp,  sp, #144",
    "   ret",

    ".global sprintf",
    ".type sprintf, %function",
    "sprintf:",
    "   sub  sp,  sp, #144",
    "   stp  x29, x30, [sp, #128]",
    "   str  x2, [sp, #0]",
    "   str  x3, [sp, #8]",
    "   str  x4, [sp, #16]",
    "   str  x5, [sp, #24]",
    "   str  x6, [sp, #32]",
    "   str  x7, [sp, #40]",
    "   stp  d0, d1, [sp, #64]",
    "   stp  d2, d3, [sp, #80]",
    "   stp  d4, d5, [sp, #96]",
    "   stp  d6, d7, [sp, #112]",
    "   mov  x2,  sp",
    "   bl   sprintf_impl",
    "   ldp  x29, x30, [sp, #128]",
    "   add  sp,  sp, #144",
    "   ret",

    ".global snprintf",
    ".type snprintf, %function",
    "snprintf:",
    "   sub  sp,  sp, #144",
    "   stp  x29, x30, [sp, #128]",
    "   str  x3, [sp, #0]",
    "   str  x4, [sp, #8]",
    "   str  x5, [sp, #16]",
    "   str  x6, [sp, #24]",
    "   str  x7, [sp, #32]",
    "   stp  d0, d1, [sp, #64]",
    "   stp  d2, d3, [sp, #80]",
    "   stp  d4, d5, [sp, #96]",
    "   stp  d6, d7, [sp, #112]",
    "   mov  x3,  sp",
    "   bl   snprintf_impl",
    "   ldp  x29, x30, [sp, #128]",
    "   add  sp,  sp, #144",
    "   ret",
);


// x86_64 assembly for printf functions
// On x86_64, System V ABI passes integer args in rdi, rsi, rdx, rcx, r8, r9, then stack
#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".global printf",
    ".type printf, @function",
    "printf:",
    "   sub  rsp, 136",
    "   mov  [rsp+0], rsi",
    "   mov  [rsp+8], rdx",
    "   mov  [rsp+16], rcx",
    "   mov  [rsp+24], r8",
    "   mov  [rsp+32], r9",
    "   movq [rsp+64], xmm0",
    "   movq [rsp+72], xmm1",
    "   movq [rsp+80], xmm2",
    "   movq [rsp+88], xmm3",
    "   movq [rsp+96], xmm4",
    "   movq [rsp+104], xmm5",
    "   movq [rsp+112], xmm6",
    "   movq [rsp+120], xmm7",
    "   mov  rsi, rsp",
    "   call printf_impl",
    "   add  rsp, 136",
    "   ret",

    ".global fprintf",
    ".type fprintf, @function",
    "fprintf:",
    "   sub  rsp, 136",
    "   mov  [rsp+0], rdx",
    "   mov  [rsp+8], rcx",
    "   mov  [rsp+16], r8",
    "   mov  [rsp+24], r9",
    "   movq [rsp+64], xmm0",
    "   movq [rsp+72], xmm1",
    "   movq [rsp+80], xmm2",
    "   movq [rsp+88], xmm3",
    "   movq [rsp+96], xmm4",
    "   movq [rsp+104], xmm5",
    "   movq [rsp+112], xmm6",
    "   movq [rsp+120], xmm7",
    "   mov  rdx, rsp",
    "   call fprintf_impl",
    "   add  rsp, 136",
    "   ret",

    ".global sprintf",
    ".type sprintf, @function",
    "sprintf:",
    "   sub  rsp, 136",
    "   mov  [rsp+0], rdx",
    "   mov  [rsp+8], rcx",
    "   mov  [rsp+16], r8",
    "   mov  [rsp+24], r9",
    "   movq [rsp+64], xmm0",
    "   movq [rsp+72], xmm1",
    "   movq [rsp+80], xmm2",
    "   movq [rsp+88], xmm3",
    "   movq [rsp+96], xmm4",
    "   movq [rsp+104], xmm5",
    "   movq [rsp+112], xmm6",
    "   movq [rsp+120], xmm7",
    "   mov  rdx, rsp",
    "   call sprintf_impl",
    "   add  rsp, 136",
    "   ret",

    ".global snprintf",
    ".type snprintf, @function",
    "snprintf:",
    "   sub  rsp, 136",
    "   mov  [rsp+0], rcx",
    "   mov  [rsp+8], r8",
    "   mov  [rsp+16], r9",
    "   movq [rsp+64], xmm0",
    "   movq [rsp+72], xmm1",
    "   movq [rsp+80], xmm2",
    "   movq [rsp+88], xmm3",
    "   movq [rsp+96], xmm4",
    "   movq [rsp+104], xmm5",
    "   movq [rsp+112], xmm6",
    "   movq [rsp+120], xmm7",
    "   mov  rcx, rsp",
    "   call snprintf_impl",
    "   add  rsp, 136",
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
