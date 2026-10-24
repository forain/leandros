//! `scmtest bench [iters]` — AF_UNIX IPC micro-benchmark (lane ipcfast).
//!
//! Firefox's IPC (laptop YouTube playback) spends 31–36 us of kernel time
//! per unix sendmsg/recvmsg; Linux does a small one in 2–5 us. This times a
//! parent↔child ping-pong over a socketpair for SOCK_STREAM and
//! SOCK_SEQPACKET, payloads 64 B / 4 KiB / 64 KiB, with 0 / 1 / 4 SCM_RIGHTS
//! fds on the parent's send (the child closes what it receives and echoes the
//! payload back without fds), and an eventfd / memfd create+dup+close loop.
//!
//! Output lines start with "IPCBENCH ". `rt_us` is the wall time of one
//! round trip; `snd_ns`/`rcv_ns` are the parent's own on-CPU kernel time per
//! sendmsg/recvmsg from /proc/sysstat (exact syscall accounting).

use super::*;

#[cfg(target_arch = "aarch64")] const NR_SENDMSG: u64 = 211;
#[cfg(target_arch = "x86_64")]  const NR_SENDMSG: u64 = 46;
#[cfg(target_arch = "aarch64")] const NR_RECVMSG: u64 = 212;
#[cfg(target_arch = "x86_64")]  const NR_RECVMSG: u64 = 47;
#[cfg(target_arch = "aarch64")] const NR_EVENTFD2: usize = 19;
#[cfg(target_arch = "x86_64")]  const NR_EVENTFD2: usize = 290;
#[cfg(target_arch = "aarch64")] const NR_MEMFD: u64 = 279;
#[cfg(target_arch = "x86_64")]  const NR_MEMFD: u64 = 319;
const SOCK_SEQPACKET: i32 = 5;
const MAXP: usize = 65536;

static mut SBUF: [u8; MAXP] = [0; MAXP];
static mut RBUF: [u8; MAXP] = [0; MAXP];
static mut STAT: [u8; 1 << 20] = [0; 1 << 20];

fn now_ns() -> u64 {
    let mut t = timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { clock_gettime(CLOCK_MONOTONIC, &mut t); }
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}

/// (calls, ns) of syscall `nr` for thread group `tgid`, from /proc/sysstat.
unsafe fn sysstat(tgid: u64, nrs: &[u64], out: &mut [(u64, u64, u64)]) {
    for o in out.iter_mut() { *o = (0, 0, 0); }
    let fd = open(b"/proc/sysstat\0".as_ptr(), 0, 0);
    if fd < 0 { return; }
    let mut len = 0usize;
    loop {
        let n = read(fd, STAT.as_mut_ptr().add(len), STAT.len() - len);
        if n <= 0 { break; }
        len += n as usize;
        if len == STAT.len() { break; }
    }
    close(fd);
    let s = &STAT[..len];
    for line in s.split(|&b| b == b'\n') {
        if line.len() < 2 || line[0] != b's' || line[1] != b' ' { continue; }
        let line = &line[2..];
        let mut f = [0u64; 6];
        let mut k = 0;
        let mut ok = true;
        for w in line.split(|&b| b == b' ').filter(|w| !w.is_empty()).take(6) {
            let mut v = 0u64;
            for &c in w { if !c.is_ascii_digit() { ok = false; break; } v = v * 10 + (c - b'0') as u64; }
            if !ok { break; }
            f[k] = v; k += 1;
        }
        if !ok || k < 6 || f[0] != tgid { continue; }
        for (i, &nr) in nrs.iter().enumerate() {
            if f[1] == nr { out[i] = (f[2], f[3], f[5]); }
        }
    }
}

