//! forktest — regression coverage for the libc-level fork+wait contract on
//! top of the kernel's clone/wait path.
//!
//! The raw kernel clone/fork path (return 0 to the child, inherit the
//! parent's TLS base) is already exercised by memtest. What this suite adds
//! is the contract userspace actually depends on, and which the kernel `wait4`
//! path used to get wrong:
//!
//!   1. `fork()` returns the child pid to the parent and 0 to the child, the
//!      parent can `waitpid()` the child, and the wait status decodes
//!      correctly through `WIFEXITED`/`WEXITSTATUS` — the kernel must return
//!      the reaped pid and encode the status as `(code & 0xff) << 8`, not the
//!      raw exit code;
//!   2. the child's heap allocator is usable after fork (the atfork lock
//!      handoff must not leave it wedged);
//!   3. `pthread_atfork()` prepare/parent/child handlers fire in the right
//!      process at the right time.
//!
//! Parent/child communication uses a `MAP_SHARED` page rather than a pipe: it
//! needs no blocking I/O (the child writes, exits, and the parent reads only
//! after `waitpid` has reaped it — the exit is the synchronisation point), and
//! MAP_SHARED-across-fork is itself proven by memtest. The child is identified
//! by `fork() == 0`, exactly as every POSIX program does (and as memtest
//! does); the child records what it observed in shared memory and the parent
//! verifies it after reaping, so a regression is a failed assertion, never a
//! hang.
//!
//! Initializes via relibc_start_v1 (same as pthreadtest/timertest/sigtest/
//! polltest) so TLS is set up — errno, the allocator, and atfork all need it.
//!
//! Each check prints "<name>: PASS" or "<name>: FAIL" to the serial console;
//! `fork_main` returns the number of failures as the exit code.
//!
//! Status: green on aarch64. On x86_64 it currently FAILS, but not because of
//! anything tested here — it reproduces a pre-existing bug where relibc's
//! `fork()` wrapper returns a corrupted (nonzero) value to the child. The
//! kernel clone path is correct on x86_64 (memtest and a raw `clone(SIGCHLD)`
//! syscall both give the child 0, and the kernel writes rax=0 into the child's
//! frame); the corruption is in relibc's userspace fork() tail and is
//! timing-sensitive. This suite gives that hazard a deterministic repro on
//! x86_64 while locking in correct behaviour on aarch64.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

type c_int = i32;
type pid_t = i32;
type size_t = usize;

const PROT_READ:     c_int = 1;
const PROT_WRITE:    c_int = 2;
const MAP_SHARED:    c_int = 0x01;
const MAP_ANONYMOUS: c_int = 0x20;
const PAGE:          size_t = 4096;

// WEXITSTATUS / WIFEXITED on the musl/Linux wait-status encoding: a normal
// exit is `(code & 0xff) << 8`, and the low 7 bits are a terminating signal
// (0 ⇒ exited normally).
fn wifexited(status: c_int) -> bool { (status & 0x7f) == 0 }
fn wexitstatus(status: c_int) -> c_int { (status >> 8) & 0xff }

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

    pub fn malloc(size: size_t) -> *mut c_void;
    pub fn free(ptr: *mut c_void);

    pub fn read(fd: c_int, buf: *mut c_void, count: size_t) -> isize;
    pub fn pipe(fds: *mut c_int) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn usleep(usec: u32) -> c_int;
    pub fn clock_gettime(clk: c_int, tp: *mut timespec) -> c_int;
    pub fn pthread_create(
        thread: *mut *mut c_void,
        attr: *const c_void,
        start_routine: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> c_int;
    pub fn pthread_join(thread: *mut c_void, retval: *mut *mut c_void) -> c_int;
    pub fn socketpair(domain: c_int, ty: c_int, proto: c_int, sv: *mut c_int) -> c_int;

    pub fn pthread_atfork(
        prepare: Option<extern "C" fn()>,
        parent: Option<extern "C" fn()>,
        child: Option<extern "C" fn()>,
    ) -> c_int;
}

