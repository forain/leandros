//! pthreadtest — standalone regression coverage for TODO.md Phase 4 (Thread Management):
//! pthread_create/join, mutex contention, condvar wait/signal, thread-specific data
//! (TSD) destructors, and cleanup stack execution.
//!
//! Initializes via relibc_start_v1 to set up TLS (tcb / %fs_base / tpidr_el0) properly.
//!
//! Each check prints "<name>: PASS" or "<name>: FAIL" to stdout (serial
//! console); `pthread_main` returns the number of failures as the exit code.

#![no_std]
#![no_main]
#![feature(c_variadic)]
#![allow(non_camel_case_types)]

#[path = "../../muslrt/rt.rs"]
mod rt;
#[allow(unused_imports)]
use rt::syscall;

use core::ffi::c_void;

mod churn;

pub type pthread_t = *mut c_void;
pub type pthread_key_t = u32; // musl: unsigned int

/// musl 64-bit pthread_mutex_t is 40 bytes; over-sized and 8-aligned here.
#[repr(C, align(8))]
pub struct pthread_mutex_t([u64; 8]);

/// musl 64-bit pthread_cond_t is 48 bytes; over-sized and 8-aligned here.
#[repr(C, align(8))]
pub struct pthread_cond_t([u64; 8]);

/// musl `struct __ptcb { void (*__f)(void *); void *__x; struct __ptcb *__next; }`.
#[repr(C)]
pub struct CleanupLinkedListEntry {
    routine: extern "C" fn(*mut c_void),
    arg: *mut c_void,
    prev: *const c_void,
}