unsafe fn send_all(fd: i32, len: usize, fds: &[i32]) -> bool {
    let mut cm = CmsgBuf { b: [0; 32] };
    let mut off = 0usize;
    let mut first = true;
    while off < len {
        let mut iov = iovec { iov_base: SBUF.as_mut_ptr().add(off), iov_len: len - off };
        let mut m: msghdr = core::mem::zeroed();
        m.msg_iov = &mut iov; m.msg_iovlen = 1;
        if first && !fds.is_empty() {
            let h = cm.b.as_mut_ptr() as *mut cmsghdr;
            (*h).cmsg_len = cmsg_len(4 * fds.len());
            (*h).cmsg_level = SOL_SOCKET; (*h).cmsg_type = SCM_RIGHTS;
            core::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8,
                cm.b.as_mut_ptr().add(cmsg_data_off()), 4 * fds.len());
            m.msg_control = cm.b.as_mut_ptr(); m.msg_controllen = cmsg_space(4 * fds.len());
        }
        let r = raw_sendmsg(fd, &m, 0);
        if r <= 0 { return false; }
        off += r as usize; first = false;
    }
    true
}

/// Receive `len` bytes (one record for SEQPACKET); close any fds received.
unsafe fn recv_all(fd: i32, len: usize) -> bool {
    let mut cm = CmsgBuf { b: [0; 32] };
    let mut off = 0usize;
    while off < len {
        let mut iov = iovec { iov_base: RBUF.as_mut_ptr().add(off), iov_len: len - off };
        let mut m: msghdr = core::mem::zeroed();
        m.msg_iov = &mut iov; m.msg_iovlen = 1;
        m.msg_control = cm.b.as_mut_ptr(); m.msg_controllen = cm.b.len();
        let r = raw_recvmsg(fd, &mut m, 0);
        if r <= 0 { return false; }
        if m.msg_controllen >= cmsg_len(4) {
            let h = cm.b.as_ptr() as *const cmsghdr;
            let n = ((*h).cmsg_len - cmsg_data_off()) / 4;
            for i in 0..n {
                let mut v = 0i32;
                core::ptr::copy_nonoverlapping(cm.b.as_ptr().add(cmsg_data_off() + 4 * i),
                    &mut v as *mut i32 as *mut u8, 4);
                close(v);
            }
        }
        off += r as usize;
    }
    true
}

unsafe fn pingpong(kind: i32, size: usize, nfds: usize, iters: usize, mfd: i32, tgid: u64) -> u64 {
    let mut sv = [0i32; 2];
    if raw_socketpair(AF_UNIX, kind, 0, sv.as_mut_ptr()) != 0 {
        dbg0(b"IPCBENCH socketpair failed\n\0"); return 0;
    }
    let pid = fork();
    if pid == 0 {
        close(sv[0]);
        for _ in 0..iters + 10 {
            if !recv_all(sv[1], size) { exit(1); }
            if !send_all(sv[1], size, &[]) { exit(2); }
        }
        exit(0);
    }
    close(sv[1]);
    let fds = [mfd; 4];
    for _ in 0..10 { send_all(sv[0], size, &fds[..nfds]); recv_all(sv[0], size); }
    let nrs = [NR_SENDMSG, NR_RECVMSG];
    let mut a = [(0u64, 0u64, 0u64); 2];
    let mut b = [(0u64, 0u64, 0u64); 2];
    let mut ca = [(0u64, 0u64, 0u64); 2];
    let mut cb = [(0u64, 0u64, 0u64); 2];
    sysstat(pid as u64, &nrs, &mut ca);
    sysstat(tgid, &nrs, &mut a);
    let t0 = now_ns();
    let mut ok = true;
    for _ in 0..iters {
        if !send_all(sv[0], size, &fds[..nfds]) || !recv_all(sv[0], size) { ok = false; break; }
    }
    let dt = now_ns() - t0;
    sysstat(tgid, &nrs, &mut b);
    sysstat(pid as u64, &nrs, &mut cb);
    let mut st = 0i32;
    wait4(pid, &mut st, 0, core::ptr::null_mut());
    close(sv[0]);
    let per = |i: usize| -> u64 {
        let c = b[i].0 - a[i].0; if c == 0 { 0 } else { (b[i].1 - a[i].1) / c }
    };
    let kname: &[u8] = if kind == SOCK_STREAM { b"stream\0" } else { b"seqpkt\0" };
    printf(b"IPCBENCH %s size=%lu fds=%lu \0".as_ptr(), kname.as_ptr() as u64, size as u64, nfds as u64, 0);
    printf(b"rt_us=%lu.%02lu snd_ns=%lu rcv_ns=%lu\0".as_ptr(),
        dt / iters as u64 / 1000, (dt / iters as u64 % 1000) / 10, per(0), per(1));
    let cper = |i: usize| -> u64 {
        let c = cb[i].0 - ca[i].0; if c == 0 { 0 } else { (cb[i].1 - ca[i].1) / c }
    };
    let rwall = { let c = b[1].0 - a[1].0; if c == 0 { 0 } else { (b[1].2 - a[1].2) / c } };
    printf(b" child_snd_ns=%lu child_rcv_ns=%lu rcv_wall_ns=%lu\0".as_ptr(), cper(0), cper(1), rwall, 0);
    printf(b" snd_calls=%lu rcv_calls=%lu ok=%lu\n\0".as_ptr(),
        b[0].0 - a[0].0, b[1].0 - a[1].0, (ok && st == 0) as u64, 0);
    if ok && st == 0 { per(0) + per(1) } else { 0 }
}