// ── Assembly entry point (identical to polltest's) ───────────────────────────

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset fork_main",
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
    "   adrp x1, fork_main",
    "   add x1, x1, :lo12:fork_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

#[no_mangle]
pub unsafe extern "C" fn fork_main(_argc: isize, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let mut failures = 0;

    if !test_fork_return_and_waitpid() { failures += 1; }
    if !test_child_malloc_after_fork() { failures += 1; }
    if !test_pthread_atfork_hooks_run() { failures += 1; }
    if !test_cow_isolation() { failures += 1; }
    if !test_cow_kernel_write() { failures += 1; }
    if !test_cow_threads_fork() { failures += 1; }
    if !test_cow_socket_fork() { failures += 1; }
    if !test_cow_fork_cost() { failures += 1; }

    puts(b"--- forktest done ---\n\0".as_ptr());
    failures
}

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

/// A one-page MAP_SHARED scratch area both fork halves can see. Returns null
/// on failure.
unsafe fn shared_page() -> *mut u8 {
    let p = mmap(core::ptr::null_mut(), PAGE, PROT_READ | PROT_WRITE,
                 MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if p as isize == -1 { core::ptr::null_mut() } else { p as *mut u8 }
}

// ── 1. fork() return value + waitpid + exit-status encoding ──────────────────
//
// The historical bug was in the kernel's wait path, not fork: `wait4`
// returned 0 instead of the child pid and wrote the raw exit code instead of
// the encoded wait status, so `WIFEXITED`/`WEXITSTATUS` misread every result.
// The child (fork() == 0) records that it ran via shared memory and exits 42;
// the parent then checks the pid it got, the reaped pid, and the decoded
// status.
//
// SHARED[0] = 1 once the child has run
unsafe fn test_fork_return_and_waitpid() -> bool {
    let name = b"fork_return_and_waitpid\0";
    let sh = shared_page();
    if sh.is_null() { return report(name, false); }
    *sh.add(0) = 0;

    let r = fork();
    if r == 0 {
        *sh.add(0) = 1;
        _exit(42);
    }
    if r < 0 { munmap(sh as *mut c_void, PAGE); return report(name, false); }

    let mut status: c_int = 0;
    let waited = waitpid(r, &mut status, 0);

    let child_ran = *sh.add(0) == 1;
    munmap(sh as *mut c_void, PAGE);

    let ok = child_ran                       // child actually executed
        && waited == r                       // waitpid returned the child pid
        && wifexited(status)                 // status decodes as a normal exit
        && wexitstatus(status) == 42;        // exit code propagated intact
    report(name, ok)
}

// ── 2. child heap allocator is usable after fork ─────────────────────────────
//
// fork() runs the allocator's atfork handlers around the raw clone so the
// global malloc lock is released in both processes. If that handoff is wrong
// the child's first malloc() can deadlock or corrupt; here the child performs
// a real allocation and records success in shared memory.
unsafe fn test_child_malloc_after_fork() -> bool {
    let name = b"child_malloc_after_fork\0";
    let sh = shared_page();
    if sh.is_null() { return report(name, false); }
    *sh.add(0) = 0;

    let r = fork();
    if r == 0 {
        let p = malloc(256);
        let ok = if p.is_null() {
            0u8
        } else {
            // Touch every byte so a bogus pointer faults here, not silently.
            core::ptr::write_bytes(p as *mut u8, 0xAB, 256);
            let last = *(p as *const u8).add(255);
            free(p);
            if last == 0xAB { 1u8 } else { 0u8 }
        };
        *sh.add(0) = ok;
        _exit(0);
    }
    if r < 0 { munmap(sh as *mut c_void, PAGE); return report(name, false); }
    let mut status: c_int = 0;
    waitpid(r, &mut status, 0);
    let ok = *sh.add(0) == 1;
    munmap(sh as *mut c_void, PAGE);
    report(name, ok)
}

// ── 3. pthread_atfork prepare/parent/child handlers ──────────────────────────
//
// prepare handlers run in the forking process before the fork (so both the
// parent and, via copy-on-write, the child observe PREPARE==1); the parent
// handler runs only in the parent, the child handler only in the child. The
// child records its own counters in shared memory.
//
// SHARED[0..4] child's PREPARE, [4..8] child's PARENT, [8..12] child's CHILD
static PREPARE: AtomicU32 = AtomicU32::new(0);
static PARENT: AtomicU32 = AtomicU32::new(0);
static CHILD: AtomicU32 = AtomicU32::new(0);

extern "C" fn on_prepare() { PREPARE.fetch_add(1, Ordering::SeqCst); }
extern "C" fn on_parent() { PARENT.fetch_add(1, Ordering::SeqCst); }
extern "C" fn on_child() { CHILD.fetch_add(1, Ordering::SeqCst); }

unsafe fn test_pthread_atfork_hooks_run() -> bool {
    let name = b"pthread_atfork_hooks_run\0";

    if pthread_atfork(Some(on_prepare), Some(on_parent), Some(on_child)) != 0 {
        return report(name, false);
    }

    let sh = shared_page();
    if sh.is_null() { return report(name, false); }
    for i in 0..12 { *sh.add(i) = 0; }

    let r = fork();
    if r == 0 {
        let v = [
            PREPARE.load(Ordering::SeqCst) as i32,
            PARENT.load(Ordering::SeqCst) as i32,
            CHILD.load(Ordering::SeqCst) as i32,
        ];
        for (i, x) in v.iter().enumerate() {
            core::ptr::copy_nonoverlapping(x.to_le_bytes().as_ptr(), sh.add(i * 4), 4);
        }
        _exit(0);
    }
    if r < 0 { munmap(sh as *mut c_void, PAGE); return report(name, false); }
    let mut status: c_int = 0;
    waitpid(r, &mut status, 0);

    let rd = |off: usize| -> i32 {
        let mut b = [0u8; 4];
        core::ptr::copy_nonoverlapping(sh.add(off), b.as_mut_ptr(), 4);
        i32::from_le_bytes(b)
    };
    let (child_prepare, child_parent, child_child) = (rd(0), rd(4), rd(8));

    let parent_prepare = PREPARE.load(Ordering::SeqCst) as i32;
    let parent_parent = PARENT.load(Ordering::SeqCst) as i32;
    let parent_child = CHILD.load(Ordering::SeqCst) as i32;
    munmap(sh as *mut c_void, PAGE);

    let ok =
        // child inherited the prepare that ran in the parent, ran its own
        // child handler, and never ran the parent handler.
        child_prepare == 1 && child_child == 1 && child_parent == 0
        // parent ran prepare and its parent handler, never the child handler.
        && parent_prepare == 1 && parent_parent == 1 && parent_child == 0;
    report(name, ok)
}

// ── 4. Copy-on-write of writable private memory ─────────────────────────────
//
// fork() shares writable private pages copy-on-write instead of copying them
// up front. These cases pin the semantics that must survive that: each side
// sees its own writes and never the other's (both directions), a kernel store
// into a still-shared page (read(2) into a heap buffer, the wait status) lands
// only in the caller's copy, and sibling threads writing through fork do not
// lose stores. The last case prints the fork cost for a large resident heap.

#[repr(C)]
pub struct timespec { pub tv_sec: i64, pub tv_nsec: i64 }
const MAP_PRIVATE: c_int = 0x02;
const CLOCK_MONOTONIC: c_int = 1;

unsafe fn now_us() -> u64 {
    let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
    clock_gettime(CLOCK_MONOTONIC, &mut ts);
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

unsafe fn private_region(len: size_t) -> *mut u8 {
    let p = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE,
                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if p as isize == -1 { core::ptr::null_mut() } else { p as *mut u8 }
}

unsafe fn fill(p: *mut u8, len: size_t, seed: u32) {
    let w = p as *mut u32;
    for i in 0..len / 4 { w.add(i).write_volatile(seed ^ (i as u32).wrapping_mul(2654435761)); }
}

unsafe fn check(p: *const u8, len: size_t, seed: u32) -> bool {
    let w = p as *const u32;
    for i in 0..len / 4 {
        if w.add(i).read_volatile() != seed ^ (i as u32).wrapping_mul(2654435761) { return false; }
    }
    true
}

unsafe fn wait_exit_code(pid: pid_t) -> c_int {
    let mut st: c_int = -1;
    if waitpid(pid, &mut st, 0) != pid || !wifexited(st) { return -1; }
    wexitstatus(st)
}

// SHARED[0]: parent → child "parent has finished writing"
unsafe fn test_cow_isolation() -> bool {
    let name = b"cow_isolation\0";
    const LEN: size_t = 16 << 20;
    let sh = shared_page();
    let p = private_region(LEN);
    if sh.is_null() || p.is_null() { return report(name, false); }
    fill(p, LEN, 0x1111_1111);
    let flag = sh as *mut u32;
    flag.write_volatile(0);
    let pid = fork();
    if pid == 0 {
        // Wait for the parent's post-fork writes, then the original must
        // still be what this child sees; then write our own and re-check.
        let mut spins = 0;
        while flag.read_volatile() == 0 && spins < 5000 { usleep(1000); spins += 1; }
        let mut code = 0;
        if !check(p, LEN, 0x1111_1111) { code |= 1; }
        fill(p, LEN, 0x2222_2222);
        if !check(p, LEN, 0x2222_2222) { code |= 2; }
        _exit(code);
    }
    if pid < 0 { return report(name, false); }
    // Parent writes half the pages while the child still shares them.
    fill(p, LEN / 2, 0x3333_3333);
    flag.write_volatile(1);
    let code = wait_exit_code(pid);
    let ok = code == 0
        && check(p, LEN / 2, 0x3333_3333)
        // the untouched half must be the original, not the child's 0x2222
        && { let q = p.add(LEN / 2) as *const u32;
             let mut good = true;
             for i in 0..(LEN / 2) / 4 {
                 let j = i + (LEN / 2) / 4;
                 if q.add(i).read_volatile() != 0x1111_1111 ^ (j as u32).wrapping_mul(2654435761) { good = false; break; }
             }
             good };
    munmap(p as *mut c_void, LEN);
    munmap(sh as *mut c_void, PAGE);
    report(name, ok)
}

// The kernel writes into user memory through the page tables (read(2), wait
// status, signal frames). Into a page still shared copy-on-write, that store
// must break the sharing first, exactly as a user store would.
unsafe fn test_cow_kernel_write() -> bool {
    let name = b"cow_kernel_write\0";
    const LEN: size_t = 64 * 1024;
    let p = private_region(LEN);
    let buf = malloc(LEN) as *mut u8;
    if p.is_null() || buf.is_null() { return report(name, false); }
    fill(p, LEN, 0x4444_4444);
    fill(buf, LEN, 0x5555_5555);
    let mut fds = [0 as c_int; 2];
    if pipe(fds.as_mut_ptr()) != 0 { return report(name, false); }
    let pid = fork();
    if pid == 0 {
        // Child: read the pipe straight into the shared pages.
        close(fds[1]);
        let mut got = 0usize;
        while got < 8192 {
            let n = read(fds[0], p.add(got) as *mut c_void, 8192 - got);
            if n <= 0 { break; }
            got += n as usize;
        }
        let mut got2 = 0usize;
        while got2 < 4096 {
            let n = read(fds[0], buf.add(got2) as *mut c_void, 4096 - got2);
            if n <= 0 { break; }
            got2 += n as usize;
        }
        let ok = got == 8192 && got2 == 4096 && *p == 0xEE && *p.add(8191) == 0xEE && *buf == 0xEE;
        _exit(if ok { 0 } else { 1 });
    }
    if pid < 0 { return report(name, false); }
    close(fds[0]);
    let junk = [0xEEu8; 4096];
    for _ in 0..3 { write(fds[1], junk.as_ptr() as *const c_void, 4096); }
    close(fds[1]);
    let code = wait_exit_code(pid);
    let ok = code == 0 && check(p, LEN, 0x4444_4444) && check(buf, LEN, 0x5555_5555);
    munmap(p as *mut c_void, LEN);
    free(buf as *mut c_void);
    report(name, ok)
}

// Sibling threads keep storing into private memory while another thread
// forks over and over (the brush/tokio shape). A lost or misdirected store
// shows up as a mismatch between a thread's register-held counter and its
// memory copy.
#[repr(C)]
struct Hammer { stop: AtomicU32, bad: AtomicU32, slots: [u64; 4 * 512] }

extern "C" fn hammer(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let (h, idx) = *(arg as *const (*mut Hammer, usize));
        // each thread owns a distinct page-sized stride inside `slots`
        let slot = (*h).slots.as_mut_ptr().add(idx * 512);
        let mut n: u64 = 0;
        while (*h).stop.load(Ordering::Relaxed) == 0 {
            n += 1;
            slot.write_volatile(n);
            if slot.read_volatile() != n { (*h).bad.fetch_add(1, Ordering::Relaxed); }
            // stack writes too
            let mut local = [0u64; 64];
            for (i, v) in local.iter_mut().enumerate() { core::ptr::write_volatile(v, n + i as u64); }
            for (i, v) in local.iter().enumerate() {
                if core::ptr::read_volatile(v) != n + i as u64 { (*h).bad.fetch_add(1, Ordering::Relaxed); }
            }
        }
    }
    core::ptr::null_mut()
}

unsafe fn test_cow_threads_fork() -> bool {
    let name = b"cow_threads_fork\0";
    let h = private_region(core::mem::size_of::<Hammer>() + PAGE) as *mut Hammer;
    if h.is_null() { return report(name, false); }
    (*h).stop.store(0, Ordering::Relaxed);
    (*h).bad.store(0, Ordering::Relaxed);
    let mut args = [(h, 0usize), (h, 1), (h, 2)];
    let mut th = [core::ptr::null_mut::<c_void>(); 3];
    for i in 0..3 {
        if pthread_create(&mut th[i], core::ptr::null(), hammer, &mut args[i] as *mut _ as *mut c_void) != 0 {
            return report(name, false);
        }
    }
    let mut child_fail = 0;
    for _ in 0..40 {
        let pid = fork();
        if pid == 0 {
            // Child scribbles over every slot; the parent must never see it.
            let s = (*h).slots.as_mut_ptr();
            for i in 0..4 * 512 { s.add(i).write_volatile(0xDEAD_BEEF); }
            _exit(0);
        }
        if pid < 0 || wait_exit_code(pid) != 0 { child_fail += 1; }
    }
    (*h).stop.store(1, Ordering::Relaxed);
    for i in 0..3 { pthread_join(th[i], core::ptr::null_mut()); }
    let s = (*h).slots.as_ptr();
    let mut scribbled = false;
    for i in 0..3 { if s.add(i * 512).read_volatile() == 0xDEAD_BEEF { scribbled = true; } }
    let ok = child_fail == 0 && (*h).bad.load(Ordering::Relaxed) == 0 && !scribbled;
    report(name, ok)
}

// Fork cost with 128 MiB of resident private memory. Not a pass/fail
// threshold — the line is the measurement (plus the kernel's [FORK] line).
unsafe fn test_cow_fork_cost() -> bool {
    let name = b"cow_fork_cost\0";
    const LEN: size_t = 128 << 20;
    let p = private_region(LEN);
    if p.is_null() { return report(name, false); }
    for i in (0..LEN).step_by(PAGE) { p.add(i).write_volatile(i as u8 | 1); }
    let mut total = 0u64;
    let mut worst = 0u64;
    let mut ok = true;
    for _ in 0..5 {
        let t0 = now_us();
        let pid = fork();
        if pid == 0 { _exit(0); }
        let dt = now_us() - t0;
        if pid < 0 || wait_exit_code(pid) != 0 { ok = false; }
        total += dt;
        if dt > worst { worst = dt; }
    }
    let mut line = [0u8; 96];
    let mut n = 0;
    for &b in b"cow_fork_cost: 128MiB resident, fork avg_us=" { line[n] = b; n += 1; }
    n += fmt_u64(&mut line[n..], total / 5);
    for &b in b" worst_us=" { line[n] = b; n += 1; }
    n += fmt_u64(&mut line[n..], worst);
    line[n] = b'\n'; n += 1;
    write(1, line.as_ptr() as *const c_void, n);
    munmap(p as *mut c_void, LEN);
    report(name, ok)
}

fn fmt_u64(out: &mut [u8], mut v: u64) -> usize {
    let mut tmp = [0u8; 20];
    let mut n = 0;
    loop { tmp[n] = b'0' + (v % 10) as u8; n += 1; v /= 10; if v == 0 { break; } }
    for i in 0..n { out[i] = tmp[n - 1 - i]; }
    n
}

// A writer thread streams counters through a unix socketpair (and a pipe)
// into a reader thread's heap buffer while the main thread forks over and
// over — the compositor shape: kernel stores into a recv buffer that fork
// keeps making copy-on-write again. Any lost or misdirected kernel store
// shows up as a gap in the counter sequence.
#[repr(C)]
struct Stream { fd_w: c_int, fd_r: c_int, n: u32, bad: AtomicU32, done: AtomicU32, buf: *mut u32, wbuf: *mut u32 }

extern "C" fn stream_writer(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let st = arg as *mut Stream;
        let w = (*st).wbuf;
        let mut next = 0u32;
        while next < (*st).n {
            for i in 0..16 { w.add(i).write_volatile(next + i as u32); }
            let mut off = 0usize;
            while off < 64 {
                let r = write((*st).fd_w, (w as *const u8).add(off) as *const c_void, 64 - off);
                if r <= 0 { (*st).bad.fetch_add(1000, Ordering::Relaxed); return core::ptr::null_mut(); }
                off += r as usize;
            }
            next += 16;
        }
    }
    core::ptr::null_mut()
}

