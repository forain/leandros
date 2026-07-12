//! ping — ICMP echo over a raw socket (AF_INET/SOCK_RAW/IPPROTO_ICMP), the
//! only protocol the net server understands (see servers/net/src/lib.rs's
//! IcmpUnbound/IcmpBound socket states). No DNS resolver exists on this OS,
//! so the target must be a dotted-quad IPv4 address. Fixed 4-packet count,
//! ~1s interval, ~2s per-packet timeout — no -c/-i/-t flags in this first
//! pass.
//!
//! Initializes via relibc_start_v1 (same as pthreadtest/timertest/sigtest/
//! polltest/racetest) so TLS, errno, and the real socket()/sendto()/
//! recvfrom() Pal calls all work.
//!
//! The net server's ICMP path is non-blocking-only (no epoll/poll wiring for
//! it yet), so replies are collected via a short sleep-retry loop rather than
//! blocking recv — mirrors smoltcp's own examples/ping.rs.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;

type c_int = i32;
type c_long = i64;
type time_t = i64;
type pid_t = i32;
type size_t = usize;
type ssize_t = isize;

const AF_INET:      c_int = 2;
const SOCK_RAW:     c_int = 3;
const IPPROTO_ICMP: c_int = 1;

const CLOCK_MONOTONIC: c_int = 1;

const ICMP_ECHO_REQUEST: u8 = 8;
const ICMP_ECHO_REPLY:   u8 = 0;

const PING_COUNT:    u32 = 4;
const TIMEOUT_MS:    i64 = 2000;
const INTERVAL_MS:   i64 = 1000;
const PACKET_LEN:    usize = 40; // 8-byte ICMP header + 8-byte timestamp + 24 filler

#[repr(C)]
#[derive(Clone, Copy)]
pub struct timespec {
    tv_sec:  time_t,
    tv_nsec: c_long,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct sockaddr_in {
    sin_family: u16,
    sin_port:   u16,
    sin_addr:   [u8; 4],
    sin_zero:   [u8; 8],
}

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;

    pub fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    pub fn close(fd: i32) -> i32;
    pub fn exit(status: i32) -> !;

    pub fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    pub fn sendto(
        socket: c_int, message: *const c_void, length: size_t, flags: c_int,
        dest_addr: *const c_void, dest_len: u32,
    ) -> ssize_t;
    pub fn recvfrom(
        socket: c_int, buffer: *mut c_void, length: size_t, flags: c_int,
        address: *mut c_void, address_len: *mut u32,
    ) -> ssize_t;

    pub fn nanosleep(rqtp: *const timespec, rmtp: *mut timespec) -> c_int;
    pub fn clock_gettime(clk: c_int, tp: *mut timespec) -> c_int;
    pub fn getpid() -> pid_t;
}

// ── Assembly entry point (identical to polltest's/timertest's) ──────────────

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset ping_main",
    "   and rsp, -16",
    "   call relibc_start_v1",
    "   ud2"
);

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   mov x29, #0",
    "   mov x30, #0",
    "   mov x0, sp",
    "   adrp x1, ping_main",
    "   add x1, x1, :lo12:ping_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

// ── Helpers ───────────────────────────────────────────────────────────────

unsafe fn write_str(s: &[u8]) {
    write(1, s.as_ptr(), s.len());
}

