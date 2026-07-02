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
