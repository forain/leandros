//! dbusprobe — a minimal raw-wire D-Bus client, written to exercise busd's
//! `.service`-file activation (`ports/busd/start-service-activation.patch`):
//! `ListActivatableNames`, `StartServiceByName`, and implicit activation on
//! an unowned-but-activatable name, plus the ServiceUnknown-fast-path
//! regression that `service-unknown-reply.patch` fixed.
//!
//! No `dbus` crate exists for this target, and the repo is Rust-only (no C
//! `libdbus` binding), so this speaks the wire protocol directly: a plain
//! AF_UNIX stream socket, the `EXTERNAL` SASL handshake, and hand-rolled
//! D-Bus marshalling for exactly the messages this probe needs. Follows
//! `userland/scmtest/src/main.rs`'s conventions — plain `leandros-libc` (no
//! relibc/TLS needed), syscalls `leandros-libc` doesn't wrap yet
//! (socket/connect) made directly via `syscall3`, matching the
//! `SYS_SOCKET`/`SYS_CONNECT` numbers and `sockaddr_un` shape scmtest
//! already uses for its AF_UNIX socket-node tests.
//!
//! # Usage
//!
//! `dbusprobe` (no args) — connects, runs the activation-evidence sequence
//! below, and exits. `dbusprobe --serve <well-known-name>` — connects,
//! calls `RequestName`, then serves forever, answering every method call
//! addressed to it with an empty `method_return`. This is what the scratch
//! `org.leandros.ActivationProbe.service`'s `Exec=` line spawns, so
//! `StartServiceByName`/implicit activation have something to actually
//! observe claiming the name inside busd's 5s `ACTIVATION_TIMEOUT`
//! (`ports/busd/start-service-activation.patch`) instead of just spawning a
//! process that exits without ever owning the name.
//!
//! Both modes read the bus address the same way: `$DBUS_SESSION_BUS_ADDRESS`
//! (the `path=` component), falling back to `unix:path=/run/user/0/bus`.
//!
//! Exit code: steps 3-7 (see `main`) each need only *some* reply — a D-Bus
//! error reply is just as much evidence the wire protocol works as a
//! success reply is. Only a missing reply (socket/protocol failure — the
//! peer never answered, or the connection died) counts as a failure.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

extern crate leandros_libc;
use leandros_libc::*;
use leandros_libc::syscall::syscall3;

// ── Socket syscalls leandros-libc doesn't wrap yet ──────────────────────────
// Numbers match kernel/src/syscall.rs's `mod nr` tables, same source
// userland/scmtest/src/main.rs's SYS_SOCKET/SYS_CONNECT block cites.
#[cfg(target_arch = "aarch64")] const SYS_SOCKET:  usize = 198;
#[cfg(target_arch = "x86_64")]  const SYS_SOCKET:  usize = 41;
#[cfg(target_arch = "aarch64")] const SYS_CONNECT: usize = 203;
#[cfg(target_arch = "x86_64")]  const SYS_CONNECT: usize = 42;

const AF_UNIX: i32 = 1;
const SOCK_STREAM: i32 = 1;

/// `sockaddr_un`: sun_family(2) + sun_path(108), matching
/// userland/scmtest/src/main.rs's struct of the same name.
#[repr(C)]
struct sockaddr_un {
    sun_family: u16,
    sun_path: [u8; 108],
}

impl sockaddr_un {
    /// Build from a pathname (no NUL in `name`); addrlen = 2 + strlen + 1,
    /// the same convention musl uses.
    fn from_path(name: &[u8]) -> (sockaddr_un, usize) {
        let mut a = sockaddr_un { sun_family: AF_UNIX as u16, sun_path: [0u8; 108] };
        let n = name.len().min(107);
        a.sun_path[..n].copy_from_slice(&name[..n]);
        (a, 2 + n + 1)
    }
}

fn xret(r: isize) -> isize {
    if r < 0 { set_errno(-r as i32); -1 } else { r }
}

unsafe fn raw_socket(domain: i32, kind: i32, proto: i32) -> i32 {
    xret(syscall3(SYS_SOCKET, domain as usize, kind as usize, proto as usize)) as i32
}
unsafe fn raw_connect(fd: i32, addr: *const sockaddr_un, addrlen: usize) -> isize {
    xret(syscall3(SYS_CONNECT, fd as usize, addr as *const _ as usize, addrlen))
}