unsafe fn fdloop(iters: usize, tgid: u64) {
    let nrs = [nr::CLOSE as u64, nr::DUP as u64, NR_EVENTFD2 as u64, NR_MEMFD];
    let mut a = [(0u64, 0u64, 0u64); 4];
    let mut b = [(0u64, 0u64, 0u64); 4];
    for memfd in [false, true] {
        sysstat(tgid, &nrs, &mut a);
        let t0 = now_ns();
        for _ in 0..iters {
            let fd = if memfd { raw_memfd_create(b"ipcbench\0".as_ptr(), 0) }
                     else { syscall2(NR_EVENTFD2, 0, 0) as i32 };
            let d = syscall1(nr::DUP, fd as usize) as i32;
            close(d); close(fd);
        }
        let dt = now_ns() - t0;
        sysstat(tgid, &nrs, &mut b);
        let per = |i: usize| -> u64 {
            let c = b[i].0 - a[i].0; if c == 0 { 0 } else { (b[i].1 - a[i].1) / c }
        };
        let name: &[u8] = if memfd { b"memfd\0" } else { b"eventfd\0" };
        printf(b"IPCBENCH fdloop %s op_us=%lu.%02lu create_ns=%lu \0".as_ptr(), name.as_ptr() as u64,
            dt / iters as u64 / 1000, (dt / iters as u64 % 1000) / 10, per(if memfd { 3 } else { 2 }));
        printf(b"dup_ns=%lu close_ns=%lu\n\0".as_ptr(), per(1), per(0), 0, 0);
    }
}