extern "C" fn stream_reader(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let st = arg as *mut Stream;
        let b = (*st).buf;
        let mut expect = 0u32;
        let mut have = 0usize; // bytes buffered in b
        while expect < (*st).n {
            let r = read((*st).fd_r, (b as *mut u8).add(have) as *mut c_void, 4096 - have);
            if r <= 0 { (*st).bad.fetch_add(1000, Ordering::Relaxed); break; }
            have += r as usize;
            let words = have / 4;
            for i in 0..words {
                let got = b.add(i).read_volatile();
                if got != expect {
                    if (*st).bad.fetch_add(1, Ordering::Relaxed) < 3 {
                        let mut line = [0u8; 128];
                        let mut n = 0;
                        for &c in b"  stream mismatch: word=" { line[n] = c; n += 1; }
                        n += fmt_u64(&mut line[n..], i as u64);
                        for &c in b" expect=" { line[n] = c; n += 1; }
                        n += fmt_u64(&mut line[n..], expect as u64);
                        for &c in b" got=" { line[n] = c; n += 1; }
                        n += fmt_u64(&mut line[n..], got as u64);
                        line[n] = b'\n'; n += 1;
                        write(1, line.as_ptr() as *const c_void, n);
                    }
                }
                expect = expect.wrapping_add(1);
            }
            let rem = have % 4;
            if rem != 0 { core::ptr::copy((b as *const u8).add(words * 4), b as *mut u8, rem); }
            have = rem;
        }
        (*st).done.store(1, Ordering::Relaxed);
    }
    core::ptr::null_mut()
}