extern "C" {

    pub fn puts(s: *const u8) -> i32;
    pub fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    pub fn exit(status: i32) -> !;
    pub fn usleep(usec: u32) -> i32;
    pub fn open(path: *const u8, flags: i32, ...) -> i32;
    pub fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    pub fn close(fd: i32) -> i32;
    pub fn fork() -> i32;
    pub fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;

    pub fn pthread_create(
        thread: *mut pthread_t,
        attr: *const c_void,
        start_routine: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> i32;

    pub fn pthread_join(thread: pthread_t, retval: *mut *mut c_void) -> i32;
    pub fn pthread_detach(thread: pthread_t) -> i32;
    pub fn pthread_cancel(thread: pthread_t) -> i32;

    pub fn pthread_mutex_init(mutex: *mut pthread_mutex_t, attr: *const c_void) -> i32;
    pub fn pthread_mutex_lock(mutex: *mut pthread_mutex_t) -> i32;
    pub fn pthread_mutex_unlock(mutex: *mut pthread_mutex_t) -> i32;
    pub fn pthread_mutex_destroy(mutex: *mut pthread_mutex_t) -> i32;

    pub fn pthread_cond_init(cond: *mut pthread_cond_t, attr: *const c_void) -> i32;
    pub fn pthread_cond_wait(cond: *mut pthread_cond_t, mutex: *mut pthread_mutex_t) -> i32;
    pub fn pthread_cond_signal(cond: *mut pthread_cond_t) -> i32;
    pub fn pthread_cond_destroy(cond: *mut pthread_cond_t) -> i32;

    pub fn pthread_key_create(
        key: *mut pthread_key_t,
        destructor: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> i32;
    pub fn pthread_setspecific(key: pthread_key_t, value: *const c_void) -> i32;
    pub fn pthread_getspecific(key: pthread_key_t) -> *mut c_void;

    pub fn _pthread_cleanup_push(
        cb: *mut CleanupLinkedListEntry,
        f: extern "C" fn(*mut c_void),
        x: *mut c_void,
    );
    pub fn _pthread_cleanup_pop(cb: *mut CleanupLinkedListEntry, execute: i32);
    pub fn pthread_exit(retval: *mut c_void) -> !;
}

macro_rules! pthread_cleanup_push {
    ($entry:ident, $routine:expr, $arg:expr) => {
        let mut $entry = CleanupLinkedListEntry {
            routine: $routine,
            arg: $arg,
            prev: core::ptr::null(),
        };
        _pthread_cleanup_push(core::ptr::from_mut(&mut $entry), $routine, $arg);
    };
}

macro_rules! pthread_cleanup_pop {
    ($entry:ident, $execute:expr) => {
        _pthread_cleanup_pop(core::ptr::from_mut(&mut $entry), $execute);
    };
}

// ── Assembly Entry point ─────────────────────────────────────────────────────



#[no_mangle]
pub unsafe extern "C" fn main(argc_c: i32, argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let argc = argc_c as isize;
    // Mode `pthreadtest exitkill`: regression coverage for "plain libc exit()
    // must kill sibling threads". A sibling parks in a long sleep; the main
    // thread calls libc exit(3). Correct (exit_group) semantics tear the whole
    // process down with status 3 before the sibling wakes, so its forbidden
    // marker never prints. If exit() only reaped the calling thread, the
    // sibling would wake and print SIBLING_ALIVE_AFTER_EXIT, and the process
    // would exit 0 (the sibling's own return) instead of 3.
    if argc >= 2 && arg_eq(argv, 1, b"exitkill") {
        run_exit_kills_siblings();
        // Reached only if exit(3) failed to terminate the process.
        let msg = b"EXITKILL: FAIL (exit returned)\n";
        write(1, msg.as_ptr(), msg.len());
        return 1;
    }

    // Modes for memtest's address-space teardown checks (lane memexact):
    //   `pthreadtest asfree_mt <readyfd>`: three sibling threads park, the
    //     leader writes one byte to `readyfd` and parks (memtest kills it);
    //   `pthreadtest asfree_leader <readyfd> <gofd>`: one worker blocks on
    //     `gofd`; the leader writes a byte to `readyfd` and pthread_exit()s
    //     FIRST; the worker, the last thread, exits once `gofd` delivers.
    if argc >= 3 && arg_eq(argv, 1, b"asfree_mt") {
        let rfd = arg_num(argv, 2);
        let mut t: pthread_t = core::ptr::null_mut();
        for _ in 0..3 {
            pthread_create(&mut t, core::ptr::null(), asfree_parker, core::ptr::null_mut());
        }
        usleep(50_000);
        write(rfd, b"r".as_ptr(), 1);
        loop { usleep(1_000_000); }
    }
    if argc >= 4 && arg_eq(argv, 1, b"asfree_leader") {
        let rfd = arg_num(argv, 2);
        let mut t: pthread_t = core::ptr::null_mut();
        ASFREE_GOFD = arg_num(argv, 3);
        if pthread_create(&mut t, core::ptr::null(), asfree_waiter, core::ptr::null_mut()) != 0 {
            return 1;
        }
        write(rfd, b"r".as_ptr(), 1);
        pthread_exit(core::ptr::null_mut());
    }

    // Mode `pthreadtest churn [N]`: only the thread-churn leak check.
    if argc >= 2 && arg_eq(argv, 1, b"churn") {
        let mut n = 5000u64;
        if argc >= 3 {
            let p = *argv.offset(2);
            let mut v = 0u64; let mut i = 0;
            while (*p.add(i)).is_ascii_digit() { v = v * 10 + (*p.add(i) - b'0') as u64; i += 1; }
            if v > 0 { n = v; }
        }
        return if churn::test_thread_churn_flat(n) { 0 } else { 1 };
    }

    let mut failures = 0;

    if !test_pthread_create_join() { failures += 1; }
    if !test_pthread_mutex() { failures += 1; }
    if !test_pthread_condvar() { failures += 1; }
    if !test_pthread_tsd() { failures += 1; }
    if !test_pthread_cleanup() { failures += 1; }
    if !test_thread_getpid_is_process() { failures += 1; }
    if !test_getrandom_distinct() { failures += 1; }
    if !test_getrandom_quality() { failures += 1; }
    if !test_futex_pi_basic() { failures += 1; }
    if !test_futex_pi_contended() { failures += 1; }
    if !test_futex_pi_timeout() { failures += 1; }
    #[cfg(target_arch = "x86_64")]
    if !test_arch_gs_base() { failures += 1; }

    puts(b"--- pthreadtest done ---\n\0".as_ptr());
    failures
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}

// ── 1. Create and Join ───────────────────────────────────────────────────────

extern "C" fn create_join_shim(arg: *mut c_void) -> *mut c_void {
    arg
}

unsafe fn test_pthread_create_join() -> bool {
    let name = b"pthread_create_join\0";
    let mut thread: pthread_t = core::ptr::null_mut();
    let magic = 0x12345678 as *mut c_void;

    let r = pthread_create(&mut thread, core::ptr::null(), create_join_shim, magic);
    if r != 0 { return report(name, false); }

    let mut retval: *mut c_void = core::ptr::null_mut();
    let r2 = pthread_join(thread, &mut retval);
    if r2 != 0 { return report(name, false); }

    report(name, retval == magic)
}

// ── 1b. getpid() from a thread names the process ────────────────────────────
//
// getpid(2)/getppid(2) are per-process: every thread of a process gets the
// same answers, and only gettid(2) differs. The kernel used to return the
// calling thread's id from getpid, which made Firefox's IPC layer abort
// (`MOZ_RELEASE_ASSERT(mMyProcInfo == ... EndpointProcInfo::Current())`).

#[cfg(target_arch = "x86_64")]
mod ids { pub const GETPID: i64 = 39; pub const GETPPID: i64 = 110; pub const GETTID: i64 = 186; }
#[cfg(target_arch = "aarch64")]
mod ids { pub const GETPID: i64 = 172; pub const GETPPID: i64 = 173; pub const GETTID: i64 = 178; }

static mut THREAD_IDS: [i64; 3] = [0; 3];

extern "C" fn ids_worker(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        THREAD_IDS = [syscall(ids::GETPID), syscall(ids::GETPPID), syscall(ids::GETTID)];
    }
    core::ptr::null_mut()
}

unsafe fn test_thread_getpid_is_process() -> bool {
    let name = b"thread_getpid_is_process\0";
    let (pid, ppid, tid) = (syscall(ids::GETPID), syscall(ids::GETPPID), syscall(ids::GETTID));
    let mut thread: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut thread, core::ptr::null(), ids_worker, core::ptr::null_mut()) != 0 {
        return report(name, false);
    }
    let mut rv: *mut c_void = core::ptr::null_mut();
    if pthread_join(thread, &mut rv) != 0 { return report(name, false); }
    let [tpid, tppid, ttid] = THREAD_IDS;
    report(name, pid == tid && tpid == pid && tppid == ppid && ttid != tid && ttid > 0)
}

