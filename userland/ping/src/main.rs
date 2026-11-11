//! ping — ICMP echo over a raw socket (AF_INET/SOCK_RAW/IPPROTO_ICMP) as root,
//! or the unprivileged "ping socket" (SOCK_DGRAM/IPPROTO_ICMP) otherwise (see
//! servers/net/src/lib.rs's IcmpUnbound/IcmpBound socket states). The target
//! must be a dotted-quad IPv4 address.
//!
//!   ping [-c COUNT] [-W TIMEOUT_S] [-i INTERVAL_S] <ipv4>
//!
//! Defaults: 4 packets, 2 s per-packet timeout, 1 s interval. Exit status 0
//! when at least one reply arrived, 1 otherwise (Linux ping's convention).
//!
//! Initializes via relibc_start_v1 (same as pthreadtest/timertest/sigtest/
//! polltest/racetest) so TLS, errno, and the real socket()/sendto()/
//! recvfrom() Pal calls all work.
//!
//! Replies are waited for with poll() bounded by the per-packet timeout, and
//! read with MSG_DONTWAIT. The socket is a normal *blocking* socket (the
//! kernel's net_blocking_op parks a blocking recvfrom until data arrives), so a
//! plain recvfrom() would never return when no reply comes: that is how
//! `ping <unreachable>` used to print its header and then hang forever
//! instead of reporting "Request timeout".

#![no_std]
#![no_main]
#![feature(c_variadic)]
#![allow(non_camel_case_types)]

#[path = "../../muslrt/rt.rs"]
mod rt;

use core::ffi::c_void;

type c_int = i32;
type c_long = i64;
type time_t = i64;
type pid_t = i32;
type size_t = usize;
type ssize_t = isize;

const AF_INET:      c_int = 2;
const AF_INET6:     c_int = 10;
const IPPROTO_ICMPV6: c_int = 58;
const ICMP6_ECHO_REQUEST: u8 = 128;
const ICMP6_ECHO_REPLY:   u8 = 129;
const SOCK_RAW:     c_int = 3;
const SOCK_DGRAM:   c_int = 2;
const IPPROTO_ICMP: c_int = 1;

const CLOCK_MONOTONIC: c_int = 1;

const ICMP_ECHO_REQUEST: u8 = 8;
const ICMP_ECHO_REPLY:   u8 = 0;

const PING_COUNT:    u32 = 4;
const TIMEOUT_MS:    i64 = 2000;
const INTERVAL_MS:   i64 = 1000;
const MSG_DONTWAIT:  c_int = 0x40;
const POLLIN:        i16 = 0x1;
const PACKET_LEN:    usize = 40; // 8-byte ICMP header + 8-byte timestamp + 24 filler

#[repr(C)]
#[derive(Clone, Copy)]
pub struct timespec {
    tv_sec:  time_t,
    tv_nsec: c_long,
}

#[repr(C)]
pub struct pollfd {
    fd:      c_int,
    events:  i16,
    revents: i16,
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

    pub fn poll(fds: *mut pollfd, nfds: u64, timeout: c_int) -> c_int;
    pub fn nanosleep(rqtp: *const timespec, rmtp: *mut timespec) -> c_int;
    pub fn clock_gettime(clk: c_int, tp: *mut timespec) -> c_int;
    pub fn getpid() -> pid_t;
}

// ── Assembly entry point (identical to polltest's/timertest's) ──────────────



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

unsafe fn arg_at<'a>(argv: *mut *mut u8, i: isize) -> &'a [u8] {
    let p = *argv.offset(i);
    core::slice::from_raw_parts(p, cstr_len(p))
}

fn parse_uint(s: &[u8]) -> Option<u64> {
    if s.is_empty() || s.len() > 9 { return None; }
    let mut v = 0u64;
    for &b in s {
        if !b.is_ascii_digit() { return None; }
        v = v * 10 + (b - b'0') as u64;
    }
    Some(v)
}