pub unsafe fn run(iters: usize, hogs: usize) -> i32 {
    let tgid = syscall0(nr::GETPID) as u64;
    for i in 0..MAXP { SBUF[i] = i as u8; }
    let mfd = raw_memfd_create(b"ipcbench-pass\0".as_ptr(), 0);
    for kind in [SOCK_STREAM, SOCK_SEQPACKET] {
        for size in [64usize, 4096, 65536] {
            for nfds in [0usize, 1, 4] {
                let n = if size == 65536 { iters / 4 } else { iters }.max(1);
                let _ = pingpong(kind, size, nfds, n, mfd, tgid);
            }
        }
    }
    fdloop(iters, tgid);
    // Firefox holds hundreds of descriptors: repeat the small cases with 500
    // more open (an O(open fds) scan on the IPC path shows up here).
    let mut extra = 0;
    for _ in 0..500 { if syscall1(nr::DUP, mfd as usize) >= 0 { extra += 1; } }
    printf(b"IPCBENCH --- with %lu extra open fds\n\0".as_ptr(), extra, 0, 0, 0);
    for nfds in [0usize, 1, 4] { let _ = pingpong(SOCK_STREAM, 64, nfds, iters, mfd, tgid); }
    fdloop(iters, tgid);
    // The laptop desktop has ~100 processes holding sockets (Firefox content
    // processes, COSMIC, PipeWire, dbus): a per-call scan over every process's
    // socket table shows up here and not on an idle test VM.
    let mut kids = [0i32; 256];
    let hogs = hogs.min(kids.len());
    for k in 0..hogs {
        let pid = fork();
        if pid == 0 {
            let mut sv = [0i32; 2];
            raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr());
            let mut b = [0u8; 1];
            raw_recv(sv[0], b.as_mut_ptr(), 1, 0); // blocks until killed
            exit(0);
        }
        kids[k] = pid;
    }
    if hogs > 0 {
        sleep_ms(300);
        printf(b"IPCBENCH --- with %lu socket-holding processes\n\0".as_ptr(), hogs as u64, 0, 0, 0);
        for nfds in [0usize, 1, 4] { let _ = pingpong(SOCK_STREAM, 64, nfds, iters, mfd, tgid); }
        let _ = pingpong(SOCK_STREAM, 4096, 0, iters, mfd, tgid);
        let _ = pingpong(SOCK_SEQPACKET, 64, 0, iters, mfd, tgid);
        for k in 0..hogs {
            syscall2(nr::KILL, kids[k] as usize, 9);
            let mut st = 0i32;
            wait4(kids[k], &mut st, 0, core::ptr::null_mut());
        }
    }
    // Independent pairs at once: a lock shared by every unix connection
    // (held across the user copy) shows as on-CPU time rising with the
    // number of pairs although no two pairs share a socket.
    for par in [1usize, 2] {
        for (kind, size) in [(SOCK_STREAM, 64usize), (SOCK_STREAM, 65536), (SOCK_SEQPACKET, 4096)] {
            printf(b"IPCBENCH --- %lu concurrent pairs\n\0".as_ptr(), par as u64, 0, 0, 0);
            let mut w = [0i32; 4];
            for k in 0..par {
                let pid = fork();
                if pid == 0 {
                    let me = syscall0(nr::GETPID) as u64;
                    let n = if size == 65536 { iters / 2 } else { iters * 2 };
                    let _ = pingpong(kind, size, 0, n, mfd, me);
                    exit(0);
                }
                w[k] = pid;
            }
            for k in 0..par { let mut st = 0i32; wait4(w[k], &mut st, 0, core::ptr::null_mut()); }
        }
    }
    close(mfd);
    0
}

/// `(acquisitions, contended, spin_us)` of the named lock in /proc/lockstat.
unsafe fn lockstat(name: &[u8]) -> (u64, u64, u64) {
    let fd = open(b"/proc/lockstat\0".as_ptr(), 0, 0);
    if fd < 0 { return (0, 0, 0); }
    let n = read(fd, STAT.as_mut_ptr(), 8192);
    close(fd);
    if n <= 0 { return (0, 0, 0); }
    for line in STAT[..n as usize].split(|&b| b == b'\n') {
        let mut w = line.split(|&b| b == b' ');
        if w.next() != Some(name) { continue; }
        let mut f = [0u64; 3];
        for (i, t) in w.take(3).enumerate() {
            for &c in t { if c.is_ascii_digit() { f[i] = f[i] * 10 + (c - b'0') as u64; } }
        }
        return (f[0], f[1], f[2]);
    }
    (0, 0, 0)
}