// ── 1d. arch_prctl(ARCH_SET_GS) is a per-thread register that survives ──────
//
// wasm2c's "segue" sandboxes (Firefox's RLBox libraries on x86-64) keep their
// memory base in GS: they set it with arch_prctl(ARCH_SET_GS) and read memory
// through %gs. The kernel answered EINVAL and zeroed GS.base on every return
// to user mode, so Firefox aborted at startup. Check: the value reads back,
// %gs-relative loads see it across sleeps (context switches), a second thread
// keeps its own, and a forked child inherits it.

#[cfg(target_arch = "x86_64")]
unsafe fn gs_read_u64() -> u64 {
    let v: u64;
    core::arch::asm!("mov {}, qword ptr gs:[0]", out(reg) v, options(nostack, readonly));
    v
}

#[cfg(target_arch = "x86_64")]
static mut GS_CELL_MAIN: u64 = 0x1111_2222_3333_4444;
#[cfg(target_arch = "x86_64")]
static mut GS_CELL_WORKER: u64 = 0x5555_6666_7777_8888;
#[cfg(target_arch = "x86_64")]
static mut GS_WORKER_SAW: [u64; 2] = [0; 2];

#[cfg(target_arch = "x86_64")]
const ARCH_SET_GS: i64 = 0x1001;
#[cfg(target_arch = "x86_64")]
const ARCH_GET_GS: i64 = 0x1004;
#[cfg(target_arch = "x86_64")]
const SYS_ARCH_PRCTL: i64 = 158;

#[cfg(target_arch = "x86_64")]
extern "C" fn gs_worker(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        let r = syscall(SYS_ARCH_PRCTL, ARCH_SET_GS, core::ptr::addr_of!(GS_CELL_WORKER) as u64);
        usleep(20_000);
        GS_WORKER_SAW = [r as u64, gs_read_u64()];
    }
    core::ptr::null_mut()
}

#[cfg(target_arch = "x86_64")]
unsafe fn test_arch_gs_base() -> bool {
    let name = b"arch_gs_base\0";
    let cell = core::ptr::addr_of!(GS_CELL_MAIN) as u64;
    if syscall(SYS_ARCH_PRCTL, ARCH_SET_GS, cell) != 0 { return report(name, false); }
    let mut got: u64 = 0;
    let get_ok = syscall(SYS_ARCH_PRCTL, ARCH_GET_GS, &mut got as *mut u64) == 0 && got == cell;
    let mut thread: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut thread, core::ptr::null(), gs_worker, core::ptr::null_mut()) != 0 {
        return report(name, false);
    }
    let mut seen_ok = true;
    for _ in 0..10 {
        usleep(5_000);
        if gs_read_u64() != GS_CELL_MAIN { seen_ok = false; }
    }
    let mut rv: *mut c_void = core::ptr::null_mut();
    pthread_join(thread, &mut rv);
    let worker_ok = GS_WORKER_SAW == [0, GS_CELL_WORKER];
    let main_after = gs_read_u64() == GS_CELL_MAIN;
    // fork: the child inherits GS.base.
    let child = fork();
    if child == 0 {
        usleep(5_000);
        exit(if gs_read_u64() == GS_CELL_MAIN { 0 } else { 1 });
    }
    let mut st: i32 = -1;
    waitpid(child, &mut st, 0);
    let fork_ok = st == 0;
    syscall(SYS_ARCH_PRCTL, ARCH_SET_GS, 0u64);
    let ok = get_ok && seen_ok && worker_ok && main_after && fork_ok;
    if !ok {
        let msg = b"[arch_gs_base] get/seen/worker/after/fork mismatch\n";
        write(1, msg.as_ptr(), msg.len());
    }
    report(name, ok)
}

// ── 1c. getrandom() never repeats back to back ──────────────────────────────
//
// getrandom(2) used to reseed from the 100 Hz tick on every call, so calls in
// the same tick returned identical bytes — Firefox drew two equal 128-bit IPC
// port names from it (`ERROR_PORT_EXISTS`). Draw 64 values in a tight loop,
// from this thread and a second one at once, and require them all distinct.

#[cfg(target_arch = "x86_64")]
const SYS_GETRANDOM: i64 = 318;
#[cfg(target_arch = "aarch64")]
const SYS_GETRANDOM: i64 = 278;

static mut RAND_WORKER: [u64; 32] = [0; 32];