// ── printf-based diagnostics (see userland/libc/src/stdio.rs) ──────────────

extern "C" {
    fn printf(fmt: *const u8, a0: u64, a1: u64, a2: u64, a3: u64) -> i32;
}
unsafe fn dbg0(fmt: &[u8]) { printf(fmt.as_ptr(), 0, 0, 0, 0); }
unsafe fn dbg1(fmt: &[u8], a: i64) { printf(fmt.as_ptr(), a as u64, 0, 0, 0); }
unsafe fn dbg2(fmt: &[u8], a: i64, b: i64) { printf(fmt.as_ptr(), a as u64, b as u64, 0, 0); }
unsafe fn dbg_p(fmt: &[u8], p: *const u8) { printf(fmt.as_ptr(), p as u64, 0, 0, 0); }
unsafe fn dbg_p1(fmt: &[u8], p: *const u8, a: i64) { printf(fmt.as_ptr(), p as u64, a as u64, 0, 0); }
unsafe fn dbg_pp(fmt: &[u8], p0: *const u8, p1: *const u8) { printf(fmt.as_ptr(), p0 as u64, p1 as u64, 0, 0); }
unsafe fn dbg_u1(fmt: &[u8], u: u32, a: i64) { printf(fmt.as_ptr(), u as u64, a as u64, 0, 0); }
unsafe fn dbg_pu(fmt: &[u8], p: *const u8, u: u32) { printf(fmt.as_ptr(), p as u64, u as u64, 0, 0); }

// ── D-Bus wire encoding ──────────────────────────────────────────────────────
//
// Header-field codes (D-Bus spec):
const FIELD_PATH: u8         = 1; // o
const FIELD_INTERFACE: u8    = 2; // s
const FIELD_MEMBER: u8       = 3; // s
const FIELD_ERROR_NAME: u8   = 4; // s
const FIELD_REPLY_SERIAL: u8 = 5; // u
const FIELD_DESTINATION: u8  = 6; // s
const FIELD_SENDER: u8       = 7; // s

const MSG_METHOD_CALL: u8   = 1;
const MSG_METHOD_RETURN: u8 = 2;
const MSG_ERROR: u8         = 3;
const MSG_SIGNAL: u8        = 4;

fn align_to(pos: usize, n: usize) -> usize { (pos + n - 1) / n * n }

/// Pad `buf[pos..]` with zero bytes up to the next `n`-byte boundary;
/// returns the new (aligned) position.
fn put_pad(buf: &mut [u8], pos: usize, n: usize) -> usize {
    let np = align_to(pos, n);
    for b in &mut buf[pos..np] { *b = 0; }
    np
}

fn put_u32(buf: &mut [u8], pos: usize, v: u32) -> usize {
    let p = put_pad(buf, pos, 4);
    buf[p..p + 4].copy_from_slice(&v.to_le_bytes());
    p + 4
}

/// STRING/OBJECT_PATH value: u32 len (4-aligned) + bytes + NUL.
fn put_dbus_string(buf: &mut [u8], pos: usize, s: &[u8]) -> usize {
    let mut p = put_u32(buf, pos, s.len() as u32);
    buf[p..p + s.len()].copy_from_slice(s);
    p += s.len();
    buf[p] = 0;
    p + 1
}

/// One `(yv)` header-field struct with an `s`/`o`-typed value.
fn put_header_field_string(buf: &mut [u8], pos: usize, code: u8, sig_char: u8, s: &[u8]) -> usize {
    let mut p = put_pad(buf, pos, 8);
    buf[p] = code; p += 1;
    buf[p] = 1; p += 1;           // variant signature length
    buf[p] = sig_char; p += 1;    // 's' or 'o'
    buf[p] = 0; p += 1;           // signature NUL
    put_dbus_string(buf, p, s)
}

/// One `(yv)` header-field struct with a `g`-typed (SIGNATURE) value.
fn put_header_field_sig(buf: &mut [u8], pos: usize, code: u8, s: &[u8]) -> usize {
    let mut p = put_pad(buf, pos, 8);
    buf[p] = code; p += 1;
    buf[p] = 1; p += 1; buf[p] = b'g'; p += 1; buf[p] = 0; p += 1;
    // SIGNATURE value: u8 len + bytes + NUL, no extra alignment.
    buf[p] = s.len() as u8; p += 1;
    buf[p..p + s.len()].copy_from_slice(s); p += s.len();
    buf[p] = 0; p + 1
}