unsafe fn usage() -> i32 {
    write_str(b"usage: ping [-4|-6] [-c count] [-W timeout_s] [-i interval_s] <ip-address>\n");
    2
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

// An IPv6 literal: hex groups with at most one `::` (no embedded IPv4, no scope).
fn parse_ipv6(s: &[u8]) -> Option<[u8; 16]> {
    let mut head = [0u16; 8];
    let mut tail = [0u16; 8];
    let (mut nh, mut nt) = (0usize, 0usize);
    let mut in_tail = false;
    let mut cur: u32 = 0;
    let mut digits = 0;
    let mut i = 0;
    if s.is_empty() { return None; }
    while i < s.len() {
        let b = s[i];
        match b {
            b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F' => {
                let d = (b as char).to_digit(16)?;
                cur = cur * 16 + d;
                digits += 1;
                if digits > 4 { return None; }
            }
            b':' => {
                let dbl = s.get(i + 1) == Some(&b':');
                if digits > 0 {
                    if in_tail { if nt >= 8 { return None; } tail[nt] = cur as u16; nt += 1; }
                    else { if nh >= 8 { return None; } head[nh] = cur as u16; nh += 1; }
                    cur = 0; digits = 0;
                } else if !(dbl && i == 0) && !(i > 0 && s[i - 1] == b':') {
                    return None;
                }
                if dbl {
                    if in_tail { return None; }
                    in_tail = true;
                    i += 1;
                }
            }
            _ => return None,
        }
        i += 1;
    }
    if digits > 0 {
        if in_tail { if nt >= 8 { return None; } tail[nt] = cur as u16; nt += 1; }
        else { if nh >= 8 { return None; } head[nh] = cur as u16; nh += 1; }
    } else if s[s.len() - 1] == b':' && !(s.len() >= 2 && s[s.len() - 2] == b':') {
        return None;
    }
    if in_tail { if nh + nt > 7 { return None; } } else if nh != 8 { return None; }
    let mut g = [0u16; 8];
    g[..nh].copy_from_slice(&head[..nh]);
    g[8 - nt..].copy_from_slice(&tail[..nt]);
    let mut o = [0u8; 16];
    for k in 0..8 { o[k * 2] = (g[k] >> 8) as u8; o[k * 2 + 1] = g[k] as u8; }
    Some(o)
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

#[repr(C)]
struct sockaddr_in6 {
    sin6_family:   u16,
    sin6_port:     u16,
    sin6_flowinfo: u32,
    sin6_addr:     [u8; 16],
    sin6_scope_id: u32,
}

// ICMPv6 echo: the kernel fills the checksum (a pseudo-header sum the sender
// cannot know before routing), and a raw ICMPv6 socket carries no IPv6 header.
unsafe fn ping6(dest: [u8; 16], arg: &[u8], count: u32, timeout_ms: i64, interval_ms: i64) -> i32 {
    let mut fd = socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6);
    if fd < 0 { fd = socket(AF_INET6, SOCK_DGRAM, IPPROTO_ICMPV6); }
    if fd < 0 {
        write_str(b"ping: socket() failed\n");
        return 1;
    }
    let ident = (getpid() as u16) & 0x7FFF;
    let dest_addr = sockaddr_in6 { sin6_family: AF_INET6 as u16, sin6_port: 0, sin6_flowinfo: 0, sin6_addr: dest, sin6_scope_id: 0 };

    write_str(b"PING "); write_str(arg); write_str(b"\n");
    let mut sent = 0u32;
    let mut received = 0u32;
    for seq in 0..count {
        let mut pkt = [0u8; PACKET_LEN];
        let t0 = now_ms();
        build_packet(ident, seq as u16, t0, &mut pkt);
        pkt[0] = ICMP6_ECHO_REQUEST;
        pkt[2] = 0; pkt[3] = 0;
        sent += 1;
        let n = sendto(fd, pkt.as_ptr() as *const c_void, pkt.len(), 0,
                       &dest_addr as *const sockaddr_in6 as *const c_void, 28);
        let mut got_reply = false;
        if n < 0 {
            write_str(b"ping: sendto failed\n");
        } else {
            loop {
                let left = timeout_ms - (now_ms() - t0);
                if left <= 0 { break; }
                let mut pfd = pollfd { fd, events: POLLIN, revents: 0 };
                if poll(&mut pfd, 1, left as c_int) <= 0 { continue; }
                let mut rbuf = [0u8; 128];
                let mut from = sockaddr_in6 { sin6_family: 0, sin6_port: 0, sin6_flowinfo: 0, sin6_addr: [0; 16], sin6_scope_id: 0 };
                let mut fromlen: u32 = 28;
                let rn = recvfrom(fd, rbuf.as_mut_ptr() as *mut c_void, rbuf.len(), MSG_DONTWAIT,
                                  &mut from as *mut sockaddr_in6 as *mut c_void, &mut fromlen);
                if rn >= 8 {
                    let rseq = u16::from_be_bytes([rbuf[6], rbuf[7]]);
                    if rbuf[0] == ICMP6_ECHO_REPLY && rseq == seq as u16 {
                        let rtt = now_ms() - t0;
                        write_uint(rn as u64); write_str(b" bytes from "); write_str(arg);
                        write_str(b": icmp_seq="); write_uint(seq as u64);
                        write_str(b" time="); write_uint(rtt as u64); write_str(b"ms\n");
                        received += 1;
                        got_reply = true;
                        break;
                    }
                }
            }
            if !got_reply {
                write_str(b"Request timeout for icmp_seq "); write_uint(seq as u64); write_str(b"\n");
            }
        }
        if seq + 1 < count {
            let rest = interval_ms - (now_ms() - t0);
            if rest > 0 { sleep_ms(rest); }
        }
    }
    close(fd);
    write_str(b"--- ping statistics ---\n");
    write_uint(sent as u64); write_str(b" packets transmitted, ");
    write_uint(received as u64); write_str(b" received, ");
    write_uint(((sent - received) as u64 * 100) / sent.max(1) as u64);
    write_str(b"% packet loss\n");
    if received == 0 { 1 } else { 0 }
}