extern "C" fn rand_worker(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        for i in 0..32 {
            let mut v: u64 = 0;
            syscall(SYS_GETRANDOM, &mut v as *mut u64, 8usize, 0usize);
            RAND_WORKER[i] = v;
        }
    }
    core::ptr::null_mut()
}

unsafe fn test_getrandom_distinct() -> bool {
    let name = b"getrandom_distinct\0";
    let mut thread: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut thread, core::ptr::null(), rand_worker, core::ptr::null_mut()) != 0 {
        return report(name, false);
    }
    let mut mine = [0u64; 32];
    let mut short = false;
    for v in mine.iter_mut() {
        if syscall(SYS_GETRANDOM, v as *mut u64, 8usize, 0usize) != 8 { short = true; }
    }
    let mut rv: *mut c_void = core::ptr::null_mut();
    if pthread_join(thread, &mut rv) != 0 { return report(name, false); }
    let mut all = [0u64; 64];
    all[..32].copy_from_slice(&mine);
    all[32..].copy_from_slice(&*core::ptr::addr_of!(RAND_WORKER));
    let mut distinct = true;
    for i in 0..64 {
        for j in i + 1..64 {
            if all[i] == all[j] { distinct = false; }
        }
    }
    report(name, !short && distinct)
}

// ── 1d. getrandom() / /dev/urandom output quality and flags ─────────────────
//
// A sanity net under the kernel CSPRNG (sched::random), not a proof:
//   * byte frequencies over 1 MiB pass a chi-square test (255 dof: mean 255,
//     sd ~22.6; accepted 170..350, which also rejects output that is TOO
//     even, e.g. a counter);
//   * 8192 successive 64-bit draws contain no repeat (no short cycle);
//   * GRND_NONBLOCK and GRND_RANDOM return full reads, an unknown flag and
//     GRND_INSECURE|GRND_RANDOM are rejected;
//   * /dev/urandom and /dev/random return bytes that differ from each other
//     and from the last getrandom draw.

static mut RAND_MIB: [u8; 1 << 20] = [0; 1 << 20];
static mut RAND_WORDS: [u64; 8192] = [0; 8192];

unsafe fn chi_square_x4096(buf: &[u8]) -> u64 {
    let mut counts = [0u64; 256];
    for &b in buf { counts[b as usize] += 1; }
    let expect = (buf.len() / 256) as i64;
    counts.iter().map(|&c| { let d = c as i64 - expect; (d * d) as u64 }).sum()
}

unsafe fn test_getrandom_quality() -> bool {
    let name = b"getrandom_quality\0";
    let mib = &mut *core::ptr::addr_of_mut!(RAND_MIB);
    let mut got = 0usize;
    while got < mib.len() {
        let r = syscall(SYS_GETRANDOM, mib.as_mut_ptr().add(got), mib.len() - got, 0usize);
        if r <= 0 { return report(name, false); }
        got += r as usize;
    }
    // sum((c - 4096)^2) / 4096 is the chi-square statistic for 1 MiB.
    let chi_x = chi_square_x4096(mib);
    let chi_ok = chi_x >= 170 * 4096 && chi_x <= 350 * 4096;

    let words = &mut *core::ptr::addr_of_mut!(RAND_WORDS);
    for w in words.iter_mut() {
        if syscall(SYS_GETRANDOM, w as *mut u64, 8usize, 0usize) != 8 { return report(name, false); }
    }
    let last = words[8191];
    words.sort_unstable();
    let no_repeat = words.windows(2).all(|p| p[0] != p[1]);

    let mut b = [0u8; 16];
    let nb = syscall(SYS_GETRANDOM, b.as_mut_ptr(), 16usize, 1usize) == 16;   // GRND_NONBLOCK
    let rnd = syscall(SYS_GETRANDOM, b.as_mut_ptr(), 16usize, 2usize) == 16;  // GRND_RANDOM
    let bad = syscall(SYS_GETRANDOM, b.as_mut_ptr(), 16usize, 0x40usize) < 0;
    let bad2 = syscall(SYS_GETRANDOM, b.as_mut_ptr(), 16usize, 6usize) < 0;   // INSECURE|RANDOM

    let mut u = [0u8; 4096];
    let mut r = [0u8; 4096];
    let fu = open(b"/dev/urandom\0".as_ptr(), 0);
    let fr = open(b"/dev/random\0".as_ptr(), 0);
    let nu = if fu >= 0 { read(fu, u.as_mut_ptr(), u.len()) } else { -1 };
    let nr = if fr >= 0 { read(fr, r.as_mut_ptr(), r.len()) } else { -1 };
    if fu >= 0 { close(fu); }
    if fr >= 0 { close(fr); }
    let dev_ok = nu == 4096 && nr == 4096 && u != r
        && u[..8] != last.to_le_bytes() && u.iter().any(|&x| x != 0);
    // 4 KiB is too little for a tight chi-square; allow a wide band.
    let mut dev_counts = [0u32; 256];
    for &x in u.iter() { dev_counts[x as usize] += 1; }
    let dev_spread = dev_counts.iter().filter(|&&c| c > 0).count() > 200;

    let say = |label: &[u8], v: u64| {
        write(1, label.as_ptr(), label.len());
        let mut buf = [0u8; 20]; let mut n = 0; let mut x = v;
        if x == 0 { buf[0] = b'0'; n = 1; }
        while x > 0 { buf[n] = b'0' + (x % 10) as u8; x /= 10; n += 1; }
        let mut o = [0u8; 20]; for i in 0..n { o[i] = buf[n - 1 - i]; }
        write(1, o.as_ptr(), n);
    };
    say(b"  getrandom chi2(1MiB)=", chi_x / 4096);
    say(b" no_repeat=", no_repeat as u64);
    say(b" flags nb/rnd/bad/bad2=", ((nb as u64) << 3) | ((rnd as u64) << 2) | ((bad as u64) << 1) | bad2 as u64);
    say(b" dev u/r=", ((nu.max(0) as u64) << 16) | nr.max(0) as u64);
    write(1, b"\n".as_ptr(), 1);
    report(name, chi_ok && no_repeat && nb && rnd && bad && bad2 && dev_ok && dev_spread)
}

