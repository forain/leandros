//! gpuqtest — the virtio-gpu control-queue / fence contract (lane gpuq).
//!
//! Raw ioctls on /dev/dri/card0, no libdrm, no Mesa. Each case prints
//! `<NAME>: PASS|FAIL|SKIP`; the exit code is the failure count.
//!
//!   VERSION_NAME_LEN        DRM_IOCTL_VERSION reports strlen (no NUL), the
//!                           way upstream's drm_copy_field does, so libdrm's
//!                           two-pass read gets the bytes it asked for.
//!   FENCE_LATENCY_SOLO      N fenced submits, each waited on through its
//!                           out-fence fd; prints mean/p50/p99/max (us).
//!   STRESS_TWO_PROCS        the same loop in one process while a second one
//!                           submits fenced work and exits without
//!                           waiting (its context is torn down with fences in
//!                           flight), several rounds. The waiter must never
//!                           time out and the ring must not leak.
//!   STRESS_RING_NOT_LEAKED  after the stress, the control queue has no
//!                           chains parked for good (kernel counter).
//!   RING_IDX_NO_RINGS_EINVAL a RING_IDX submit on a context with no rings
//!                           (implicit context) is EINVAL, as upstream.
//!   REFUSED_RING_FAST       a RING_IDX submit on a ring the host cannot
//!                           serve (virgl capset, ring 1) is answered with an
//!                           error at once, or its fence completes — never a
//!                           fence that stays pending until the 10 s watchdog.
//!   FENCE_FD_IN_NOT_A_FENCE FENCE_FD_IN naming an fd that is not a fence is
//!                           EINVAL (upstream: sync_file_get_fence fails).
//!   FENCE_FD_IN_WAITS       a submission with FENCE_FD_IN returns only once
//!                           the in-fence has signalled.
//!   OUT_SYNCOBJ_INSTALLED   out_syncobjs get the submission's fence: a
//!                           SYNCOBJ_WAIT on it returns 0 once it retires.
//!   IN_SYNCOBJ_NULL_EINVAL  an in-syncobj with no fence is EINVAL
//!                           (drm_syncobj_find_fence), not ignored.
//!   IN_SYNCOBJ_RESET        an in-syncobj with VIRTGPU_EXECBUF_SYNCOBJ_RESET
//!                           is reset after the submission.
//!
//! `--venus` adds DEAD_CTX_FAST: a Venus context whose render worker dies on
//! a malformed stream has its ringed fence written off within a few seconds
//! and its next submission refused.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;

type c_int = i32;
type c_uint = u32;
type c_ulong = u64;
type size_t = usize;

const O_RDWR: c_int = 0o2;
const POLLIN: i16 = 0x1;
const EINVAL: i32 = 22;

const DRM_IOCTL_VERSION: c_ulong = 0xC0406400;
const DRM_IOCTL_VIRTGPU_EXECBUFFER: c_ulong = 0xC0406442;
const DRM_IOCTL_VIRTGPU_GETPARAM: c_ulong = 0xC0106443;
const DRM_IOCTL_VIRTGPU_CONTEXT_INIT: c_ulong = 0xC010644B;
const DRM_IOCTL_SYNCOBJ_CREATE: c_ulong = 0xC00864BF;
const DRM_IOCTL_SYNCOBJ_DESTROY: c_ulong = 0xC00864C0;
const DRM_IOCTL_SYNCOBJ_WAIT: c_ulong = 0xC02064C3;
/// LeandrOS: how long a closing open waits for its context's fenced work
/// (one u64, new in / old out; u64::MAX = default; 0 = abandon at once).
const DRM_IOCTL_LEANDROS_CTX_DRAIN_US: c_ulong = 0x100A;