/// One `(yv)` header-field struct with a `u`-typed value.
fn put_header_field_u32(buf: &mut [u8], pos: usize, code: u8, v: u32) -> usize {
    let mut p = put_pad(buf, pos, 8);
    buf[p] = code; p += 1;
    buf[p] = 1; p += 1; buf[p] = b'u'; p += 1; buf[p] = 0; p += 1;
    put_u32(buf, p, v)
}

/// Encode an "su" body (StartServiceByName/RequestName argument shape):
/// a well-known name followed by a uint32 flags word.
fn encode_su(buf: &mut [u8], name: &[u8], flags: u32) -> usize {
    let p = put_dbus_string(buf, 0, name);
    put_u32(buf, p, flags)
}

/// Build a full `method_call` message. `dest`/`body_sig` are omitted from
/// the header when `None`; `body` must already be pre-encoded (e.g. via
/// `encode_su`) and match `body_sig`.
fn build_call(
    buf: &mut [u8], serial: u32,
    path: &[u8], iface: &[u8], member: &[u8], dest: Option<&[u8]>,
    body: &[u8], body_sig: Option<&[u8]>,
) -> usize {
    buf[0] = b'l';               // little-endian
    buf[1] = MSG_METHOD_CALL;
    buf[2] = 0;                  // flags
    buf[3] = 1;                  // protocol version
    buf[4..8].copy_from_slice(&(body.len() as u32).to_le_bytes());
    buf[8..12].copy_from_slice(&serial.to_le_bytes());

    let mut p = 16usize; // buf[12..16] (array byte-count) filled in below
    p = put_header_field_string(buf, p, FIELD_PATH, b'o', path);
    p = put_header_field_string(buf, p, FIELD_INTERFACE, b's', iface);
    p = put_header_field_string(buf, p, FIELD_MEMBER, b's', member);
    if let Some(d) = dest { p = put_header_field_string(buf, p, FIELD_DESTINATION, b's', d); }
    if let Some(sig) = body_sig { p = put_header_field_sig(buf, p, 8, sig); }

    let array_len = (p - 16) as u32;
    buf[12..16].copy_from_slice(&array_len.to_le_bytes());

    p = put_pad(buf, p, 8);
    buf[p..p + body.len()].copy_from_slice(body);
    p + body.len()
}

/// Build a `method_return` reply with an empty body — everything
/// `--serve`'s answer to any incoming method call needs.
fn build_reply(buf: &mut [u8], serial: u32, reply_serial: u32, dest: Option<&[u8]>) -> usize {
    buf[0] = b'l';
    buf[1] = MSG_METHOD_RETURN;
    buf[2] = 0;
    buf[3] = 1;
    buf[4..8].copy_from_slice(&0u32.to_le_bytes());
    buf[8..12].copy_from_slice(&serial.to_le_bytes());

    let mut p = 16usize;
    p = put_header_field_u32(buf, p, FIELD_REPLY_SERIAL, reply_serial);
    if let Some(d) = dest { p = put_header_field_string(buf, p, FIELD_DESTINATION, b's', d); }

    let array_len = (p - 16) as u32;
    buf[12..16].copy_from_slice(&array_len.to_le_bytes());
    put_pad(buf, p, 8)
}

// ── D-Bus wire decoding ──────────────────────────────────────────────────────

fn read_u32_at(buf: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]])
}

/// A parsed message: header-field values we care about are byte ranges
/// (offset, len) into the *same* `buf` the caller passed to `recv_message`
/// — no copying, no borrow of `buf` held past the call (avoids fighting the
/// borrow checker across the wait_for_reply retry loop).
struct RecvMsg {
    mtype: u8,
    msg_serial: u32,
    reply_serial: Option<u32>,
    error_name: Option<(usize, usize)>,
    sender: Option<(usize, usize)>,
    body_start: usize,
    // Not read by any caller today (body parsing walks its own array-length
    // prefix instead), but kept for parity with `body_start` / future callers.
    #[allow(dead_code)]
    body_len: usize,
}

unsafe fn read_exact(fd: i32, buf: &mut [u8]) -> bool {
    let mut n = 0usize;
    while n < buf.len() {
        let r = read(fd, buf.as_mut_ptr().add(n), buf.len() - n);
        if r <= 0 { return false; }
        n += r as usize;
    }
    true
}