/// Guard (lane ipcfast): `par` independent socketpair ping-pong pairs at once.
/// Returns the mean on-CPU sendmsg+recvmsg ns per round trip over the pairs.
/// With no lock shared between connections the cost must not grow with
/// `par` (each pair runs on its own vCPUs).
unsafe fn concurrent(kind: i32, size: usize, par: usize, iters: usize, mfd: i32) -> u64 {
    let mut rep = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, rep.as_mut_ptr()) != 0 { return 0; }
    let names: [&[u8]; 2] = [b"UNIX_CONNS", b"SOCK_TABLES"];
    let l0 = [lockstat(names[0]), lockstat(names[1])];
    let mut w = [0i32; 8];
    for k in 0..par {
        let pid = fork();
        if pid == 0 {
            close(rep[0]);
            let me = syscall0(nr::GETPID) as u64;
            let v = pingpong(kind, size, 0, iters, mfd, me);
            write(rep[1], &v as *const u64 as *const u8, 8);
            exit(0);
        }
        w[k] = pid;
    }
    let mut sum = 0u64;
    let mut got = 0u64;
    for _ in 0..par {
        let mut v = 0u64;
        if read(rep[0], &mut v as *mut u64 as *mut u8, 8) == 8 && v != 0 { sum += v; got += 1; }
    }
    for k in 0..par { let mut st = 0i32; wait4(w[k], &mut st, 0, core::ptr::null_mut()); }
    close(rep[0]); close(rep[1]);
    let mut spin_us = 0u64;
    for (i, nm) in names.iter().enumerate() {
        let l1 = lockstat(nm);
        printf(b"IPCBENCH   lock %s acq=%lu contended=%lu spin_us=%lu\n\0".as_ptr(),
            if i == 0 { b"UNIX_CONNS\0".as_ptr() } else { b"SOCK_TABLES\0".as_ptr() } as u64,
            l1.0 - l0[i].0, l1.1 - l0[i].1, l1.2 - l0[i].2);
        spin_us += l1.2 - l0[i].2;
    }
    // Lock spin per round trip and pair, for the pairbench verdict.
    LAST_SPIN_NS = spin_us * 1000 / ((iters * par) as u64).max(1);
    if got as usize != par { 0 } else { sum / got }
}

static mut LAST_SPIN_NS: u64 = 0;

/// `scmtest pairbench [iters]`: the 1-pair vs 2-pair guard. Fails (exit 1)
/// when, with two independent pairs running, UNIX_CONNS + SOCK_TABLES spin
/// is more than 10% of a round trip, or two pairs cost more than 2x one.
/// The spin share is the guard proper: the 2-pair/1-pair time ratio of the
/// fixed code still swings 1.24-1.69 run to run under HVF (4 vCPUs, host
/// load from the other QEMU slot), overlapping the unfixed code's 1.75, so
/// it is printed and only bounds a collapse. Unfixed code: spin share
/// 16-23% (SOCK_TABLES 30 ms at 64 B, UNIX_CONNS 32 ms at 64 KiB per run);
/// fixed: 0.7-5.4% (lane ipcfast, artifacts/notes/lane-ipcfast-2026-10-24.md).
pub unsafe fn pairs(iters: usize) -> i32 {
    for i in 0..MAXP { SBUF[i] = i as u8; }
    let mfd = raw_memfd_create(b"ipcbench-pass\0".as_ptr(), 0);
    let mut fails = 0;
    for (kind, size) in [(SOCK_STREAM, 64usize), (SOCK_STREAM, 65536), (SOCK_SEQPACKET, 4096)] {
        let n = if size == 65536 { iters / 4 } else { iters }.max(1);
        let _ = concurrent(kind, size, 1, n / 4 + 1, mfd); // warm-up
        // Best of three, so one host hiccup does not decide the verdict.
        let mut c = [u64::MAX; 2];
        let mut spin = u64::MAX;
        for _ in 0..3 {
            for p in 0..2 {
                printf(b"IPCBENCH --- %lu concurrent pairs\n\0".as_ptr(), (p + 1) as u64, 0, 0, 0);
                let v = concurrent(kind, size, p + 1, n, mfd);
                if v != 0 && v < c[p] { c[p] = v; }
                if p == 1 && v != 0 { spin = spin.min(LAST_SPIN_NS * 1000 / v); }
            }
        }
        let ratio = if c[0] == 0 || c[0] == u64::MAX || c[1] == u64::MAX { 999 } else { c[1] * 100 / c[0] };
        // spin: per-mille of a 2-pair round trip spent spinning on the two locks.
        let ok = ratio <= 200 && spin <= 100;
        printf(b"IPCGUARD spin_permille=%lu (max 100)\n\0".as_ptr(), spin, 0, 0, 0);
        if !ok { fails += 1; }
        let kname: &[u8] = if kind == SOCK_STREAM { b"stream\0" } else { b"seqpkt\0" };
        printf(b"IPCGUARD %s size=%lu \0".as_ptr(), kname.as_ptr() as u64, size as u64, 0, 0);
        printf(b"one_pair_ns=%lu two_pair_ns=%lu ratio=%lu.%02lu\0".as_ptr(), c[0], c[1], ratio / 100, ratio % 100);
        printf(b" %s\n\0".as_ptr(), if ok { b"PASS\0".as_ptr() } else { b"FAIL\0".as_ptr() } as u64, 0, 0, 0);
    }
    close(mfd);
    if fails == 0 { 0 } else { 1 }
}