// ── Entry point ───────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn main(argc_c: i32, argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let argc = argc_c as isize;
    let mut count = PING_COUNT;
    let mut timeout_ms = TIMEOUT_MS;
    let mut interval_ms = INTERVAL_MS;
    let mut target: Option<&[u8]> = None;
    let mut force6 = false;
    let mut i = 1;
    while i < argc {
        let a = arg_at(argv, i);
        let needs_val = a == b"-c" || a == b"-W" || a == b"-i";
        if needs_val {
            if i + 1 >= argc { return usage(); }
            let v = match parse_uint(arg_at(argv, i + 1)) { Some(v) => v, None => return usage() };
            match a {
                b"-c" => { if v == 0 { return usage(); } count = v as u32; }
                b"-W" => timeout_ms = (v as i64) * 1000,
                _     => interval_ms = (v as i64) * 1000,
            }
            i += 2;
        } else if a == b"-6" {
            force6 = true;
            i += 1;
        } else if a == b"-4" {
            force6 = false;
            i += 1;
        } else if a.first() == Some(&b'-') {
            return usage();
        } else {
            target = Some(a);
            i += 1;
        }
    }
    let arg = match target { Some(t) => t, None => return usage() };
    let v6 = force6 || arg.contains(&b':');
    if v6 {
        let dest6 = match parse_ipv6(arg) {
            Some(o) => o,
            None => { write_str(b"ping: invalid IPv6 address\n"); return 1; }
        };
        return ping6(dest6, arg, count, timeout_ms, interval_ms);
    }
    let dest = match parse_ipv4(arg) {
        Some(o) => o,
        None => { write_str(b"ping: invalid IPv4 address\n"); return 1; }
    };

    // A raw socket needs root (CAP_NET_RAW); everyone else gets the Linux
    // "ping socket", SOCK_DGRAM/IPPROTO_ICMP, which carries the same echo
    // request/reply bytes here.
    let mut fd = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
    // A raw socket's datagrams start with the IPv4 header; a ping socket's
    // with the ICMP header.
    let raw = fd >= 0;
    if fd < 0 {
        fd = socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP);
    }
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

    for seq in 0..count {
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
                let left = timeout_ms - (now_ms() - t0);
                if left <= 0 { break; }
                let mut pfd = pollfd { fd, events: POLLIN, revents: 0 };
                if poll(&mut pfd, 1, left as c_int) <= 0 { continue; } // timeout / EINTR: re-check the clock

                let mut rbuf = [0u8; 128];
                let mut from = sockaddr_in { sin_family: 0, sin_port: 0, sin_addr: [0; 4], sin_zero: [0; 8] };
                let mut fromlen: u32 = 16;
                let rn = recvfrom(
                    fd, rbuf.as_mut_ptr() as *mut c_void, rbuf.len(), MSG_DONTWAIT,
                    &mut from as *mut sockaddr_in as *mut c_void, &mut fromlen,
                );

                let hl = if raw && rn >= 20 && rbuf[0] >> 4 == 4 { ((rbuf[0] & 0xf) as isize) * 4 } else { 0 };
                let rn = rn - hl;
                let ib = &rbuf[hl.max(0) as usize..];
                if rn >= 8 {
                    let rtype = ib[0];
                    let rseq = u16::from_be_bytes([ib[6], ib[7]]);
                    if rtype == ICMP_ECHO_REPLY && rseq == seq as u16 {
                        let rtt = now_ms() - t0;
                        write_uint(rn as u64); write_str(b" bytes from ");
                        write_dotted(&from.sin_addr);
                        write_str(b": icmp_seq="); write_uint(seq as u64);
                        write_str(b" time="); write_uint(rtt as u64); write_str(b"ms\n");
                        received += 1;
                        got_reply = true;
                        break;
                    }
                }
            }
            if !got_reply {
                write_str(b"Request timeout for icmp_seq "); write_uint(seq as u64); write_str(b"\n");
            }
        }

        if seq + 1 < count {
            // Linux paces sends from the previous send, not from the reply.
            let rest = interval_ms - (now_ms() - t0);
            if rest > 0 { sleep_ms(rest); }
        }
    }

    close(fd);

    write_str(b"--- ping statistics ---\n");
    write_uint(sent as u64); write_str(b" packets transmitted, ");
    write_uint(received as u64); write_str(b" received, ");
    write_uint(((sent - received) as u64 * 100) / sent.max(1) as u64);
    write_str(b"% packet loss\n");

    if received == 0 { 1 } else { 0 }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}