const VIRTGPU_EXECBUF_FENCE_FD_IN: u32 = 0x01;
const VIRTGPU_EXECBUF_FENCE_FD_OUT: u32 = 0x02;
const VIRTGPU_EXECBUF_RING_IDX: u32 = 0x04;
const VIRTGPU_EXECBUF_SYNCOBJ_RESET: u32 = 0x01;
const VIRTGPU_CONTEXT_PARAM_CAPSET_ID: u64 = 0x0001;
const VIRTGPU_CONTEXT_PARAM_NUM_RINGS: u64 = 0x0002;
const VIRTGPU_PARAM_3D_FEATURES: u64 = 1;
/// LeandrOS: control-queue census; `value` carries a selector in, the count out.
const VIRTGPU_PARAM_LEANDROS_CTRLQ_STAT: u64 = 0x1000_000A;
const STAT_PARKED: u32 = 0;
const STAT_RECLAIMED: u32 = 1;
const STAT_DEAD_CTXS: u32 = 2;
const STAT_INFLIGHT: u32 = 3;
const STAT_ZOMBIES: u32 = 4;
const STAT_ROOM_WAITS_UNLOCKED: u32 = 5;
const STAT_IN_FENCE_WAITS: u32 = 6;

const CLOCK_MONOTONIC: c_int = 1;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct timespec { tv_sec: i64, tv_nsec: i64 }

#[repr(C)]
struct pollfd { fd: c_int, events: i16, revents: i16 }

#[repr(C)]
#[derive(Default)]
struct DrmVersion {
    version_major: i32,
    version_minor: i32,
    version_patchlevel: i32,
    name_len: usize,
    name: u64,
    date_len: usize,
    date: u64,
    desc_len: usize,
    desc: u64,
}

#[repr(C)]
#[derive(Default)]
struct DrmVirtgpuExecbuffer {
    flags: u32,
    size: u32,
    command: u64,
    bo_handles: u64,
    num_bo_handles: u32,
    fence_fd: i32,
    ring_idx: u32,
    syncobj_stride: u32,
    num_in_syncobjs: u32,
    num_out_syncobjs: u32,
    in_syncobjs: u64,
    out_syncobjs: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ExecSyncobj { handle: u32, flags: u32, point: u64 }

#[repr(C)]
#[derive(Default)]
struct DrmVirtgpuGetparam { param: u64, value: u64 }

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CtxSetParam { param: u64, value: u64 }

#[repr(C)]
#[derive(Default)]
struct CtxInit { num_params: u32, pad: u32, ctx_set_params: u64 }

#[repr(C)]
#[derive(Default)]
struct SyncobjCreate { handle: u32, flags: u32 }

#[repr(C)]
#[derive(Default)]
struct SyncobjWait {
    handles: u64,
    timeout_nsec: i64,
    count_handles: u32,
    flags: u32,
    first_signaled: u32,
    pad: u32,
}

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;
    pub fn write(fd: c_int, buf: *const c_void, count: size_t) -> isize;
    pub fn exit(status: c_int) -> !;
    pub fn _exit(status: c_int) -> !;
    pub fn open(path: *const u8, oflag: c_int, ...) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn fork() -> i32;
    pub fn waitpid(pid: i32, stat_loc: *mut c_int, options: c_int) -> i32;
    pub fn usleep(usec: c_uint) -> c_int;
    pub fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    pub fn poll(fds: *mut pollfd, nfds: u64, timeout: c_int) -> c_int;
    pub fn pipe(fds: *mut c_int) -> c_int;
    pub fn clock_gettime(clk_id: c_int, tp: *mut timespec) -> c_int;
    pub fn __errno_location() -> *mut c_int;
}

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset gpuq_main",
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
    "   adrp x1, gpuq_main",
    "   add x1, x1, :lo12:gpuq_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

unsafe fn errno() -> i32 { *__errno_location() }