const MSG_NOSIGNAL: i32 = 0x4000;
static mut RNG: u64 = 0;
fn rnd() -> u64 {
    unsafe { RNG ^= RNG << 13; RNG ^= RNG >> 7; RNG ^= RNG << 17; RNG }
}

/// One stress message: `len` patterned bytes with `nfds` copies of `mfd`.
/// `Ok(true)` sent, `Ok(false)` the peer is gone (EPIPE/ECONNRESET).
unsafe fn stress_send(fd: i32, i: usize, len: usize, nfds: usize, mfd: i32) -> Result<bool, isize> {
    for j in 0..len { SBUF[j] = (j + i * 13) as u8; }
    let fds = [mfd; 2];
    let mut cm = CmsgBuf { b: [0; 32] };
    let mut off = 0usize;
    while off < len {
        let mut iov = iovec { iov_base: SBUF.as_mut_ptr().add(off), iov_len: len - off };
        let mut m: msghdr = core::mem::zeroed();
        m.msg_iov = &mut iov; m.msg_iovlen = 1;
        if off == 0 && nfds > 0 {
            let h = cm.b.as_mut_ptr() as *mut cmsghdr;
            (*h).cmsg_len = cmsg_len(4 * nfds);
            (*h).cmsg_level = SOL_SOCKET; (*h).cmsg_type = SCM_RIGHTS;
            core::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, cm.b.as_mut_ptr().add(cmsg_data_off()), 4 * nfds);
            m.msg_control = cm.b.as_mut_ptr(); m.msg_controllen = cmsg_space(4 * nfds);
        }
        let r = syscall3(SYS_SENDMSG, fd as usize, &m as *const msghdr as usize, MSG_NOSIGNAL as usize) as isize;
        if r == -32 || r == -104 { return Ok(false); }
        if r <= 0 { return Err(r); }
        off += r as usize;
    }
    Ok(true)
}

/// Receive message `i` (`len` bytes) and check its pattern; received fds
/// are checked to be valid and closed. `Ok(false)` = EOF at a message start.
unsafe fn stress_recv(fd: i32, i: usize, len: usize) -> Result<bool, isize> {
    let mut cm = CmsgBuf { b: [0; 32] };
    let mut off = 0usize;
    while off < len {
        let mut iov = iovec { iov_base: RBUF.as_mut_ptr().add(off), iov_len: len - off };
        let mut m: msghdr = core::mem::zeroed();
        m.msg_iov = &mut iov; m.msg_iovlen = 1;
        m.msg_control = cm.b.as_mut_ptr(); m.msg_controllen = cm.b.len();
        let r = syscall3(SYS_RECVMSG, fd as usize, &mut m as *mut msghdr as usize, 0) as isize;
        if r == 0 && off == 0 { return Ok(false); }
        if r <= 0 { return Err(if r == 0 { -1000 } else { r }); }
        if m.msg_controllen >= cmsg_len(4) {
            let h = cm.b.as_ptr() as *const cmsghdr;
            let n = ((*h).cmsg_len - cmsg_data_off()) / 4;
            for k in 0..n {
                let mut v = 0i32;
                core::ptr::copy_nonoverlapping(cm.b.as_ptr().add(cmsg_data_off() + 4 * k), &mut v as *mut i32 as *mut u8, 4);
                if raw_fcntl(v, 1 /* F_GETFD */, 0) < 0 { return Err(-2000); }
                close(v);
            }
        }
        off += r as usize;
    }
    for j in 0..len { if RBUF[j] != (j + i * 13) as u8 { return Err(-3000); } }
    Ok(true)
}