unsafe fn run_stream(fd_w: c_int, fd_r: c_int, mode: u32) -> (u32, u32) {
    let st = private_region(PAGE * 4) as *mut Stream;
    let bufs = private_region(PAGE * 4) as *mut u32;
    if st.is_null() || bufs.is_null() { return (1, 0); }
    (*st).fd_w = fd_w; (*st).fd_r = fd_r; (*st).n = 16 * 20000;
    (*st).bad.store(0, Ordering::Relaxed); (*st).done.store(0, Ordering::Relaxed);
    (*st).buf = bufs; (*st).wbuf = bufs.add(2048);
    if mode & 1 != 0 { (*st).wbuf = shared_page() as *mut u32; }
    if mode & 2 != 0 { (*st).buf = mmap(core::ptr::null_mut(), PAGE * 2, PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0) as *mut u32; }
    let mut tw = core::ptr::null_mut::<c_void>();
    let mut tr = core::ptr::null_mut::<c_void>();
    pthread_create(&mut tr, core::ptr::null(), stream_reader, st as *mut c_void);
    pthread_create(&mut tw, core::ptr::null(), stream_writer, st as *mut c_void);
    let mut forks = 0u32;
    while (*st).done.load(Ordering::Relaxed) == 0 && forks < 200 {
        let pid = fork();
        if pid == 0 {
            // touch the shared pages from the child side too
            let b = bufs as *mut u8;
            for i in (0..PAGE * 4).step_by(PAGE) { b.add(i).write_volatile(0x5A); }
            _exit(0);
        }
        if pid > 0 { wait_exit_code(pid); forks += 1; }
    }
    pthread_join(tw, core::ptr::null_mut());
    pthread_join(tr, core::ptr::null_mut());
    ((*st).bad.load(Ordering::Relaxed), forks)
}