// ── 2. Mutex Contention ─────────────────────────────────────────────────────

static mut MUTEX_SHARED_COUNTER: i32 = 0;
static mut MUTEX_LOCK: pthread_mutex_t = pthread_mutex_t([0; 8]);

extern "C" fn mutex_worker(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        for _ in 0..2000 {
            pthread_mutex_lock(&raw mut MUTEX_LOCK);
            MUTEX_SHARED_COUNTER += 1;
            pthread_mutex_unlock(&raw mut MUTEX_LOCK);
        }
    }
    core::ptr::null_mut()
}

unsafe fn test_pthread_mutex() -> bool {
    let name = b"pthread_mutex\0";
    MUTEX_SHARED_COUNTER = 0;

    let r = pthread_mutex_init(&raw mut MUTEX_LOCK, core::ptr::null());
    if r != 0 { return report(name, false); }

    let mut t1: pthread_t = core::ptr::null_mut();
    let mut t2: pthread_t = core::ptr::null_mut();

    let r1 = pthread_create(&mut t1, core::ptr::null(), mutex_worker, core::ptr::null_mut());
    let r2 = pthread_create(&mut t2, core::ptr::null(), mutex_worker, core::ptr::null_mut());

    if r1 != 0 || r2 != 0 {
        pthread_mutex_destroy(&raw mut MUTEX_LOCK);
        return report(name, false);
    }

    pthread_join(t1, core::ptr::null_mut());
    pthread_join(t2, core::ptr::null_mut());

    let final_val = MUTEX_SHARED_COUNTER;
    pthread_mutex_destroy(&raw mut MUTEX_LOCK);

    report(name, final_val == 4000)
}

// ── 3. Condvar Wait/Signal ──────────────────────────────────────────────────

static mut COND_MUTEX: pthread_mutex_t = pthread_mutex_t([0; 8]);
static mut COND_VAR: pthread_cond_t = pthread_cond_t([0; 8]);
static mut COND_READY: i32 = 0;

extern "C" fn cond_worker(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        pthread_mutex_lock(&raw mut COND_MUTEX);
        COND_READY = 1;
        pthread_cond_signal(&raw mut COND_VAR);
        pthread_mutex_unlock(&raw mut COND_MUTEX);
    }
    core::ptr::null_mut()
}

unsafe fn test_pthread_condvar() -> bool {
    let name = b"pthread_condvar\0";
    COND_READY = 0;

    pthread_mutex_init(&raw mut COND_MUTEX, core::ptr::null());
    pthread_cond_init(&raw mut COND_VAR, core::ptr::null());

    let mut t: pthread_t = core::ptr::null_mut();
    let r = pthread_create(&mut t, core::ptr::null(), cond_worker, core::ptr::null_mut());
    if r != 0 {
        pthread_mutex_destroy(&raw mut COND_MUTEX);
        pthread_cond_destroy(&raw mut COND_VAR);
        return report(name, false);
    }

    pthread_mutex_lock(&raw mut COND_MUTEX);
    while COND_READY == 0 {
        pthread_cond_wait(&raw mut COND_VAR, &raw mut COND_MUTEX);
    }
    pthread_mutex_unlock(&raw mut COND_MUTEX);

    pthread_join(t, core::ptr::null_mut());

    pthread_mutex_destroy(&raw mut COND_MUTEX);
    pthread_cond_destroy(&raw mut COND_VAR);

    report(name, true)
}

// ── 4. Thread-Specific Data (TSD) ──────────────────────────────────────────

static mut TSD_DESTRUCTOR_RUNS: i32 = 0;

unsafe extern "C" fn tsd_destructor(arg: *mut c_void) {
    if arg == 0xBAADF00D as *mut c_void {
        TSD_DESTRUCTOR_RUNS += 1;
    }
}

