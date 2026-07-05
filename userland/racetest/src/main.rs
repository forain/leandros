//! racetest — regression coverage for the three SMP hazards found during
//! the TCG IRQ-window-stall investigation:
//!
//!   1. `ipc_ping_pong` — the sys_recv check-then-block lost-wake race: a
//!      cross-CPU sender enqueuing between the receiver's empty queue check
//!      and its Blocked transition used to leave the message queued while
//!      the receiver slept forever.  Two processes ping-pong thousands of
//!      messages over raw kernel ports (created via SYS_PORT_CREATE, which
//!      exists exactly for this test); every blocking recv is one shot at
//!      the race window, so a regression shows up as a hang (caught by the
//!      watchdog thread, since the harness may not time out).
//!
//!   2. `eevdf_poller_latency` / `eevdf_cpu_fairness` — EEVDF vruntime
//!      accounting at sub-tick granularity (VT_PER_TICK): pollers used to
//!      be charged a full 10 ms tick per microsecond dispatch, inflating
//!      their vruntime ~1000× and starving them once runnable tasks
//!      exceeded CPUs; conversely, charging pollers nothing would let them
//!      pin the eligibility average and starve CPU-bound tasks.  The pair
//!      of checks bounds both directions: a nanosleep poller keeps sane
//!      latency under 6 CPU-bound children, and a CPU-bound worker isn't
//!      slowed unboundedly by 6 polling children.
//!
//!   3. `pf_storm_processes` / `pf_storm_threads` — page-fault service off
//!      the run-queue lock: the fault path (allocate + copy up to 4 KiB +
//!      map) used to run holding RUN_QUEUE, stalling every other CPU's
//!      scheduler and convoying with TLB-shootdown waits.  It now runs
//!      under a per-address-space lock, so concurrent faults in ONE
//!      process (threads on different CPUs) need their own mutual
//!      exclusion — the threaded storm would corrupt VMA state if that
//!      lock were wrong, and the multi-process storm exercises fault
//!      storms concurrent with fork/munmap shootdowns.
//!
//! Initializes via relibc_start_v1 (same as pthreadtest/timertest/sigtest/
//! polltest/forktest) so TLS is set up.  Each check prints "<name>: PASS"
//! or "<name>: FAIL"; `race_main` returns the number of failures.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

type c_int = i32;
type c_long = i64;
type time_t = i64;
type pid_t = i32;
type size_t = usize;
pub type pthread_t = *mut c_void;

const PROT_READ:     c_int = 1;
const PROT_WRITE:    c_int = 2;
const MAP_SHARED:    c_int = 0x01;
const MAP_PRIVATE:   c_int = 0x02;
const MAP_ANONYMOUS: c_int = 0x20;
const PAGE:          size_t = 4096;

const CLOCK_MONOTONIC: c_int = 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct timespec {
    pub tv_sec:  time_t,
    pub tv_nsec: c_long,
}

// ── Raw LeandrOS IPC syscalls ────────────────────────────────────────────────
// Numbers from kernel/src/syscall.rs (Leandros-private range).

const SYS_IPC_SEND:    usize = 511;
const SYS_IPC_RECV:    usize = 512;
const SYS_PORT_CREATE: usize = 514;

/// Must match `ipc::Message` in ipc/src/message.rs exactly (repr(C)).
const MESSAGE_INLINE_BYTES: usize = 440;
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Message {
    pub tag:        u64,
    pub reply_port: u32,
    pub data:       [u8; MESSAGE_INLINE_BYTES],
    pub has_cap:    u64,
    pub cap:        u64,
}