/// Read one full D-Bus message off `fd` into `buf`. Only little-endian
/// ('l') messages are supported — busd (like us) is native-endian on both
/// our target arches, so this is never exercised in practice.
unsafe fn recv_message(fd: i32, buf: &mut [u8]) -> Option<RecvMsg> {
    if !read_exact(fd, &mut buf[0..16]) { return None; }
    if buf[0] != b'l' { return None; }
    let mtype = buf[1];
    let body_len = read_u32_at(buf, 4) as usize;
    let msg_serial = read_u32_at(buf, 8);
    let array_len = read_u32_at(buf, 12) as usize;
    let body_start = align_to(16 + array_len, 8);
    let total = body_start + body_len;
    if total > buf.len() { return None; }
    if total > 16 && !read_exact(fd, &mut buf[16..total]) { return None; }

    let mut pos = 16usize;
    let end = 16 + array_len;
    let mut reply_serial = None;
    let mut error_name = None;
    let mut sender = None;
    while pos < end {
        pos = align_to(pos, 8);
        if pos >= end { break; }
        let code = buf[pos]; pos += 1;
        let sig_len = buf[pos] as usize; pos += 1;
        let sig0 = if sig_len > 0 { buf[pos] } else { 0 };
        pos += sig_len; pos += 1; // signature bytes + NUL
        match sig0 {
            b'o' | b's' => {
                pos = align_to(pos, 4);
                let len = read_u32_at(buf, pos) as usize; pos += 4;
                let vstart = pos; pos += len; pos += 1;
                if code == FIELD_ERROR_NAME { error_name = Some((vstart, len)); }
                if code == FIELD_SENDER { sender = Some((vstart, len)); }
            }
            b'g' => {
                let len = buf[pos] as usize; pos += 1;
                pos += len; pos += 1;
            }
            b'u' => {
                pos = align_to(pos, 4);
                let v = read_u32_at(buf, pos); pos += 4;
                if code == FIELD_REPLY_SERIAL { reply_serial = Some(v); }
            }
            b'y' => { pos += 1; }
            b'b' | b'i' => { pos = align_to(pos, 4); pos += 4; }
            b'n' | b'q' => { pos = align_to(pos, 2); pos += 2; }
            b'x' | b't' | b'd' => { pos = align_to(pos, 8); pos += 8; }
            _ => break, // unexpected/array-typed header field — stop, best effort
        }
    }
    Some(RecvMsg { mtype, msg_serial, reply_serial, error_name, sender, body_start, body_len })
}

/// Read messages until one whose REPLY_SERIAL matches `expected` shows up
/// (skipping signals such as NameAcquired), or give up after a bounded
/// number of unrelated messages. Returns `None` on a read/protocol error —
/// the only case that counts as a probe failure (see module doc).
unsafe fn wait_for_reply(fd: i32, expected: u32, buf: &mut [u8]) -> Option<RecvMsg> {
    for _ in 0..16 {
        match recv_message(fd, buf) {
            None => return None,
            Some(m) => {
                if m.mtype == MSG_SIGNAL { continue; }
                if m.reply_serial == Some(expected) { return Some(m); }
            }
        }
    }
    None
}

// ── SASL handshake helpers ──────────────────────────────────────────────────

/// uid -> ASCII-hex of its decimal digits, e.g. uid 0 -> "0" -> "30".
fn uid_to_hex(uid: u32, out: &mut [u8]) -> usize {
    let mut dec = [0u8; 10];
    let dl = if uid == 0 {
        dec[0] = b'0';
        1
    } else {
        let mut v = uid;
        let mut tmp = [0u8; 10];
        let mut tl = 0usize;
        while v > 0 { tmp[tl] = b'0' + (v % 10) as u8; v /= 10; tl += 1; }
        for i in 0..tl { dec[i] = tmp[tl - 1 - i]; }
        tl
    };
    const HEX: &[u8] = b"0123456789abcdef";
    let mut o = 0usize;
    for &b in &dec[..dl] {
        out[o] = HEX[(b >> 4) as usize]; o += 1;
        out[o] = HEX[(b & 0xF) as usize]; o += 1;
    }
    o
}

/// Read a CRLF-terminated line (the CRLF is stripped). Returns -1 on
/// read error/EOF.
unsafe fn read_line(fd: i32, buf: &mut [u8]) -> isize {
    let mut n = 0usize;
    loop {
        if n >= buf.len() { return n as isize; }
        let mut b = [0u8; 1];
        let r = read(fd, b.as_mut_ptr(), 1);
        if r <= 0 { return -1; }
        if b[0] == b'\n' {
            if n > 0 && buf[n - 1] == b'\r' { n -= 1; }
            return n as isize;
        }
        buf[n] = b[0]; n += 1;
    }
}