unsafe fn test_pthread_tsd() -> bool {
    let name = b"pthread_tsd\0";
    TSD_DESTRUCTOR_RUNS = 0;

    let mut key: pthread_key_t = 0;
    let r = pthread_key_create(&mut key, Some(tsd_destructor));
    if r != 0 { return report(name, false); }

    extern "C" fn tsd_worker(arg: *mut c_void) -> *mut c_void {
        let k = unsafe { *(arg as *mut pthread_key_t) };
        unsafe {
            pthread_setspecific(k, 0xBAADF00D as *mut c_void);
        }
        core::ptr::null_mut()
    }

    let mut t: pthread_t = core::ptr::null_mut();
    let r2 = pthread_create(&mut t, core::ptr::null(), tsd_worker, &mut key as *mut _ as *mut c_void);
    if r2 != 0 { return report(name, false); }

    pthread_join(t, core::ptr::null_mut());

    report(name, TSD_DESTRUCTOR_RUNS == 1)
}

// ── 5. Cleanup Handlers ─────────────────────────────────────────────────────

static mut CLEANUP_RUNS: i32 = 0;

extern "C" fn cleanup_routine(arg: *mut c_void) {
    let val = arg as usize;
    unsafe {
        CLEANUP_RUNS += val as i32;
    }
}

extern "C" fn cleanup_worker(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        pthread_cleanup_push!(entry, cleanup_routine, 10 as *mut c_void);
        pthread_cleanup_pop!(entry, 1);

        pthread_cleanup_push!(entry2, cleanup_routine, 100 as *mut c_void);
        pthread_exit(core::ptr::null_mut());
    }
}

unsafe fn test_pthread_cleanup() -> bool {
    let name = b"pthread_cleanup\0";
    CLEANUP_RUNS = 0;

    let mut t: pthread_t = core::ptr::null_mut();
    let r = pthread_create(&mut t, core::ptr::null(), cleanup_worker, core::ptr::null_mut());
    if r != 0 { return report(name, false); }

    pthread_join(t, core::ptr::null_mut());

    report(name, CLEANUP_RUNS == 110)
}

// ── Helper ──────────────────────────────────────────────────────────────────

// ── PI futexes (FUTEX_LOCK_PI / UNLOCK_PI / TRYLOCK_PI) ──────────────────────
//
// musl's pthread_mutexattr_setprotocol(PTHREAD_PRIO_INHERIT) probes
// FUTEX_LOCK_PI on a zero word and fails the call on any error; libpulse's
// pa_mutex_new asserts on that and aborted Firefox's audio thread (the kernel
// answered ENOSYS). These drive the syscall directly with musl's protocol:
// user-space cas(0 -> tid) / cas(tid -> 0), the kernel only on contention.

#[cfg(target_arch = "x86_64")]
mod fx { pub const FUTEX: i64 = 202; pub const CLOCK_GETTIME: i64 = 228; }
#[cfg(target_arch = "aarch64")]
mod fx { pub const FUTEX: i64 = 98; pub const CLOCK_GETTIME: i64 = 113; }
const FUTEX_LOCK_PI: i64 = 6;
const FUTEX_UNLOCK_PI: i64 = 7;
const FUTEX_TRYLOCK_PI: i64 = 8;
const FUTEX_PRIVATE: i64 = 128;
const PI_WAITERS: u32 = 0x8000_0000;

use core::sync::atomic::{AtomicU32, AtomicI32, Ordering::SeqCst};

unsafe fn pi(word: &AtomicU32, op: i64, ts: *const [i64; 2]) -> i64 {
    syscall(fx::FUTEX, word as *const AtomicU32, op | FUTEX_PRIVATE, 0usize, ts, 0usize, 0usize)
}

unsafe fn say(msg: &[u8]) { write(1, msg.as_ptr(), msg.len()); }

unsafe fn test_futex_pi_basic() -> bool {
    let name = b"futex_pi_basic\0";
    let tid = syscall(ids::GETTID) as u32;
    let w = AtomicU32::new(0);
    // musl's probe: LOCK_PI on a free word takes it.
    let lock = pi(&w, FUTEX_LOCK_PI, core::ptr::null());
    let owned = w.load(SeqCst) == tid;
    let relock = pi(&w, FUTEX_LOCK_PI, core::ptr::null());   // EDEADLK
    let trylock = pi(&w, FUTEX_TRYLOCK_PI, core::ptr::null()); // EDEADLK
    let unlock = pi(&w, FUTEX_UNLOCK_PI, core::ptr::null());
    let freed = w.load(SeqCst) == 0;
    let unlock2 = pi(&w, FUTEX_UNLOCK_PI, core::ptr::null()); // EPERM: not ours
    let try2 = pi(&w, FUTEX_TRYLOCK_PI, core::ptr::null());
    let owned2 = w.load(SeqCst) == tid;
    let unlock3 = pi(&w, FUTEX_UNLOCK_PI, core::ptr::null());
    // Held by a TID that names no task: ESRCH.
    let dead = AtomicU32::new(0x3fff_fff0);
    let esrch = pi(&dead, FUTEX_LOCK_PI, core::ptr::null());
    let ok = lock == 0 && owned && relock == -35 && trylock == -35 && unlock == 0 && freed
        && unlock2 == -1 && try2 == 0 && owned2 && unlock3 == 0 && esrch == -3;
    if !ok { say(b"[futex_pi_basic] lock/owned/relock/try/unlock/freed/eperm/try2/esrch mismatch\n"); }
    report(name, ok)
}