impl Message {
    const fn empty() -> Self {
        Self { tag: 0, reply_port: u32::MAX, data: [0; MESSAGE_INLINE_BYTES], has_cap: 0, cap: 0 }
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn syscall2(nr: usize, a0: usize, a1: usize) -> isize {
    let ret: isize;
    core::arch::asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret,
         in("x1") a1, options(nostack));
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn syscall0(nr: usize) -> isize {
    let ret: isize;
    core::arch::asm!("svc #0", in("x8") nr, lateout("x0") ret, options(nostack));
    ret
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn syscall2(nr: usize, a0: usize, a1: usize) -> isize {
    let ret: isize;
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") a0,
        in("rsi") a1,
        out("rcx") _,
        out("r11") _,
        options(nostack),
    );
    ret
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn syscall0(nr: usize) -> isize {
    let ret: isize;
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => ret,
        out("rcx") _,
        out("r11") _,
        options(nostack),
    );
    ret
}

unsafe fn port_create() -> isize {
    syscall0(SYS_PORT_CREATE)
}

unsafe fn ipc_send(port: u32, msg: &Message) -> isize {
    syscall2(SYS_IPC_SEND, port as usize, msg as *const Message as usize)
}

unsafe fn ipc_recv(port: u32, msg: &mut Message) -> isize {
    syscall2(SYS_IPC_RECV, port as usize, msg as *mut Message as usize)
}

// ── libc externs (relibc) ────────────────────────────────────────────────────

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;

    pub fn puts(s: *const u8) -> i32;
    pub fn write(fd: c_int, buf: *const c_void, count: size_t) -> isize;
    pub fn _exit(status: c_int) -> !;

    pub fn fork() -> pid_t;
    pub fn waitpid(pid: pid_t, stat_loc: *mut c_int, options: c_int) -> pid_t;

    pub fn mmap(addr: *mut c_void, len: size_t, prot: c_int, flags: c_int,
                fd: c_int, offset: i64) -> *mut c_void;
    pub fn munmap(addr: *mut c_void, len: size_t) -> c_int;

    pub fn nanosleep(rqtp: *const timespec, rmtp: *mut timespec) -> c_int;
    pub fn clock_gettime(clk: c_int, tp: *mut timespec) -> c_int;

    pub fn pthread_create(
        thread: *mut pthread_t,
        attr: *const c_void,
        start: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> c_int;
    pub fn pthread_join(thread: pthread_t, retval: *mut *mut c_void) -> c_int;
}

// ── Assembly entry point (identical to forktest's) ───────────────────────────

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset race_main",
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
    "   adrp x1, race_main",
    "   add x1, x1, :lo12:race_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { _exit(134); }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

unsafe fn report(name: &[u8], passed: bool) -> bool {
    write(1, name.as_ptr() as *const c_void, name.len() - 1);
    if passed {
        write(1, b": PASS\n".as_ptr() as *const c_void, 7);
    } else {
        write(1, b": FAIL\n".as_ptr() as *const c_void, 7);
    }
    passed
}

/// Print `label` followed by `v` in decimal and a newline (diagnostics).
unsafe fn print_val(label: &[u8], v: u64) {
    write(1, label.as_ptr() as *const c_void, label.len());
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    let mut n = v;
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 { break; }
    }
    write(1, buf.as_ptr().add(i) as *const c_void, buf.len() - i);
    write(1, b"\n".as_ptr() as *const c_void, 1);
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

unsafe fn shared_page() -> *mut u8 {
    let p = mmap(core::ptr::null_mut(), PAGE, PROT_READ | PROT_WRITE,
                 MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if p as isize == -1 { core::ptr::null_mut() } else { p as *mut u8 }
}

// ── Watchdog ─────────────────────────────────────────────────────────────────
//
// A lost IPC wake manifests as a hang (the receiver sleeps forever with the
// message queued), not a failed assertion, so the suite carries its own
// watchdog thread.  DONE is flipped once by race_main when all checks have
// reported.

static DONE: AtomicU32 = AtomicU32::new(0);

extern "C" fn watchdog_thread(_arg: *mut c_void) -> *mut c_void {
    const BUDGET_SECONDS: i64 = 240;
    unsafe {
        for _ in 0..BUDGET_SECONDS {
            sleep_ms(1000);
            if DONE.load(Ordering::SeqCst) != 0 {
                return core::ptr::null_mut();
            }
        }
        puts(b"racetest: WATCHDOG timeout - suite hung: FAIL\n\0".as_ptr());
        _exit(3);
    }
}

// ── 1. sys_recv lost-wake race ───────────────────────────────────────────────
//
// Parent creates port PP before forking (the child inherits the id by plain
// memory copy and may SEND to it; only the parent may recv).  The child
// creates its own port CP after the fork and publishes the id through a
// MAP_SHARED page.  Then ITERS strict ping-pongs:
//
//     parent: send(CP) → recv(PP)          child: recv(CP) → send(PP)
//
// Each side enters its blocking recv at the same moment the peer's send is
// in flight on another CPU, so every iteration races a sender's
// enqueue+unblock against the receiver's empty-check→Blocked transition —
// the exact lost-wake window.  Pre-fix this hangs within a few thousand
// iterations; post-fix it must complete every time.
//
// SHARED[0..4]  = CP (child's port id, 0 until published)
// SHARED[4..8]  = child result (1 = ok, 2 = error), written before exit
unsafe fn test_ipc_ping_pong() -> bool {
    let name = b"ipc_ping_pong\0";
    const ITERS: usize = 2000;

    let sh = shared_page();
    if sh.is_null() { return report(name, false); }
    let cp_slot     = &*(sh as *const AtomicU32);
    let result_slot = &*(sh.add(4) as *const AtomicU32);
    cp_slot.store(0, Ordering::SeqCst);
    result_slot.store(0, Ordering::SeqCst);

    let pp = port_create();
    if pp <= 0 { return report(name, false); }
    let pp = pp as u32;

    let r = fork();
    if r == 0 {
        // Child: create CP, publish, then recv/send ITERS times.
        let cp = port_create();
        if cp <= 0 { result_slot.store(2, Ordering::SeqCst); _exit(1); }
        let cp = cp as u32;
        cp_slot.store(cp, Ordering::SeqCst);

        let mut msg = Message::empty();
        for i in 0..ITERS {
            if ipc_recv(cp, &mut msg) != 0 { result_slot.store(2, Ordering::SeqCst); _exit(1); }
            let mut reply = Message::empty();
            reply.tag = i as u64;
            loop {
                let s = ipc_send(pp, &reply);
                if s == 0 { break; }
                if s != -11 { result_slot.store(2, Ordering::SeqCst); _exit(1); } // retry only EAGAIN
            }
        }
        result_slot.store(1, Ordering::SeqCst);
        _exit(0);
    }
    if r < 0 { return report(name, false); }

    // Parent: wait for the child's port id, then ping-pong.
    let cp = loop {
        let v = cp_slot.load(Ordering::SeqCst);
        if v != 0 { break v; }
        sleep_ms(10);
    };

    let mut ok = true;
    let mut msg = Message::empty();
    for i in 0..ITERS {
        let mut ping = Message::empty();
        ping.tag = i as u64;
        loop {
            let s = ipc_send(cp, &ping);
            if s == 0 { break; }
            if s != -11 { ok = false; break; }
        }
        if !ok { break; }
        if ipc_recv(pp, &mut msg) != 0 { ok = false; break; }
        if msg.tag != i as u64 { ok = false; break; }
    }

    let mut status: c_int = 0;
    waitpid(r, &mut status, 0);
    let child_ok = result_slot.load(Ordering::SeqCst) == 1;
    munmap(sh as *mut c_void, PAGE);
    report(name, ok && child_ok)
}

// ── 2. EEVDF accounting bounds ───────────────────────────────────────────────

/// Spin in user mode until `ms` milliseconds have elapsed.
unsafe fn busy_spin_ms(ms: i64) {
    let end = now_ms() + ms;
    let mut sink: u64 = 0;
    while now_ms() < end {
        for i in 0..50_000u64 {
            sink = sink.wrapping_add(i).rotate_left(7);
        }
        core::hint::black_box(sink);
    }
}

// 2a. A nanosleep poller keeps bounded latency while 6 CPU-bound children
// (more runnable tasks than the 4 CPUs) saturate the machine.  With the old
// full-tick-per-dispatch minimum charge the poller's vruntime inflated far
// past the eligibility average and its sleeps overran by seconds.
unsafe fn test_eevdf_poller_latency() -> bool {
    let name = b"eevdf_poller_latency\0";
    const NCHILD: usize = 6;
    const BUSY_MS: i64 = 4000;
    const SLEEP_MS: i64 = 100;
    const WORST_ALLOWED_MS: i64 = 1500;

    let mut pids = [0 as pid_t; NCHILD];
    for slot in pids.iter_mut() {
        let r = fork();
        if r == 0 {
            busy_spin_ms(BUSY_MS);
            _exit(0);
        }
        if r < 0 { return report(name, false); }
        *slot = r;
    }

    // Let the children reach their spin loops, then measure.
    sleep_ms(200);
    let mut worst: i64 = 0;
    for _ in 0..15 {
        let t0 = now_ms();
        sleep_ms(SLEEP_MS);
        let dt = now_ms() - t0;
        if dt > worst { worst = dt; }
    }

    let mut status: c_int = 0;
    for pid in pids {
        waitpid(pid, &mut status, 0);
    }

    print_val(b"  worst 100ms-sleep latency (ms): ", worst as u64);
    report(name, worst <= WORST_ALLOWED_MS)
}

// 2b. The opposite bound: 6 polling children (nanosleep loops) must not
// starve a CPU-bound worker.  Guards against "fixing" poller inflation by
// undercharging pollers so far that they pin the eligibility average at the
// bottom and CPU-bound tasks go permanently ineligible.
unsafe fn test_eevdf_cpu_fairness() -> bool {
    let name = b"eevdf_cpu_fairness\0";
    const NPOLLERS: usize = 6;
    const WORK_MS: i64 = 1500;
    const SLOWDOWN_ALLOWED: i64 = 4; // completion may take at most 4× the work

    let sh = shared_page();
    if sh.is_null() { return report(name, false); }
    let stop = &*(sh as *const AtomicU32);
    stop.store(0, Ordering::SeqCst);

    let mut pids = [0 as pid_t; NPOLLERS];
    for slot in pids.iter_mut() {
        let r = fork();
        if r == 0 {
            while stop.load(Ordering::SeqCst) == 0 {
                sleep_ms(20);
            }
            _exit(0);
        }
        if r < 0 { return report(name, false); }
        *slot = r;
    }

    sleep_ms(100);
    let t0 = now_ms();
    busy_spin_ms(WORK_MS);
    let elapsed = now_ms() - t0;

    stop.store(1, Ordering::SeqCst);
    let mut status: c_int = 0;
    for pid in pids {
        waitpid(pid, &mut status, 0);
    }
    munmap(sh as *mut c_void, PAGE);

    print_val(b"  1500ms of CPU work took (ms): ", elapsed as u64);
    report(name, elapsed <= WORK_MS * SLOWDOWN_ALLOWED)
}

// ── 3. Page-fault storms ─────────────────────────────────────────────────────

/// One storm round: map `pages` lazy pages, fault each in with a write,
/// verify the pattern, unmap (which broadcasts a TLB shootdown).
unsafe fn storm_round(pages: usize, seed: u64) -> bool {
    let len = pages * PAGE;
    let p = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE,
                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if p as isize == -1 { return false; }
    let base = p as *mut u8;

    for i in 0..pages {
        let word = seed ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        *(base.add(i * PAGE) as *mut u64) = word;
        *(base.add(i * PAGE + PAGE - 8) as *mut u64) = !word;
    }
    let mut ok = true;
    for i in 0..pages {
        let word = seed ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        if *(base.add(i * PAGE) as *const u64) != word { ok = false; break; }
        if *(base.add(i * PAGE + PAGE - 8) as *const u64) != !word { ok = false; break; }
    }
    munmap(p, len);
    ok
}

// 3a. Five processes (4 children + the parent) run fault storms
// concurrently: every lazy touch is a page fault, every munmap a TLB
// shootdown, all racing each other's scheduling on 4 CPUs.  Pre-fix this
// serialized every fault behind RUN_QUEUE (and convoyed with the shootdown
// waits); the check is that the storm completes correctly and promptly.
unsafe fn test_pf_storm_processes() -> bool {
    let name = b"pf_storm_processes\0";
    const NCHILD: usize = 4;
    const ROUNDS: usize = 6;
    const PAGES: usize = 256; // 1 MiB per round

    let mut pids = [0 as pid_t; NCHILD];
    for (n, slot) in pids.iter_mut().enumerate() {
        let r = fork();
        if r == 0 {
            for round in 0..ROUNDS {
                if !storm_round(PAGES, ((n + 1) * 1000 + round) as u64) {
                    _exit(1);
                }
            }
            _exit(0);
        }
        if r < 0 { return report(name, false); }
        *slot = r;
    }

    let mut ok = true;
    for round in 0..ROUNDS {
        if !storm_round(PAGES, (9000 + round) as u64) { ok = false; }
    }

    for pid in pids {
        let mut status: c_int = 0;
        let waited = waitpid(pid, &mut status, 0);
        if waited != pid || (status & 0x7f) != 0 || ((status >> 8) & 0xff) != 0 {
            ok = false;
        }
    }
    report(name, ok)
}

// 3b. Concurrent faults inside ONE address space: 4 threads each fault in
// and verify their own quarter of a shared 4 MiB lazy mapping.  The fault
// path no longer holds the run-queue lock, so this is the direct test that
// the per-address-space lock actually serializes concurrent
// handle_user_page_fault calls (racing faults would corrupt the VMA's
// lazy_pages bookkeeping and fail verification).
const TSTORM_PAGES_PER_THREAD: usize = 256; // 1 MiB each
static TSTORM_FAILS: AtomicU32 = AtomicU32::new(0);

struct ThreadArg {
    base: *mut u8,
    idx:  usize,
}

extern "C" fn tstorm_thread(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let a = &*(arg as *const ThreadArg);
        let my_base = a.base.add(a.idx * TSTORM_PAGES_PER_THREAD * PAGE);
        let seed = 0xC0FF_EE00u64 + a.idx as u64;

        for i in 0..TSTORM_PAGES_PER_THREAD {
            let word = seed ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            *(my_base.add(i * PAGE) as *mut u64) = word;
        }
        for i in 0..TSTORM_PAGES_PER_THREAD {
            let word = seed ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            if *(my_base.add(i * PAGE) as *const u64) != word {
                TSTORM_FAILS.fetch_add(1, Ordering::SeqCst);
                return core::ptr::null_mut();
            }
        }
    }
    core::ptr::null_mut()
}

unsafe fn test_pf_storm_threads() -> bool {
    let name = b"pf_storm_threads\0";
    const NTHREADS: usize = 4;

    let len = NTHREADS * TSTORM_PAGES_PER_THREAD * PAGE;
    let p = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE,
                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if p as isize == -1 { return report(name, false); }

    TSTORM_FAILS.store(0, Ordering::SeqCst);
    let mut args: [ThreadArg; NTHREADS] = [
        ThreadArg { base: p as *mut u8, idx: 0 },
        ThreadArg { base: p as *mut u8, idx: 1 },
        ThreadArg { base: p as *mut u8, idx: 2 },
        ThreadArg { base: p as *mut u8, idx: 3 },
    ];
    let mut threads: [pthread_t; NTHREADS] = [core::ptr::null_mut(); NTHREADS];

    let mut ok = true;
    for i in 0..NTHREADS {
        if pthread_create(&mut threads[i], core::ptr::null(),
                          tstorm_thread,
                          &mut args[i] as *mut ThreadArg as *mut c_void) != 0 {
            ok = false;
        }
    }
    for t in threads.iter() {
        if !t.is_null() {
            pthread_join(*t, core::ptr::null_mut());
        }
    }
    munmap(p, len);
    report(name, ok && TSTORM_FAILS.load(Ordering::SeqCst) == 0)
}

// ── Entry ────────────────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn race_main(_argc: isize, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let mut failures = 0;

    let mut wd: pthread_t = core::ptr::null_mut();
    pthread_create(&mut wd, core::ptr::null(), watchdog_thread, core::ptr::null_mut());

    if !test_ipc_ping_pong()       { failures += 1; }
    if !test_eevdf_poller_latency() { failures += 1; }
    if !test_eevdf_cpu_fairness()  { failures += 1; }
    if !test_pf_storm_processes()  { failures += 1; }
    if !test_pf_storm_threads()    { failures += 1; }

    DONE.store(1, Ordering::SeqCst);
    puts(b"--- racetest done ---\n\0".as_ptr());
    failures
}