/// One stress process: ROUNDS socketpairs (stream and seqpacket), each with
/// a forked peer, random sizes up to 64 KiB, 0-2 fds per message, and a
/// random early close by the receiver (sender must see EPIPE, not hang) or by
/// the sender (receiver must read every byte sent, then EOF). Ends with an
/// fd-leak check.
unsafe fn stress_worker(seed: u64) -> i32 {
    RNG = seed.wrapping_mul(0x9E3779B97F4A7C15) | 1;
    let mfd = raw_memfd_create(b"ipcstress\0".as_ptr(), 0);
    let base = syscall1(nr::DUP, mfd as usize) as i32; close(base);
    for round in 0..40usize {
        let kind = if round % 3 == 2 { SOCK_SEQPACKET } else { SOCK_STREAM };
        let mut sv = [0i32; 2];
        if raw_socketpair(AF_UNIX, kind, 0, sv.as_mut_ptr()) != 0 { return 10; }
        let msgs = 8 + (rnd() % 24) as usize;
        let mode = rnd() % 3; // 0 run to the end, 1 receiver closes early, 2 sender stops early
        let cut = (rnd() % msgs as u64) as usize;
        let rx_stop = if mode == 1 { cut } else { msgs };
        let tx_stop = if mode == 2 { cut } else { msgs };
        let state = RNG;
        let pid = fork();
        if pid == 0 {
            close(sv[0]);
            for i in 0..rx_stop {
                let len = 1 + (rnd() % 65536) as usize; let _ = rnd();
                match stress_recv(sv[1], i, len) {
                    Ok(true) => {}
                    Ok(false) => { if i == tx_stop { exit(0); } exit(20); }
                    Err(e) => { printf(b"IPCSTRESS recv round=%lu msg=%lu err=%ld\n\0".as_ptr(), round as u64, i as u64, e as u64, 0); exit(21); }
                }
            }
            if rx_stop == msgs && tx_stop == msgs {
                // Sender closes after the last message: EOF must follow.
                let mut b = [0u8; 1];
                if raw_recv(sv[1], b.as_mut_ptr(), 1, 0) != 0 { exit(22); }
            }
            close(sv[1]);
            exit(0);
        }
        close(sv[1]);
        RNG = state;
        let mut gone = false;
        for i in 0..tx_stop {
            let len = 1 + (rnd() % 65536) as usize;
            let nfds = (rnd() % 3) as usize;
            match stress_send(sv[0], i, len, nfds, mfd) {
                Ok(true) => {}
                Ok(false) => { gone = true; break; }
                Err(e) => { printf(b"IPCSTRESS send round=%lu msg=%lu err=%ld\n\0".as_ptr(), round as u64, i as u64, e as u64, 0); return 11; }
            }
        }
        if gone && mode != 1 { return 12; }
        close(sv[0]);
        let mut st = -1i32;
        wait4(pid, &mut st, 0, core::ptr::null_mut());
        if st != 0 { printf(b"IPCSTRESS peer round=%lu mode=%lu status=%lx\n\0".as_ptr(), round as u64, mode, st as u64, 0); return 13; }
    }
    let b2 = syscall1(nr::DUP, mfd as usize) as i32; close(b2);
    close(mfd);
    if b2 != base { return 14; }
    0
}

/// Several concurrent stress processes; every one must finish cleanly.
pub unsafe fn stress() -> bool {
    const P: usize = 4;
    let mut w = [0i32; P];
    for k in 0..P {
        let pid = fork();
        if pid == 0 { exit(stress_worker(k as u64 + 1)); }
        w[k] = pid;
    }
    let mut ok = true;
    for k in 0..P {
        let mut st = -1i32;
        wait4(w[k], &mut st, 0, core::ptr::null_mut());
        if st != 0 { ok = false; printf(b"IPCSTRESS worker %lu status %lx\n\0".as_ptr(), k as u64, st as u64, 0, 0); }
    }
    ok
}