unsafe fn test_cow_socket_fork() -> bool {
    let name = b"cow_socket_fork\0";
    let mut sv = [0 as c_int; 2];
    if socketpair(1 /* AF_UNIX */, 1 /* SOCK_STREAM */, 0, sv.as_mut_ptr()) != 0 { return report(name, false); }
    let (bad_s, forks_s) = run_stream(sv[0], sv[1], 0);
    for mode in 1..4u32 {
        let mut sv2 = [0 as c_int; 2];
        socketpair(1, 1, 0, sv2.as_mut_ptr());
        let (b, f) = run_stream(sv2[0], sv2[1], mode);
        let mut line = [0u8; 96];
        let mut n = 0;
        for &c in b"cow_socket_fork: mode=" { line[n] = c; n += 1; }
        n += fmt_u64(&mut line[n..], mode as u64);
        for &c in b" bad=" { line[n] = c; n += 1; }
        n += fmt_u64(&mut line[n..], b as u64);
        for &c in b" forks=" { line[n] = c; n += 1; }
        n += fmt_u64(&mut line[n..], f as u64);
        line[n] = b'\n'; n += 1;
        write(1, line.as_ptr() as *const c_void, n);
    }
    let mut pf = [0 as c_int; 2];
    if pipe(pf.as_mut_ptr()) != 0 { return report(name, false); }
    let (bad_p, forks_p) = run_stream(pf[1], pf[0], 0);
    let mut line = [0u8; 96];
    let mut n = 0;
    for &b in b"cow_socket_fork: sock_bad=" { line[n] = b; n += 1; }
    n += fmt_u64(&mut line[n..], bad_s as u64);
    for &b in b" pipe_bad=" { line[n] = b; n += 1; }
    n += fmt_u64(&mut line[n..], bad_p as u64);
    for &b in b" forks=" { line[n] = b; n += 1; }
    n += fmt_u64(&mut line[n..], (forks_s + forks_p) as u64);
    line[n] = b'\n'; n += 1;
    write(1, line.as_ptr() as *const c_void, n);
    report(name, bad_s == 0 && bad_p == 0)
}