// ── argv/envp helpers ────────────────────────────────────────────────────────

unsafe fn streq(a: *const u8, b: &[u8]) -> bool {
    let mut i = 0usize;
    loop {
        let ca = *a.add(i);
        let cb = if i < b.len() { b[i] } else { 0 };
        if ca != cb { return false; }
        if ca == 0 { return true; }
        i += 1;
    }
}

/// Scan a NUL-terminated `envp` for `key=`; return a pointer to the value
/// (a NUL-terminated C string) or None.
unsafe fn env_lookup(envp: *const *const u8, key: &[u8]) -> Option<*const u8> {
    if envp.is_null() { return None; }
    let mut pp = envp;
    while !(*pp).is_null() {
        let s = *pp;
        let mut i = 0usize;
        let mut matched = true;
        while i < key.len() {
            if *s.add(i) != key[i] { matched = false; break; }
            i += 1;
        }
        if matched && *s.add(key.len()) == b'=' { return Some(s.add(key.len() + 1)); }
        pp = pp.add(1);
    }
    None
}

/// Find `path=` inside a D-Bus address string (e.g.
/// `unix:path=/run/user/0/bus,guid=...`) and copy the path component (up to
/// the next `,` or NUL) into `out`. Returns the copied length, or None if
/// `path=` isn't present.
unsafe fn find_and_copy_path(s: *const u8, out: &mut [u8; 108]) -> Option<usize> {
    const NEEDLE: &[u8] = b"path=";
    let mut i = 0usize;
    loop {
        let c = *s.add(i);
        if c == 0 { return None; }
        let mut j = 0usize;
        let mut m = true;
        while j < NEEDLE.len() {
            if *s.add(i + j) != NEEDLE[j] { m = false; break; }
            j += 1;
        }
        if m {
            let start = i + NEEDLE.len();
            let mut k = 0usize;
            loop {
                if k >= out.len() { break; }
                let c2 = *s.add(start + k);
                if c2 == 0 || c2 == b',' { break; }
                out[k] = c2; k += 1;
            }
            return Some(k);
        }
        i += 1;
    }
}

const DEFAULT_BUS_PATH: &[u8] = b"/run/user/0/bus";

unsafe fn resolve_bus_path(envp: *const *const u8, out: &mut [u8; 108]) -> usize {
    if let Some(val) = env_lookup(envp, b"DBUS_SESSION_BUS_ADDRESS") {
        if let Some(n) = find_and_copy_path(val, out) {
            if n > 0 { return n; }
        }
    }
    out[..DEFAULT_BUS_PATH.len()].copy_from_slice(DEFAULT_BUS_PATH);
    DEFAULT_BUS_PATH.len()
}

unsafe fn now_ms() -> i64 {
    let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
    clock_gettime(CLOCK_MONOTONIC, &mut ts);
    ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000
}

// ── Connect + SASL EXTERNAL handshake (shared by probe and --serve modes) ──

unsafe fn connect_and_handshake(envp: *const *const u8) -> Option<i32> {
    let mut path_buf = [0u8; 108];
    let path_len = resolve_bus_path(envp, &mut path_buf);
    if path_len < path_buf.len() { path_buf[path_len] = 0; } // NUL for %s
    dbg_p(b"dbusprobe: connecting to %s\n\0", path_buf.as_ptr());

    let fd = raw_socket(AF_UNIX, SOCK_STREAM, 0);
    if fd < 0 { dbg0(b"dbusprobe: socket() failed\n\0"); return None; }

    let (addr, alen) = sockaddr_un::from_path(&path_buf[..path_len]);
    if raw_connect(fd, &addr, alen) != 0 {
        dbg1(b"dbusprobe: connect() failed errno=%d\n\0", get_errno() as i64);
        close(fd);
        return None;
    }

    // SASL: leading NUL, then "AUTH EXTERNAL <hex-uid>\r\n".
    let nul = [0u8];
    if write(fd, nul.as_ptr(), 1) != 1 { close(fd); return None; }

    let uid = getuid();
    let mut hexbuf = [0u8; 32];
    let hexlen = uid_to_hex(uid, &mut hexbuf);

    let mut authline = [0u8; 64];
    let mut ap = 0usize;
    const PREFIX: &[u8] = b"AUTH EXTERNAL ";
    authline[ap..ap + PREFIX.len()].copy_from_slice(PREFIX); ap += PREFIX.len();
    authline[ap..ap + hexlen].copy_from_slice(&hexbuf[..hexlen]); ap += hexlen;
    authline[ap] = b'\r'; ap += 1;
    authline[ap] = b'\n'; ap += 1;
    if write(fd, authline.as_ptr(), ap as usize) != ap as isize {
        dbg0(b"dbusprobe: AUTH EXTERNAL write failed\n\0"); close(fd); return None;
    }

    let mut line = [0u8; 256];
    let n = read_line(fd, &mut line);
    if n < 2 || &line[0..2] != b"OK" {
        dbg0(b"dbusprobe: SASL AUTH not OK\n\0");
        close(fd);
        return None;
    }

    const BEGIN: &[u8] = b"BEGIN\r\n";
    if write(fd, BEGIN.as_ptr(), BEGIN.len()) != BEGIN.len() as isize {
        dbg0(b"dbusprobe: BEGIN write failed\n\0"); close(fd); return None;
    }
    Some(fd)
}

