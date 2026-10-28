//! Thread churn must not grow the address space (lane threadleak, 2026-10-27).
//!
//! relibc used to keep every exited thread's 1 MiB stack and its TLS/TCB
//! mapping forever: a process that spawns and joins short-lived threads grew
//! by ~3 VMAs and ~1 MiB of address space per thread (epolltest churn reached
//! 33 GB virtual, and every later fork() had to copy the growing map).
//! This spawns and joins N joinable threads, then N/2 detached ones, and
//! requires the VMA count, VmSize and VmRSS to stay flat within a small bound.

use super::*;
use core::sync::atomic::{AtomicU32, Ordering};

#[derive(Clone, Copy)]
struct Sample { vmas: u64, vsz_kb: u64, rss_kb: u64 }

unsafe fn count_maps() -> u64 {
    let fd = open(b"/proc/self/maps\0".as_ptr(), 0);
    if fd < 0 { return u64::MAX; }
    let mut buf = [0u8; 4096];
    let mut n = 0u64;
    loop {
        let r = read(fd, buf.as_mut_ptr(), buf.len());
        if r <= 0 { break; }
        n += buf[..r as usize].iter().filter(|&&c| c == b'\n').count() as u64;
    }
    close(fd);
    n
}

fn field(buf: &[u8], key: &[u8]) -> u64 {
    let mut i = 0;
    while i + key.len() <= buf.len() {
        if (i == 0 || buf[i - 1] == b'\n') && &buf[i..i + key.len()] == key {
            let mut v = 0u64;
            for &c in &buf[i + key.len()..] {
                if c.is_ascii_digit() { v = v * 10 + (c - b'0') as u64; }
                else if c == b'\n' { break; }
            }
            return v;
        }
        i += 1;
    }
    u64::MAX
}

unsafe fn sample() -> Sample {
    let mut buf = [0u8; 4096];
    let mut len = 0usize;
    let fd = open(b"/proc/self/status\0".as_ptr(), 0);
    if fd >= 0 {
        loop {
            let r = read(fd, buf.as_mut_ptr().add(len), buf.len() - len);
            if r <= 0 { break; }
            len += r as usize;
            if len == buf.len() { break; }
        }
        close(fd);
    }
    Sample { vmas: count_maps(), vsz_kb: field(&buf[..len], b"VmSize:"), rss_kb: field(&buf[..len], b"VmRSS:") }
}

unsafe fn put_dec(mut v: u64) {
    let mut b = [0u8; 20];
    let mut i = b.len();
    if v == 0 { i -= 1; b[i] = b'0'; }
    while v > 0 { i -= 1; b[i] = b'0' + (v % 10) as u8; v /= 10; }
    write(1, b.as_ptr().add(i), b.len() - i);
}

unsafe fn put_sample(tag: &[u8], done: u64, s: Sample) {
    write(1, b"CHURN ".as_ptr(), 6);
    write(1, tag.as_ptr(), tag.len());
    write(1, b" threads=".as_ptr(), 9); put_dec(done);
    write(1, b" vmas=".as_ptr(), 6); put_dec(s.vmas);
    write(1, b" vsz_kb=".as_ptr(), 8); put_dec(s.vsz_kb);
    write(1, b" rss_kb=".as_ptr(), 8); put_dec(s.rss_kb);
    write(1, b"\n".as_ptr(), 1);
}

static DETACHED_DONE: AtomicU32 = AtomicU32::new(0);

extern "C" fn churn_joinable(arg: *mut c_void) -> *mut c_void {
    // Touch a few stack pages so a leaked stack also shows up in RSS.
    let mut pad = [0u8; 12 * 1024];
    unsafe { core::ptr::write_volatile(pad.as_mut_ptr(), 1); core::ptr::write_volatile(pad.as_mut_ptr().add(8 * 1024), 1); }
    core::hint::black_box(&mut pad);
    arg
}

extern "C" fn churn_detached(_arg: *mut c_void) -> *mut c_void {
    DETACHED_DONE.fetch_add(1, Ordering::SeqCst);
    core::ptr::null_mut()
}

unsafe fn spawn_join(n: u64) -> bool {
    for i in 0..n {
        let mut t: pthread_t = core::ptr::null_mut();
        if pthread_create(&mut t, core::ptr::null(), churn_joinable, i as *mut c_void) != 0 { return false; }
        let mut rv: *mut c_void = core::ptr::null_mut();
        if pthread_join(t, &mut rv) != 0 || rv as u64 != i { return false; }
    }
    true
}

fn within(a: Sample, b: Sample) -> bool {
    // Allow a little allocator/heap growth, never per-thread growth.
    b.vmas <= a.vmas + 16 && b.vsz_kb <= a.vsz_kb + 16 * 1024 && b.rss_kb <= a.rss_kb + 2048
}

/// `n` joinable threads (sampled every 1000), then `n / 2` detached ones.
pub unsafe fn test_thread_churn_flat(n: u64) -> bool {
    let name = b"thread_churn_flat\0";
    if !spawn_join(100) { return report(name, false); }
    let base = sample();
    put_sample(b"join", 0, base);
    let mut done = 0;
    let mut last = base;
    while done < n {
        let step = core::cmp::min(1000, n - done);
        if !spawn_join(step) { return report(name, false); }
        done += step;
        last = sample();
        put_sample(b"join", done, last);
    }
    let ok_join = within(base, last);

    let nd = (n / 2) as u32;
    DETACHED_DONE.store(0, Ordering::SeqCst);
    for i in 0..nd {
        let mut t: pthread_t = core::ptr::null_mut();
        if pthread_create(&mut t, core::ptr::null(), churn_detached, core::ptr::null_mut()) != 0 { return report(name, false); }
        if pthread_detach(t) != 0 { return report(name, false); }
        // Bound the number of live detached threads.
        while i >= DETACHED_DONE.load(Ordering::SeqCst) + 64 { usleep(100); }
    }
    while DETACHED_DONE.load(Ordering::SeqCst) < nd { usleep(1000); }
    usleep(20_000);
    // A last joinable round gives a deferred reaper a chance to run.
    if !spawn_join(10) { return report(name, false); }
    let det = sample();
    put_sample(b"detach", nd as u64, det);
    let ok_det = within(base, det);

    // Cancellation racing thread exit (threadleak2): the cancel signal may
    // land while the thread is already exiting. Neither the joinable nor the
    // detached case may run the exit path twice (double post / zombie cycle).
    let nc = core::cmp::max(n / 5, 100);
    for i in 0..nc {
        let mut t: pthread_t = core::ptr::null_mut();
        if pthread_create(&mut t, core::ptr::null(), churn_joinable, i as *mut c_void) != 0 { return report(name, false); }
        pthread_cancel(t);
        if i % 2 == 0 {
            let mut rv: *mut c_void = core::ptr::null_mut();
            if pthread_join(t, &mut rv) != 0 { return report(name, false); }
        } else if pthread_detach(t) != 0 { return report(name, false); }
        if i % 64 == 63 { usleep(2000); }
    }
    usleep(50_000);
    if !spawn_join(10) { return report(name, false); }
    let cs = sample();
    put_sample(b"cancel", nc, cs);
    report(name, ok_join && ok_det && within(base, cs))
}