unsafe fn write_uint(n: u64) {
    if n == 0 {
        write_str(b"0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = 20;
    let mut n = n;
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    write(1, buf[i..].as_ptr(), 20 - i);
}

unsafe fn write_dotted(o: &[u8; 4]) {
    for i in 0..4 {
        if i > 0 { write_str(b"."); }
        write_uint(o[i] as u64);
    }
}

unsafe fn now_ms() -> i64 {
    let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
    clock_gettime(CLOCK_MONOTONIC, &mut ts);
    ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000
}

unsafe fn sleep_ms(ms: i64) {
    let req = timespec { tv_sec: ms / 1000, tv_nsec: (ms % 1000) * 1_000_000 };
    nanosleep(&req, core::ptr::null_mut());
}

unsafe fn cstr_len(p: *const u8) -> usize {
    let mut n = 0;
    while *p.add(n) != 0 { n += 1; }
    n
}

fn parse_ipv4(s: &[u8]) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut idx = 0;
    let mut cur: u32 = 0;
    let mut have_digit = false;
    for &b in s {
        match b {
            b'0'..=b'9' => {
                cur = cur * 10 + (b - b'0') as u32;
                if cur > 255 { return None; }
                have_digit = true;
            }
            b'.' => {
                if !have_digit || idx >= 3 { return None; }
                octets[idx] = cur as u8;
                idx += 1;
                cur = 0;
                have_digit = false;
            }
            _ => return None,
        }
    }
    if !have_digit || idx != 3 { return None; }
    octets[3] = cur as u8;
    Some(octets)
}

// Standard RFC 1071 Internet checksum (ones'-complement sum of 16-bit words).
fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

fn build_packet(ident: u16, seq: u16, send_time_ms: i64, buf: &mut [u8; PACKET_LEN]) {
    buf[0] = ICMP_ECHO_REQUEST;
    buf[1] = 0; // code
    buf[2] = 0; buf[3] = 0; // checksum, filled below
    buf[4] = (ident >> 8) as u8; buf[5] = ident as u8;
    buf[6] = (seq >> 8) as u8;   buf[7] = seq as u8;
    buf[8..16].copy_from_slice(&send_time_ms.to_be_bytes());
    for b in &mut buf[16..PACKET_LEN] { *b = 0xAA; }
    let csum = checksum(buf);
    buf[2] = (csum >> 8) as u8;
    buf[3] = csum as u8;
}

// ── Entry point ───────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn ping_main(argc: isize, argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    if argc < 2 {
        write_str(b"usage: ping <ipv4-address>\n");
        return 1;
    }

    let arg_ptr = *argv.add(1);
    let arg = core::slice::from_raw_parts(arg_ptr, cstr_len(arg_ptr));
    let dest = match parse_ipv4(arg) {
        Some(o) => o,
        None => { write_str(b"ping: invalid IPv4 address\n"); return 1; }
    };

    let fd = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
    if fd < 0 {
        write_str(b"ping: socket() failed\n");
        return 1;
    }

    let ident = (getpid() as u16) & 0x7FFF;
    let dest_addr = sockaddr_in {
        sin_family: AF_INET as u16,
        sin_port: 0,
        sin_addr: dest,
        sin_zero: [0; 8],
    };

    write_str(b"PING ");
    write_dotted(&dest);
    write_str(b"\n");

    let mut sent = 0u32;
    let mut received = 0u32;

    for seq in 0..PING_COUNT {
        let mut pkt = [0u8; PACKET_LEN];
        let t0 = now_ms();
        build_packet(ident, seq as u16, t0, &mut pkt);

        sent += 1;
        let n = sendto(
            fd, pkt.as_ptr() as *const c_void, pkt.len(), 0,
            &dest_addr as *const sockaddr_in as *const c_void, 16,
        );

        let mut got_reply = false;
        if n < 0 {
            write_str(b"ping: sendto failed\n");
        } else {
            loop {
                if now_ms() - t0 > TIMEOUT_MS { break; }

                let mut rbuf = [0u8; 128];
                let mut from = sockaddr_in { sin_family: 0, sin_port: 0, sin_addr: [0; 4], sin_zero: [0; 8] };
                let mut fromlen: u32 = 16;
                let rn = recvfrom(
                    fd, rbuf.as_mut_ptr() as *mut c_void, rbuf.len(), 0,
                    &mut from as *mut sockaddr_in as *mut c_void, &mut fromlen,
                );

                if rn >= 8 {
                    let rtype = rbuf[0];
                    let rseq = u16::from_be_bytes([rbuf[6], rbuf[7]]);
                    if rtype == ICMP_ECHO_REPLY && rseq == seq as u16 {
                        let rtt = now_ms() - t0;
                        write_uint(rn as u64); write_str(b" bytes from ");
                        write_dotted(&dest);
                        write_str(b": icmp_seq="); write_uint(seq as u64);
                        write_str(b" time="); write_uint(rtt as u64); write_str(b"ms\n");
                        received += 1;
                        got_reply = true;
                        break;
                    }
                }
                sleep_ms(10);
            }
            if !got_reply {
                write_str(b"Request timeout for icmp_seq "); write_uint(seq as u64); write_str(b"\n");
            }
        }

        if seq + 1 < PING_COUNT {
            sleep_ms(INTERVAL_MS);
        }
    }

    close(fd);

    write_str(b"--- ping statistics ---\n");
    write_uint(sent as u64); write_str(b" packets transmitted, ");
    write_uint(received as u64); write_str(b" received\n");

    if received == 0 { 1 } else { 0 }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}