static PI_WORD: AtomicU32 = AtomicU32::new(0);
static mut PI_COUNTER: u32 = 0;
static PI_KERNEL_LOCKS: AtomicI32 = AtomicI32::new(0);
static PI_ERRORS: AtomicI32 = AtomicI32::new(0);
/// First error seen, for the failure line: kind (1 lock errno, 2 lock word not ours, 3 unlock errno) << 32 | detail.
static PI_FIRST_ERR: core::sync::atomic::AtomicI64 = core::sync::atomic::AtomicI64::new(0);

fn pi_err(kind: i64, detail: i64, word: u32) {
    PI_ERRORS.fetch_add(1, SeqCst);
    let _ = PI_FIRST_ERR.compare_exchange(0, (kind << 56) | ((detail & 0xffff) << 32) | word as i64, SeqCst, SeqCst);
}

unsafe fn pi_lock(tid: u32) {
    if PI_WORD.compare_exchange(0, tid, SeqCst, SeqCst).is_ok() { return; }
    PI_KERNEL_LOCKS.fetch_add(1, SeqCst);
    loop {
        let r = pi(&PI_WORD, FUTEX_LOCK_PI, core::ptr::null());
        if r == 0 { break; }
        if r != -4 { pi_err(1, r, PI_WORD.load(SeqCst)); break; }
    }
    let wv = PI_WORD.load(SeqCst);
    if wv & 0x3fff_ffff != tid { pi_err(2, 0, wv); }
}

unsafe fn pi_unlock(tid: u32) {
    if PI_WORD.compare_exchange(tid, 0, SeqCst, SeqCst).is_ok() { return; }
    let r = pi(&PI_WORD, FUTEX_UNLOCK_PI, core::ptr::null());
    if r != 0 { pi_err(3, r, PI_WORD.load(SeqCst)); }
}

extern "C" fn pi_worker(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        let tid = syscall(ids::GETTID) as u32;
        for i in 0..3000u32 {
            pi_lock(tid);
            let v = PI_COUNTER;
            // Widen the critical section now and then so lockers really queue.
            if i % 64 == 0 { usleep(200); }
            PI_COUNTER = v + 1;
            pi_unlock(tid);
        }
    }
    core::ptr::null_mut()
}

unsafe fn test_futex_pi_contended() -> bool {
    let name = b"futex_pi_contended\0";
    PI_WORD.store(0, SeqCst);
    PI_COUNTER = 0;
    PI_KERNEL_LOCKS.store(0, SeqCst);
    PI_ERRORS.store(0, SeqCst);
    PI_FIRST_ERR.store(0, SeqCst);
    let mut t = [core::ptr::null_mut::<c_void>(); 4];
    for th in t.iter_mut() {
        if pthread_create(th, core::ptr::null(), pi_worker, core::ptr::null_mut()) != 0 {
            return report(name, false);
        }
    }
    for th in t.iter() { pthread_join(*th, core::ptr::null_mut()); }
    let ok = PI_COUNTER == 12000 && PI_ERRORS.load(SeqCst) == 0 && PI_WORD.load(SeqCst) == 0
        && PI_KERNEL_LOCKS.load(SeqCst) > 0;
    if !ok {
        say(b"[futex_pi_contended] counter/errors/final word/kernel-path mismatch\n");
        let f = PI_FIRST_ERR.load(SeqCst);
        use core::fmt::Write;
        struct B([u8; 200], usize);
        impl Write for B { fn write_str(&mut self, t: &str) -> core::fmt::Result {
            let n = t.len().min(self.0.len() - self.1); self.0[self.1..self.1 + n].copy_from_slice(&t.as_bytes()[..n]); self.1 += n; Ok(()) } }
        let mut b = B([0; 200], 0);
        let _ = write!(b, "[futex_pi_contended] counter={} errors={} final={:#x} kernel_locks={} first_err kind={} detail={} word={:#x}\n",
            PI_COUNTER, PI_ERRORS.load(SeqCst), PI_WORD.load(SeqCst), PI_KERNEL_LOCKS.load(SeqCst), f >> 56, ((f >> 32) & 0xffff) as i16, f as u32);
        say(&b.0[..b.1]);
    }
    report(name, ok)
}

static PI_T_WORD: AtomicU32 = AtomicU32::new(0);
static PI_T_RESULT: AtomicI32 = AtomicI32::new(1);