unsafe fn now_ns() -> u64 {
    let mut ts = timespec::default();
    if clock_gettime(CLOCK_MONOTONIC, &mut ts as *mut _) != 0 { return 0; }
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

fn out(s: &[u8]) { unsafe { write(1, s.as_ptr() as *const c_void, s.len()); } }

fn dec(label: &[u8], v: u64) {
    out(label);
    let mut buf = [0u8; 20];
    let mut n = 0usize;
    let mut x = v;
    if x == 0 { buf[0] = b'0'; n = 1; }
    while x > 0 { buf[n] = b'0' + (x % 10) as u8; n += 1; x /= 10; }
    let mut rev = [0u8; 20];
    for i in 0..n { rev[i] = buf[n - 1 - i]; }
    out(&rev[..n]);
}

fn report(name: &[u8], ok: bool) -> bool {
    out(name);
    out(if ok { b": PASS\n" } else { b": FAIL\n" });
    ok
}

fn skip(name: &[u8]) { out(name); out(b": SKIP\n"); }

fn arg_is(p: *const u8, s: &[u8]) -> bool {
    if p.is_null() { return false; }
    unsafe {
        for (i, &c) in s.iter().enumerate() { if *p.add(i) != c { return false; } }
        *p.add(s.len()) == 0
    }
}

unsafe fn open_card() -> c_int { open(b"/dev/dri/card0\0".as_ptr(), O_RDWR) }

unsafe fn getparam(fd: c_int, param: u64, input: u32) -> Option<u32> {
    let mut io = input;
    let mut g = DrmVirtgpuGetparam { param, value: &mut io as *mut u32 as u64 };
    if ioctl(fd, DRM_IOCTL_VIRTGPU_GETPARAM, &mut g as *mut _) == 0 { Some(io) } else { None }
}

unsafe fn stat(fd: c_int, sel: u32) -> Option<u32> { getparam(fd, VIRTGPU_PARAM_LEANDROS_CTRLQ_STAT, sel) }

unsafe fn ctx_init(fd: c_int, capset: u64, rings: u64) -> bool {
    let params = [
        CtxSetParam { param: VIRTGPU_CONTEXT_PARAM_CAPSET_ID, value: capset },
        CtxSetParam { param: VIRTGPU_CONTEXT_PARAM_NUM_RINGS, value: rings },
    ];
    let mut init = CtxInit { num_params: 2, pad: 0, ctx_set_params: params.as_ptr() as u64 };
    ioctl(fd, DRM_IOCTL_VIRTGPU_CONTEXT_INIT, &mut init as *mut _) == 0
}

/// One EXECBUFFER. Returns (rc, errno, fence_fd as written back).
unsafe fn exec(fd: c_int, flags: u32, words: &[u32], ring: u32, fence_in: i32,
               ins: &[ExecSyncobj], outs: &[ExecSyncobj]) -> (c_int, i32, i32) {
    let mut eb = DrmVirtgpuExecbuffer::default();
    eb.flags = flags;
    eb.size = (words.len() * 4) as u32;
    eb.command = if words.is_empty() { 0 } else { words.as_ptr() as u64 };
    eb.fence_fd = fence_in;
    eb.ring_idx = ring;
    eb.syncobj_stride = core::mem::size_of::<ExecSyncobj>() as u32;
    eb.num_in_syncobjs = ins.len() as u32;
    eb.in_syncobjs = if ins.is_empty() { 0 } else { ins.as_ptr() as u64 };
    eb.num_out_syncobjs = outs.len() as u32;
    eb.out_syncobjs = if outs.is_empty() { 0 } else { outs.as_ptr() as u64 };
    let rc = ioctl(fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, &mut eb as *mut _);
    let e = if rc != 0 { errno() } else { 0 };
    (rc, e, eb.fence_fd)
}

unsafe fn ready_within(fd: c_int, ms: i32) -> bool {
    let mut p = pollfd { fd, events: POLLIN, revents: 0 };
    poll(&mut p as *mut _, 1, ms) == 1 && (p.revents & POLLIN) != 0
}

unsafe fn syncobj_create(fd: c_int, signaled: bool) -> u32 {
    let mut c = SyncobjCreate { handle: 0, flags: if signaled { 1 } else { 0 } };
    if ioctl(fd, DRM_IOCTL_SYNCOBJ_CREATE, &mut c as *mut _) == 0 { c.handle } else { 0 }
}

unsafe fn syncobj_destroy(fd: c_int, h: u32) {
    let mut d = [h, 0u32];
    ioctl(fd, DRM_IOCTL_SYNCOBJ_DESTROY, d.as_mut_ptr());
}

/// SYNCOBJ_WAIT on one handle, `ms` from now. Returns (rc, errno).
unsafe fn syncobj_wait(fd: c_int, h: u32, ms: u64) -> (c_int, i32) {
    let hs = [h];
    let mut w = SyncobjWait {
        handles: hs.as_ptr() as u64,
        timeout_nsec: (now_ns() + ms * 1_000_000) as i64,
        count_handles: 1,
        ..Default::default()
    };
    let rc = ioctl(fd, DRM_IOCTL_SYNCOBJ_WAIT, &mut w as *mut _);
    (rc, if rc != 0 { errno() } else { 0 })
}

const NOP: [u32; 1] = [0];

// ── latency census ───────────────────────────────────────────────────────────

const MAX_SAMPLES: usize = 1024;

struct Lat { us: [u32; MAX_SAMPLES], n: usize, timeouts: u32, errors: u32 }

impl Lat {
    const fn new() -> Self { Lat { us: [0; MAX_SAMPLES], n: 0, timeouts: 0, errors: 0 } }
    fn summary(&mut self, tag: &[u8]) {
        let n = self.n;
        let v = &mut self.us[..n];
        // insertion sort: n <= 1024, run once
        for i in 1..n {
            let x = v[i];
            let mut j = i;
            while j > 0 && v[j - 1] > x { v[j] = v[j - 1]; j -= 1; }
            v[j] = x;
        }
        let sum: u64 = v.iter().map(|&x| x as u64).sum();
        out(tag);
        dec(b" n=", n as u64);
        if n > 0 {
            dec(b" mean_us=", sum / n as u64);
            dec(b" p50_us=", v[n / 2] as u64);
            dec(b" p99_us=", v[(n * 99 / 100).min(n - 1)] as u64);
            dec(b" max_us=", v[n - 1] as u64);
        }
        dec(b" timeouts=", self.timeouts as u64);
        dec(b" errors=", self.errors as u64);
        out(b"\n");
    }
}

/// `iters` fenced submits on `fd`, each waited on (2 s) through its out-fence
/// fd. Records submit-to-signal latency. Gives up after 3 failures in a row.
unsafe fn latency_loop(fd: c_int, iters: usize, lat: &mut Lat) {
    let mut bad_run = 0;
    for _ in 0..iters {
        let t0 = now_ns();
        let (rc, _, ffd) = exec(fd, VIRTGPU_EXECBUF_FENCE_FD_OUT, &NOP, 0, 0, &[], &[]);
        if rc != 0 || ffd < 0 {
            lat.errors += 1;
            bad_run += 1;
            if bad_run >= 3 { return; }
            continue;
        }
        let ok = ready_within(ffd, 2000);
        let dt = (now_ns() - t0) / 1000;
        close(ffd);
        if !ok {
            lat.timeouts += 1;
            bad_run += 1;
            if bad_run >= 3 { return; }
            continue;
        }
        bad_run = 0;
        if lat.n < MAX_SAMPLES { lat.us[lat.n] = dt.min(u32::MAX as u64) as u32; lat.n += 1; }
    }
}

/// The abandoning process: `rounds` x (open, `per` ringed fenced submits,
/// close every out-fence unread, close the device).
unsafe fn abandoner(rounds: u32, per: u32) -> i32 {
    let mut errs = 0;
    for _ in 0..rounds {
        let fd = open_card();
        if fd < 0 { return 100; }
        for _ in 0..per {
            // Plain fenced work, as a Mesa virgl client killed mid-frame
            // leaves it (virgl never sets RING_IDX).
            let (rc, _, ffd) = exec(fd, VIRTGPU_EXECBUF_FENCE_FD_OUT, &NOP, 0, 0, &[], &[]);
            if rc != 0 { errs += 1; } else if ffd >= 0 { close(ffd); }
        }
        close(fd);
    }
    errs
}

static mut SOLO: Lat = Lat::new();
static mut STRESSED: Lat = Lat::new();

#[no_mangle]
pub unsafe extern "C" fn gpuq_main(argc: isize, argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let mut venus = false;
    let mut iters = 300usize;
    let mut i = 1isize;
    while i < argc {
        let a = *argv.add(i as usize) as *const u8;
        if arg_is(a, b"--venus") { venus = true; }
        if arg_is(a, b"--quick") { iters = 60; }
        i += 1;
    }
    let mut failures = 0i32;
    out(b"--- gpuqtest ---\n");

    let fd = open_card();
    if fd < 0 { report(b"open_card0", false); return 1; }
    let three_d = getparam(fd, VIRTGPU_PARAM_3D_FEATURES, 0).unwrap_or(0) != 0;
    let census = stat(fd, STAT_PARKED).is_some();

    // ── VERSION_NAME_LEN ────────────────────────────────────────────────────
    {
        let mut v = DrmVersion::default();
        let rc1 = ioctl(fd, DRM_IOCTL_VERSION, &mut v as *mut _);
        let (nl, dl, el) = (v.name_len, v.date_len, v.desc_len);
        let mut nb = [0xAAu8; 64];
        let mut db = [0xAAu8; 64];
        let mut eb = [0xAAu8; 128];
        let mut v2 = DrmVersion::default();
        v2.name_len = nl; v2.name = nb.as_mut_ptr() as u64;
        v2.date_len = dl; v2.date = db.as_mut_ptr() as u64;
        v2.desc_len = el; v2.desc = eb.as_mut_ptr() as u64;
        let rc2 = if nl < 64 && dl < 64 && el < 128 { ioctl(fd, DRM_IOCTL_VERSION, &mut v2 as *mut _) } else { -1 };
        // libdrm terminates at [len]; every byte before it must be the string.
        let clean = |b: &[u8], n: usize| n > 0 && b[..n].iter().all(|&c| c != 0 && c != 0xAA);
        let ok = rc1 == 0 && rc2 == 0 && v2.name_len == nl
            && clean(&nb, nl) && clean(&db, dl) && clean(&eb, el);
        if !ok { dec(b"  name_len=", nl as u64); dec(b"  last name byte=", nb[nl.saturating_sub(1).min(63)] as u64); }
        if !report(b"VERSION_NAME_LEN", ok) { failures += 1; }
    }

    if !three_d {
        out(b"gpuqtest: no virgl/3D on this device; GPU cases skipped\n");
        dec(b"gpuqtest: failed=", failures as u64);
        return failures;
    }

    // ── FENCE_LATENCY_SOLO ──────────────────────────────────────────────────
    latency_loop(fd, iters, &mut *core::ptr::addr_of_mut!(SOLO));
    (*core::ptr::addr_of_mut!(SOLO)).summary(b"gpuqtest: latency solo");
    let s = &*core::ptr::addr_of!(SOLO);
    if !report(b"FENCE_LATENCY_SOLO", s.timeouts == 0 && s.errors == 0 && s.n == iters) { failures += 1; }

    // ── STRESS_TWO_PROCS ────────────────────────────────────────────────────
    {
        // Abandon at once, so the parked-chain path runs whatever the host's
        // timing (root only; a non-root run keeps the default drain).
        let mut drain = 0u64;
        let knob = ioctl(fd, DRM_IOCTL_LEANDROS_CTX_DRAIN_US, &mut drain as *mut u64) == 0;
        let old_drain = drain;
        let parked0 = stat(fd, STAT_PARKED).unwrap_or(0);
        let pid = fork();
        if pid == 0 {
            let e = abandoner(8, 40);
            _exit(e.min(120));
        }
        let t0 = now_ns();
        let lfd = open_card();
        latency_loop(lfd, iters, &mut *core::ptr::addr_of_mut!(STRESSED));
        let mut st: c_int = 0;
        waitpid(pid, &mut st as *mut _, 0);
        let child_rc = (st >> 8) & 0xFF;
        dec(b"gpuqtest: stress wall_ms=", (now_ns() - t0) / 1_000_000);
        dec(b"gpuqtest: abandoner rc=", child_rc as u64);
        (*core::ptr::addr_of_mut!(STRESSED)).summary(b"gpuqtest: latency stressed");
        let s = &*core::ptr::addr_of!(STRESSED);
        if !report(b"STRESS_TWO_PROCS", s.timeouts == 0 && s.errors == 0 && s.n == iters && child_rc == 0) {
            failures += 1;
        }
        // The ring still works: a short burst after all that.
        let mut after = Lat::new();
        latency_loop(lfd, 32, &mut after);
        close(lfd);
        if knob {
            let mut back = old_drain;
            ioctl(fd, DRM_IOCTL_LEANDROS_CTX_DRAIN_US, &mut back as *mut u64);
        }
        usleep(300_000);
        if census {
            let parked = stat(fd, STAT_PARKED).unwrap_or(u32::MAX);
            dec(b"gpuqtest: parked chains before=", parked0 as u64);
            dec(b"gpuqtest: parked chains after=", parked as u64);
            dec(b"gpuqtest: reclaimed=", stat(fd, STAT_RECLAIMED).unwrap_or(0) as u64);
            dec(b"gpuqtest: zombies=", stat(fd, STAT_ZOMBIES).unwrap_or(0) as u64);
            dec(b"gpuqtest: inflight=", stat(fd, STAT_INFLIGHT).unwrap_or(0) as u64);
            dec(b"gpuqtest: unlocked room waits=", stat(fd, STAT_ROOM_WAITS_UNLOCKED).unwrap_or(0) as u64);
            if !report(b"STRESS_RING_NOT_LEAKED", after.n == 32 && parked <= parked0) { failures += 1; }
        } else if !report(b"STRESS_RING_NOT_LEAKED", after.n == 32) {
            failures += 1;
        }
    }

    // ── RING_IDX_NO_RINGS_EINVAL ────────────────────────────────────────────
    // Upstream: a context with no rings (implicit, or CONTEXT_INIT without
    // NUM_RINGS) has no fence context, so any RING_IDX submit is EINVAL.
    {
        let nfd = open_card();
        let (rc, e, ffd) = exec(nfd, VIRTGPU_EXECBUF_FENCE_FD_OUT | VIRTGPU_EXECBUF_RING_IDX,
                                &NOP, 0, 0, &[], &[]);
        if rc == 0 && ffd >= 0 { close(ffd); }
        if !report(b"RING_IDX_NO_RINGS_EINVAL", nfd >= 0 && rc != 0 && e == EINVAL) {
            dec(b"  rc+1=", (rc + 1) as u64);
            failures += 1;
        }
        if nfd >= 0 { close(nfd); }
    }

    // ── REFUSED_RING_FAST ───────────────────────────────────────────────────
    {
        let rfd = open_card();
        if rfd >= 0 && ctx_init(rfd, 2, 2) {
            let t0 = now_ns();
            let (rc, e, ffd) = exec(rfd, VIRTGPU_EXECBUF_FENCE_FD_OUT | VIRTGPU_EXECBUF_RING_IDX,
                                    &NOP, 1, 0, &[], &[]);
            let ret_us = (now_ns() - t0) / 1000;
            let ok = if rc != 0 {
                dec(b"  refused at submit, errno=", e as u64);
                ret_us < 100_000
            } else {
                let sig = ffd >= 0 && ready_within(ffd, 3000);
                dec(b"  accepted; fence signalled within 3 s=", sig as u64);
                if ffd >= 0 { close(ffd); }
                sig
            };
            dec(b"  submit returned after us=", ret_us);
            if !report(b"REFUSED_RING_FAST", ok) { failures += 1; }
        } else {
            skip(b"REFUSED_RING_FAST");
        }
        if rfd >= 0 { close(rfd); }
    }

    // ── VIRGL_RING0_EINVAL ──────────────────────────────────────────────────
    // A virgl context created with NUM_RINGS=1 must not forward ring-0 fences
    // either (closing it mid-flight hangs the host like ring>0 did).
    {
        let rfd = open_card();
        if rfd >= 0 && ctx_init(rfd, 2, 1) {
            let (rc, e, ffd) = exec(rfd, VIRTGPU_EXECBUF_FENCE_FD_OUT | VIRTGPU_EXECBUF_RING_IDX,
                                    &NOP, 0, 0, &[], &[]);
            if rc == 0 && ffd >= 0 { close(ffd); }
            if !report(b"VIRGL_RING0_EINVAL", rc != 0 && e == EINVAL) {
                dec(b"  rc+1=", (rc + 1) as u64);
                failures += 1;
            }
        } else {
            skip(b"VIRGL_RING0_EINVAL");
        }
        if rfd >= 0 { close(rfd); }
    }

    // ── FENCE_FD_IN ─────────────────────────────────────────────────────────
    {
        // A pipe: a real fd, but not a fence.
        let mut pp = [-1i32; 2];
        pipe(pp.as_mut_ptr());
        let efd = pp[0];
        let (rc, e, ffd) = exec(fd, VIRTGPU_EXECBUF_FENCE_FD_IN, &NOP, 0, efd, &[], &[]);
        if !report(b"FENCE_FD_IN_NOT_A_FENCE", rc != 0 && e == EINVAL) {
            dec(b"  rc+1=", (rc + 1) as u64);
            failures += 1;
        }
        let _ = ffd;
        close(pp[0]);
        close(pp[1]);

        // The in-fence is the newest of a burst, so it is usually still in
        // flight when the dependent submission is made. Five tries; at least
        // one must have caught it pending, or the case proves nothing.
        let mut pending_seen = 0;
        let mut violations = 0;
        for _ in 0..5 {
            let mut last = -1;
            for _ in 0..16 {
                let (rc, _, f) = exec(fd, VIRTGPU_EXECBUF_FENCE_FD_OUT, &NOP, 0, 0, &[], &[]);
                if rc == 0 && f >= 0 { if last >= 0 { close(last); } last = f; }
            }
            if last < 0 { violations += 1; continue; }
            if !ready_within(last, 0) { pending_seen += 1; }
            let (rc, _, f2) = exec(fd, VIRTGPU_EXECBUF_FENCE_FD_IN | VIRTGPU_EXECBUF_FENCE_FD_OUT,
                                   &NOP, 0, last, &[], &[]);
            if rc != 0 || !ready_within(last, 0) { violations += 1; }
            if rc == 0 && f2 >= 0 { ready_within(f2, 2000); close(f2); }
            close(last);
        }
        dec(b"  in-fence pending at submit in tries=", pending_seen as u64);
        if !report(b"FENCE_FD_IN_WAITS", violations == 0 && pending_seen > 0) { failures += 1; }
    }

    // ── syncobjs ────────────────────────────────────────────────────────────
    {
        let s_out = syncobj_create(fd, false);
        let o = [ExecSyncobj { handle: s_out, flags: 0, point: 0 }];
        let (rc, _, _) = exec(fd, 0, &NOP, 0, 0, &[], &o);
        let (wrc, we) = syncobj_wait(fd, s_out, 2000);
        if !report(b"OUT_SYNCOBJ_INSTALLED", s_out != 0 && rc == 0 && wrc == 0) {
            dec(b"  submit rc+1=", (rc + 1) as u64);
            dec(b"  wait errno=", we as u64);
            failures += 1;
        }
        syncobj_destroy(fd, s_out);

        let s_null = syncobj_create(fd, false);
        let n = [ExecSyncobj { handle: s_null, flags: 0, point: 0 }];
        let (rc, e, _) = exec(fd, 0, &NOP, 0, 0, &n, &[]);
        if !report(b"IN_SYNCOBJ_NULL_EINVAL", s_null != 0 && rc != 0 && e == EINVAL) {
            dec(b"  rc+1=", (rc + 1) as u64);
            failures += 1;
        }
        syncobj_destroy(fd, s_null);

        let s_sig = syncobj_create(fd, true);
        let r = [ExecSyncobj { handle: s_sig, flags: VIRTGPU_EXECBUF_SYNCOBJ_RESET, point: 0 }];
        let (rc, _, _) = exec(fd, 0, &NOP, 0, 0, &r, &[]);
        // Reset leaves the NULL fence: a wait without WAIT_FOR_SUBMIT is EINVAL.
        let (wrc, we) = syncobj_wait(fd, s_sig, 0);
        if !report(b"IN_SYNCOBJ_RESET", s_sig != 0 && rc == 0 && wrc != 0 && we == EINVAL) {
            dec(b"  submit rc+1=", (rc + 1) as u64);
            dec(b"  wait rc+1=", (wrc + 1) as u64);
            failures += 1;
        }
        syncobj_destroy(fd, s_sig);
    }

    // ── DEAD_CTX_FAST (Venus only) ──────────────────────────────────────────
    if venus {
        let vfd = open_card();
        if vfd >= 0 && ctx_init(vfd, 4, 1) {
            // Not a Venus stream: the render worker rejects it and exits.
            let junk: [u32; 8] = [0xdead_beef; 8];
            let t0 = now_ns();
            let (rc, _, ffd) = exec(vfd, VIRTGPU_EXECBUF_FENCE_FD_OUT | VIRTGPU_EXECBUF_RING_IDX,
                                    &junk, 0, 0, &[], &[]);
            let sig = rc == 0 && ffd >= 0 && ready_within(ffd, 5000);
            let dt_ms = (now_ns() - t0) / 1_000_000;
            if ffd >= 0 { close(ffd); }
            let (rc2, e2, f2) = exec(vfd, VIRTGPU_EXECBUF_FENCE_FD_OUT | VIRTGPU_EXECBUF_RING_IDX,
                                     &junk, 0, 0, &[], &[]);
            if rc2 == 0 && f2 >= 0 { close(f2); }
            dec(b"  fence written off after ms=", dt_ms);
            dec(b"  next submit errno=", e2 as u64);
            dec(b"  dead contexts=", stat(fd, STAT_DEAD_CTXS).unwrap_or(0) as u64);
            if !report(b"DEAD_CTX_FAST", sig && dt_ms < 5000 && rc2 != 0) { failures += 1; }
        } else {
            skip(b"DEAD_CTX_FAST");
        }
        if vfd >= 0 { close(vfd); }
    }

    if census {
        dec(b"gpuqtest: in-fence waits=", stat(fd, STAT_IN_FENCE_WAITS).unwrap_or(0) as u64);
        dec(b"gpuqtest: dead contexts=", stat(fd, STAT_DEAD_CTXS).unwrap_or(0) as u64);
    }
    close(fd);
    dec(b"gpuqtest: failed=", failures as u64);
    out(b"--- gpuqtest done ---\n");
    failures
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}