// ── --serve mode ─────────────────────────────────────────────────────────────

/// Answer every incoming method call with an empty `method_return`, forever.
/// This is what lets busd's activation of `org.leandros.ActivationProbe.service`
/// actually claim the name within `ACTIVATION_TIMEOUT`, instead of the
/// spawned process exiting without ever owning it.
unsafe fn serve_loop(fd: i32) -> ! {
    let mut serial: u32 = 1000;
    let mut buf = [0u8; 4096];
    loop {
        match recv_message(fd, &mut buf) {
            None => {
                dbg0(b"dbusprobe: --serve: connection closed, exiting\n\0");
                exit(1);
            }
            Some(m) => {
                if m.mtype == MSG_METHOD_CALL {
                    let dest = m.sender.map(|(s, l)| &buf[s..s + l]);
                    let mut rbuf = [0u8; 256];
                    let n = build_reply(&mut rbuf, serial, m.msg_serial, dest);
                    serial = serial.wrapping_add(1);
                    let _ = write(fd, rbuf.as_ptr(), n);
                }
                // Signals/replies addressed to us (there shouldn't be any
                // outstanding calls of our own) are simply ignored.
            }
        }
    }
}

// ── main ─────────────────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, envp: *const *const u8) -> i32 {
    if argc >= 3 && streq(*argv.add(1), b"--serve") {
        let name_ptr = *argv.add(2);
        let name_len = strlen(name_ptr);
        let name = core::slice::from_raw_parts(name_ptr, name_len);

        let fd = match connect_and_handshake(envp) {
            Some(fd) => fd,
            None => { dbg0(b"dbusprobe: --serve: handshake failed\n\0"); return 1; }
        };

        let mut bodybuf = [0u8; 64];
        let blen = encode_su(&mut bodybuf, name, 0u32);
        let mut msgbuf = [0u8; 512];
        let n = build_call(
            &mut msgbuf, 1,
            b"/org/freedesktop/DBus", b"org.freedesktop.DBus", b"RequestName",
            Some(b"org.freedesktop.DBus"), &bodybuf[..blen], Some(b"su"),
        );
        if write(fd, msgbuf.as_ptr(), n) != n as isize {
            dbg0(b"dbusprobe: --serve: RequestName write failed\n\0");
            return 1;
        }

        let mut rbuf = [0u8; 4096];
        match wait_for_reply(fd, 1, &mut rbuf) {
            Some(m) if m.mtype == MSG_METHOD_RETURN => {
                let p = align_to(m.body_start, 4);
                let code = read_u32_at(&rbuf, p);
                dbg_pu(b"dbusprobe: SERVING: %s reply=%u\n\0", name_ptr, code);
            }
            Some(m) => {
                match m.error_name {
                    Some((s, _)) => dbg_pp(b"dbusprobe: SERVING: %s reply=ERR:%s\n\0", name_ptr, rbuf.as_ptr().add(s)),
                    None => dbg_p(b"dbusprobe: SERVING: %s reply=ERR:unknown\n\0", name_ptr),
                }
            }
            None => dbg_p(b"dbusprobe: SERVING: %s reply=TIMEOUT\n\0", name_ptr),
        }

        serve_loop(fd); // never returns
    }

    dbg0(b"dbusprobe: starting\n\0");
    let fd = match connect_and_handshake(envp) {
        Some(fd) => fd,
        None => return 1,
    };
    dbg0(b"dbusprobe: SASL handshake OK\n\0");

    let mut failures = 0i32;
    let mut serial: u32 = 1;

    // Step 3: Hello.
    {
        let mut msgbuf = [0u8; 512];
        let n = build_call(&mut msgbuf, serial, b"/org/freedesktop/DBus",
            b"org.freedesktop.DBus", b"Hello", Some(b"org.freedesktop.DBus"), &[], None);
        let s = serial; serial = serial.wrapping_add(1);
        if write(fd, msgbuf.as_ptr(), n) != n as isize {
            dbg0(b"dbusprobe: Hello: write failed\n\0");
            failures += 1;
        } else {
            let mut rbuf = [0u8; 4096];
            match wait_for_reply(fd, s, &mut rbuf) {
                Some(m) if m.mtype == MSG_METHOD_RETURN => {
                    // Body is a single STRING (the unique name); we only
                    // need the wire's own NUL terminator to print it.
                    let p = align_to(m.body_start, 4) + 4;
                    dbg_p(b"dbusprobe: unique name = %s\n\0", rbuf.as_ptr().add(p));
                }
                Some(m) if m.mtype == MSG_ERROR => match m.error_name {
                    Some((s2, _)) => dbg_p(b"dbusprobe: Hello error: %s\n\0", rbuf.as_ptr().add(s2)),
                    None => dbg0(b"dbusprobe: Hello: error reply (no ERROR_NAME)\n\0"),
                },
                Some(m) => dbg1(b"dbusprobe: Hello: unexpected reply mtype=%d\n\0", m.mtype as i64),
                None => {
                    dbg0(b"dbusprobe: Hello: no reply (protocol failure)\n\0");
                    failures += 1;
                }
            }
        }
    }

    // Step 4: ListActivatableNames — the key evidence busd scanned its
    // servicedirs.
    {
        let mut msgbuf = [0u8; 512];
        let n = build_call(&mut msgbuf, serial, b"/org/freedesktop/DBus",
            b"org.freedesktop.DBus", b"ListActivatableNames", Some(b"org.freedesktop.DBus"), &[], None);
        let s = serial; serial = serial.wrapping_add(1);
        if write(fd, msgbuf.as_ptr(), n) != n as isize {
            dbg0(b"dbusprobe: ListActivatableNames: write failed\n\0");
            failures += 1;
        } else {
            let mut rbuf = [0u8; 4096];
            match wait_for_reply(fd, s, &mut rbuf) {
                Some(m) if m.mtype == MSG_METHOD_RETURN => {
                    let mut p = align_to(m.body_start, 4);
                    let abytes = read_u32_at(&rbuf, p) as usize;
                    p += 4;
                    let arr_end = p + abytes;
                    while p < arr_end {
                        p = align_to(p, 4);
                        let slen = read_u32_at(&rbuf, p) as usize;
                        p += 4;
                        dbg_p(b"ACTIVATABLE: %s\n\0", rbuf.as_ptr().add(p));
                        p += slen + 1;
                    }
                }
                Some(m) if m.mtype == MSG_ERROR => match m.error_name {
                    Some((s2, _)) => dbg_p(b"dbusprobe: ListActivatableNames error: %s\n\0", rbuf.as_ptr().add(s2)),
                    None => dbg0(b"dbusprobe: ListActivatableNames: error reply (no ERROR_NAME)\n\0"),
                },
                Some(m) => dbg1(b"dbusprobe: ListActivatableNames: unexpected reply mtype=%d\n\0", m.mtype as i64),
                None => {
                    dbg0(b"dbusprobe: ListActivatableNames: no reply (protocol failure)\n\0");
                    failures += 1;
                }
            }
        }
    }

    // Step 5: StartServiceByName on the well-known activatable name.
    {
        let mut bodybuf = [0u8; 64];
        let blen = encode_su(&mut bodybuf, b"org.leandros.ActivationProbe", 0u32);
        let mut msgbuf = [0u8; 512];
        let n = build_call(&mut msgbuf, serial, b"/org/freedesktop/DBus",
            b"org.freedesktop.DBus", b"StartServiceByName", Some(b"org.freedesktop.DBus"),
            &bodybuf[..blen], Some(b"su"));
        let s = serial; serial = serial.wrapping_add(1);
        let t0 = now_ms();
        if write(fd, msgbuf.as_ptr(), n) != n as isize {
            dbg0(b"dbusprobe: StartServiceByName: write failed\n\0");
            failures += 1;
        } else {
            let mut rbuf = [0u8; 4096];
            match wait_for_reply(fd, s, &mut rbuf) {
                Some(m) if m.mtype == MSG_METHOD_RETURN => {
                    let ms = now_ms() - t0;
                    let p = align_to(m.body_start, 4);
                    let code = read_u32_at(&rbuf, p);
                    dbg_u1(b"STARTSERVICE: result=%u %dms\n\0", code, ms);
                }
                Some(m) if m.mtype == MSG_ERROR => {
                    let ms = now_ms() - t0;
                    match m.error_name {
                        Some((s2, _)) => dbg_p1(b"STARTSERVICE: %s %dms\n\0", rbuf.as_ptr().add(s2), ms),
                        None => dbg1(b"STARTSERVICE: error(no ERROR_NAME) %dms\n\0", ms),
                    }
                }
                Some(m) => {
                    let ms = now_ms() - t0;
                    dbg2(b"STARTSERVICE: unexpected reply mtype=%d %dms\n\0", m.mtype as i64, ms);
                }
                None => {
                    let ms = now_ms() - t0;
                    dbg1(b"STARTSERVICE: TIMEOUT %dms\n\0", ms);
                    failures += 1;
                }
            }
        }
    }

    // Step 6: UNOWNED — regression guard. Must come back fast with
    // ServiceUnknown, not hang (service-unknown-reply.patch).
    {
        let mut msgbuf = [0u8; 512];
        let n = build_call(&mut msgbuf, serial, b"/",
            b"org.leandros.NoSuchService", b"Ping", Some(b"org.leandros.NoSuchService"), &[], None);
        let s = serial; serial = serial.wrapping_add(1);
        let t0 = now_ms();
        if write(fd, msgbuf.as_ptr(), n) != n as isize {
            dbg0(b"dbusprobe: UNOWNED: write failed\n\0");
            failures += 1;
        } else {
            let mut rbuf = [0u8; 4096];
            match wait_for_reply(fd, s, &mut rbuf) {
                Some(m) => {
                    let ms = now_ms() - t0;
                    match m.error_name {
                        Some((s2, _)) => dbg_p1(b"UNOWNED: %s %dms\n\0", rbuf.as_ptr().add(s2), ms),
                        None => dbg1(b"UNOWNED: unexpected reply (mtype, no ERROR_NAME) %dms\n\0", ms),
                    }
                }
                None => {
                    let ms = now_ms() - t0;
                    dbg1(b"UNOWNED: TIMEOUT %dms\n\0", ms);
                    failures += 1;
                }
            }
        }
    }

    // Step 7: IMPLICIT — a plain call to an unowned-but-activatable name.
    {
        let mut msgbuf = [0u8; 512];
        let n = build_call(&mut msgbuf, serial, b"/",
            b"org.leandros.ActivationProbe", b"Ping", Some(b"org.leandros.ActivationProbe"), &[], None);
        let s = serial; // last serial used by this probe run
        let t0 = now_ms();
        if write(fd, msgbuf.as_ptr(), n) != n as isize {
            dbg0(b"dbusprobe: IMPLICIT: write failed\n\0");
            failures += 1;
        } else {
            let mut rbuf = [0u8; 4096];
            match wait_for_reply(fd, s, &mut rbuf) {
                Some(m) if m.mtype == MSG_METHOD_RETURN => {
                    let ms = now_ms() - t0;
                    dbg1(b"IMPLICIT: success %dms\n\0", ms);
                }
                Some(m) => {
                    let ms = now_ms() - t0;
                    match m.error_name {
                        Some((s2, _)) => dbg_p1(b"IMPLICIT: %s %dms\n\0", rbuf.as_ptr().add(s2), ms),
                        None => dbg1(b"IMPLICIT: error(no ERROR_NAME) %dms\n\0", ms),
                    }
                }
                None => {
                    let ms = now_ms() - t0;
                    dbg1(b"IMPLICIT: TIMEOUT %dms\n\0", ms);
                    failures += 1;
                }
            }
        }
    }

    close(fd);
    dbg0(b"dbusprobe: done\n\0");
    if failures > 0 { 1 } else { 0 }
}