extern "C" fn pi_timeout_worker(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        // LOCK_PI's timeout: absolute CLOCK_REALTIME, here now + 50 ms.
        let mut ts = [0i64; 2];
        syscall(fx::CLOCK_GETTIME, 0usize, &mut ts as *mut [i64; 2]);
        ts[1] += 50_000_000;
        if ts[1] >= 1_000_000_000 { ts[0] += 1; ts[1] -= 1_000_000_000; }
        PI_T_RESULT.store(pi(&PI_T_WORD, FUTEX_LOCK_PI, &ts) as i32, SeqCst);
    }
    core::ptr::null_mut()
}

unsafe fn test_futex_pi_timeout() -> bool {
    let name = b"futex_pi_timeout\0";
    let tid = syscall(ids::GETTID) as u32;
    PI_T_WORD.store(tid, SeqCst); // held by this thread for the whole wait
    let mut th: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut th, core::ptr::null(), pi_timeout_worker, core::ptr::null_mut()) != 0 {
        return report(name, false);
    }
    pthread_join(th, core::ptr::null_mut());
    // The waiter must have left FUTEX_WAITERS behind, and the unlock goes
    // through the kernel and releases the word.
    let w = PI_T_WORD.load(SeqCst);
    let unlock = pi(&PI_T_WORD, FUTEX_UNLOCK_PI, core::ptr::null());
    let ok = PI_T_RESULT.load(SeqCst) == -110 && w == tid | PI_WAITERS && unlock == 0
        && PI_T_WORD.load(SeqCst) == 0;
    if !ok { say(b"[futex_pi_timeout] result/waiters bit/unlock mismatch\n"); }
    report(name, ok)
}

unsafe fn report(name: &[u8], passed: bool) -> bool {
    // One write(): a kernel console line must not splice into the verdict.
    let mut vline = [0u8; 160];
    let vn = (name.len() - 1).min(140);
    vline[..vn].copy_from_slice(&name[..vn]);
    let vsuf: &[u8] = if passed { b": PASS\n" } else { b": FAIL\n" };
    vline[vn..vn + 7].copy_from_slice(vsuf);
    write(1, vline.as_ptr(), vn + 7);
    passed
}

// ── exit()-kills-siblings regression (mode: `pthreadtest exitkill`) ──────────

unsafe fn arg_num(argv: *mut *mut u8, idx: isize) -> i32 {
    let p = *argv.offset(idx);
    let mut v = 0i32;
    let mut i = 0usize;
    while *p.add(i) >= b'0' && *p.add(i) <= b'9' { v = v * 10 + (*p.add(i) - b'0') as i32; i += 1; }
    v
}

extern "C" fn asfree_parker(_a: *mut c_void) -> *mut c_void {
    unsafe { loop { usleep(1_000_000); } }
}

static mut ASFREE_GOFD: i32 = -1;

/// Blocks until one byte arrives on `ASFREE_GOFD`, then ends the last thread.
extern "C" fn asfree_waiter(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        let mut b = [0u8; 1];
        read(ASFREE_GOFD, b.as_mut_ptr(), 1);
    }
    core::ptr::null_mut()
}

/// True if `argv[idx]` is exactly the NUL-terminated string `want` (no NUL).
unsafe fn arg_eq(argv: *mut *mut u8, idx: isize, want: &[u8]) -> bool {
    let p = *argv.offset(idx);
    if p.is_null() {
        return false;
    }
    let mut i = 0usize;
    loop {
        let c = *p.add(i);
        if i == want.len() {
            return c == 0; // matched all of `want`; arg must end here
        }
        if c == 0 || c != want[i] {
            return false;
        }
        i += 1;
    }
}

/// Sibling thread: park past the moment the main thread calls `exit(3)`. If the
/// libc `exit()` reaped only its own thread (the bug), this wakes and prints the
/// forbidden marker; correct exit_group semantics kill it first, so the line
/// must never appear on the console. `write(2)` is used deliberately (not
/// buffered stdio) so a genuine survival cannot be hidden in an unflushed
/// buffer and produce a false PASS.
extern "C" fn exitkill_sibling(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        usleep(500_000);
        let msg = b"EXITKILL: SIBLING_ALIVE_AFTER_EXIT\n";
        write(1, msg.as_ptr(), msg.len());
    }
    core::ptr::null_mut()
}

unsafe fn run_exit_kills_siblings() {
    let mut t: pthread_t = core::ptr::null_mut();
    let r = pthread_create(&mut t, core::ptr::null(), exitkill_sibling, core::ptr::null_mut());
    if r != 0 {
        let msg = b"EXITKILL: FAIL (pthread_create)\n";
        write(1, msg.as_ptr(), msg.len());
        return;
    }

    // Give the sibling time to reach its usleep park before we exit. Absolute
    // timing does not matter: the sibling is a live thread-group member the
    // instant pthread_create returns, so exit_group reaps it whether it has
    // parked, is still starting, or has not yet been scheduled.
    usleep(100_000);

    let msg = b"EXITKILL: main calling exit(3)\n";
    write(1, msg.as_ptr(), msg.len());
    exit(3);
}
