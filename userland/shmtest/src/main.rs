//! shmtest — shared memory across fork() (lane shmfork).
//!
//! `mmap(MAP_SHARED | MAP_ANONYMOUS)` (and `MAP_SHARED` of /dev/zero) is ONE
//! memory for every process that inherits it: a page either side touches for
//! the first time *after* the fork must still be the page the other side
//! sees. Until lane shmfork each address space demand-paged its own zero page
//! for such a page, so the mapping was shared only for pages touched before
//! the fork. Every case below therefore leaves the pages it checks
//! UNTOUCHED until after the fork.
//!
//!   live_both_ways   parent and child both fault an untouched page, then
//!                    talk through it in both directions while both run
//!   multi_fork       six children and a grandchild each write their own
//!                    untouched page and a slot of a common one
//!   munmap           a child punches a hole / unmaps everything; the parent
//!                    unmaps while a child still uses the memory
//!   madvise          MADV_DONTNEED on shared memory keeps the data (zap +
//!                    refault the same page); private anon still zeroes
//!   mprotect         a sub-range mprotect splits the VMA: pages on both
//!                    sides of the split stay shared; PROT_NONE round trip
//!   threads_fork     a thread forks while the main thread forks too; all
//!                    four writers land in the one object
//!   devzero          MAP_SHARED /dev/zero is shared, MAP_PRIVATE is not
//!   memfd            MAP_SHARED of a memfd: an untouched page written by a
//!                    child is seen through the mapping and pread
//!   private_cow      MAP_PRIVATE anon still copies on write, both ways
//!   mremap           a shared mapping moved and grown by mremap still sees
//!                    (and is seen by) the child's mapping
//!   release          /proc/meminfo Shmem: the object outlives the parent's
//!                    munmap while a child maps it, goes with the last
//!                    mapping; execve in a child drops only its reference
//!   rss_shmem        /proc/self/status RssShmem counts touched shared pages
//!
//! Prints `<case>: PASS|FAIL`, exit status = number of failures.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;
use core::ptr::{read_volatile, write_volatile};

type c_int = i32;
type pid_t = i32;
type size_t = usize;

const PROT_NONE:     c_int = 0;
const PROT_READ:     c_int = 1;
const PROT_WRITE:    c_int = 2;
const MAP_SHARED:    c_int = 0x01;
const MAP_PRIVATE:   c_int = 0x02;
const MAP_ANONYMOUS: c_int = 0x20;
const O_RDONLY:      c_int = 0;
const O_RDWR:        c_int = 2;
const MADV_DONTNEED: usize = 4;
const MREMAP_MAYMOVE: usize = 1;
const PS:            size_t = 4096;

#[cfg(target_arch = "aarch64")]
mod nr {
    pub const MPROTECT: usize = 226;
    pub const MREMAP: usize = 216;
    pub const MADVISE: usize = 233;
    pub const FTRUNCATE: usize = 46;
    pub const MEMFD_CREATE: usize = 279;
    pub const PREAD64: usize = 67;
    pub const PWRITE64: usize = 68;
}
#[cfg(target_arch = "x86_64")]
mod nr {
    pub const MPROTECT: usize = 10;
    pub const MREMAP: usize = 25;
    pub const MADVISE: usize = 28;
    pub const FTRUNCATE: usize = 77;
    pub const MEMFD_CREATE: usize = 319;
    pub const PREAD64: usize = 17;
    pub const PWRITE64: usize = 18;
}

fn wifexited(status: c_int) -> bool { (status & 0x7f) == 0 }
fn wexitstatus(status: c_int) -> c_int { (status >> 8) & 0xff }

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;
    pub fn write(fd: c_int, buf: *const c_void, count: size_t) -> isize;
    pub fn read(fd: c_int, buf: *mut c_void, count: size_t) -> isize;
    pub fn open(path: *const u8, flags: c_int, ...) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn _exit(status: c_int) -> !;
    pub fn fork() -> pid_t;
    pub fn waitpid(pid: pid_t, stat_loc: *mut c_int, options: c_int) -> pid_t;
    pub fn execve(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> c_int;
    pub fn mmap(addr: *mut c_void, len: size_t, prot: c_int, flags: c_int,
                fd: c_int, offset: i64) -> *mut c_void;
    pub fn munmap(addr: *mut c_void, len: size_t) -> c_int;
    pub fn usleep(usec: u32) -> c_int;
    pub fn pthread_create(
        thread: *mut *mut c_void,
        attr: *const c_void,
        start_routine: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> c_int;
    pub fn pthread_join(thread: *mut c_void, retval: *mut *mut c_void) -> c_int;
}

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset shm_main",
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
    "   adrp x1, shm_main",
    "   add x1, x1, :lo12:shm_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { _exit(134); }
}

// ── Raw syscalls (calls relibc may not export) ───────────────────────────────

#[cfg(target_arch = "x86_64")]
unsafe fn sys(n: usize, a: usize, b: usize, c: usize, d: usize, e: usize) -> isize {
    let r: isize;
    core::arch::asm!("syscall", inlateout("rax") n as isize => r,
        in("rdi") a, in("rsi") b, in("rdx") c, in("r10") d, in("r8") e,
        lateout("rcx") _, lateout("r11") _, options(nostack));
    r
}
#[cfg(target_arch = "aarch64")]
unsafe fn sys(n: usize, a: usize, b: usize, c: usize, d: usize, e: usize) -> isize {
    let r: isize;
    core::arch::asm!("svc 0", in("x8") n, inlateout("x0") a as isize => r,
        in("x1") b, in("x2") c, in("x3") d, in("x4") e, options(nostack));
    r
}

unsafe fn mprotect(p: *mut u8, len: usize, prot: c_int) -> isize {
    sys(nr::MPROTECT, p as usize, len, prot as usize, 0, 0)
}
unsafe fn madvise(p: *mut u8, len: usize, adv: usize) -> isize {
    sys(nr::MADVISE, p as usize, len, adv, 0, 0)
}
unsafe fn mremap(p: *mut u8, old: usize, new: usize, flags: usize) -> isize {
    sys(nr::MREMAP, p as usize, old, new, flags, 0)
}

// ── Helpers ──────────────────────────────────────────────────────────────────

unsafe fn say(s: &[u8]) { write(1, s.as_ptr() as *const c_void, s.len()); }
unsafe fn say_num(n: u64) {
    let mut d = [0u8; 20]; let mut i = d.len(); let mut x = n;
    loop { i -= 1; d[i] = b'0' + (x % 10) as u8; x /= 10; if x == 0 { break; } }
    say(&d[i..]);
}
unsafe fn report(name: &[u8], ok: bool) -> bool {
    say(name); say(if ok { b": PASS\n" } else { b": FAIL\n" });
    ok
}
unsafe fn why(name: &[u8], what: &[u8], v: u64) -> bool {
    say(name); say(b": FAIL ("); say(what); say(b" "); say_num(v); say(b")\n");
    false
}

unsafe fn map(len: usize, prot: c_int, flags: c_int, fd: c_int) -> *mut u8 {
    let p = mmap(core::ptr::null_mut(), len, prot, flags, fd, 0);
    if p as isize == -1 { core::ptr::null_mut() } else { p as *mut u8 }
}
unsafe fn shmap(len: usize) -> *mut u8 {
    map(len, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1)
}
unsafe fn unmap(p: *mut u8, len: usize) { munmap(p as *mut c_void, len); }

unsafe fn rd(p: *mut u8, off: usize) -> u32 { read_volatile(p.add(off) as *const u32) }
unsafe fn wr(p: *mut u8, off: usize, v: u32) { write_volatile(p.add(off) as *mut u32, v) }

/// Spin (sleeping 1 ms a round) until the u32 at `p+off` reads `v`; false
/// after ~5 s.
unsafe fn wait_for(p: *mut u8, off: usize, v: u32) -> bool {
    for _ in 0..5000 {
        if rd(p, off) == v { return true; }
        usleep(1000);
    }
    false
}

/// Fork; the child runs `f` and exits with its result. Returns the pid.
unsafe fn spawn(f: impl FnOnce() -> c_int) -> pid_t {
    let pid = fork();
    if pid == 0 { _exit(f()); }
    pid
}

unsafe fn wait_exit_code(pid: pid_t) -> c_int {
    if pid < 0 { return -1; }
    let mut st: c_int = -1;
    if waitpid(pid, &mut st, 0) != pid || !wifexited(st) { return -1; }
    wexitstatus(st)
}

/// Value of `key` (e.g. b"Shmem:") in a /proc file, the first number on
/// its line.
unsafe fn proc_val(path: &[u8], key: &[u8]) -> Option<u64> {
    static mut BUF: [u8; 8192] = [0; 8192];
    let b = &mut *core::ptr::addr_of_mut!(BUF);
    let fd = open(path.as_ptr(), O_RDONLY);
    if fd < 0 { return None; }
    let mut n = 0usize;
    loop {
        let r = read(fd, b.as_mut_ptr().add(n) as *mut c_void, b.len() - n);
        if r <= 0 { break; }
        n += r as usize;
        if n == b.len() { break; }
    }
    close(fd);
    let s = &b[..n];
    let mut i = 0;
    while i + key.len() <= s.len() {
        if (i == 0 || s[i - 1] == b'\n') && &s[i..i + key.len()] == key {
            let mut j = i + key.len();
            while j < s.len() && (s[j] == b' ' || s[j] == b'\t') { j += 1; }
            let mut v = 0u64; let mut any = false;
            while j < s.len() && s[j].is_ascii_digit() { v = v * 10 + (s[j] - b'0') as u64; j += 1; any = true; }
            return if any { Some(v) } else { None };
        }
        i += 1;
    }
    None
}
unsafe fn shmem_kb() -> u64 { proc_val(b"/proc/meminfo\0", b"Shmem:").unwrap_or(u64::MAX) }

// ── Cases ────────────────────────────────────────────────────────────────────

unsafe fn t_live_both_ways() -> bool {
    let name = b"live_both_ways";
    let p = shmap(2 * PS);
    if p.is_null() { return why(name, b"mmap", 0); }
    let pid = spawn(|| {
        // The child faults page 0 first (its own read), before the parent
        // has written anything.
        if rd(p, 0) != 0 { return 1; }
        if !wait_for(p, 0, 0x1111) { return 2; }
        wr(p, PS, 0x2222);   // page 1: untouched by everyone so far
        wr(p, 8, 0x3333);
        0
    });
    usleep(20_000);
    let _ = rd(p, 64);       // the parent faults page 0 on its own as well
    wr(p, 0, 0x1111);
    let ok_wait = wait_for(p, PS, 0x2222);
    let rc = wait_exit_code(pid);
    let ok = ok_wait && rc == 0 && rd(p, 8) == 0x3333;
    unmap(p, 2 * PS);
    if !ok { return why(name, b"child rc / reply", rc as u64 + if ok_wait { 0 } else { 1000 }); }
    report(name, true)
}

unsafe fn t_multi_fork() -> bool {
    let name = b"multi_fork";
    let p = shmap(8 * PS);
    if p.is_null() { return why(name, b"mmap", 0); }
    let mut pids = [0 as pid_t; 6];
    for k in 0..6 {
        pids[k] = spawn(|| {
            wr(p, k * PS, 0xA0 + k as u32);
            wr(p, 7 * PS + 4 * k, 0xB0 + k as u32);
            0
        });
    }
    // A child whose own child (the grandchild) writes page 6.
    let mid = spawn(|| {
        let g = spawn(|| { wr(p, 6 * PS, 0xC6); 0 });
        if wait_exit_code(g) != 0 { return 1; }
        if rd(p, 6 * PS) != 0xC6 { return 2; }
        0
    });
    let mut bad = 0u64;
    for k in 0..6 { if wait_exit_code(pids[k]) != 0 { bad += 1; } }
    if wait_exit_code(mid) != 0 { bad += 100; }
    for k in 0..6 {
        if rd(p, k * PS) != 0xA0 + k as u32 { bad += 1000; }
        if rd(p, 7 * PS + 4 * k) != 0xB0 + k as u32 { bad += 10_000; }
    }
    if rd(p, 6 * PS) != 0xC6 { bad += 100_000; }
    unmap(p, 8 * PS);
    if bad != 0 { return why(name, b"bad", bad); }
    report(name, true)
}

unsafe fn t_munmap() -> bool {
    let name = b"munmap";
    let p = shmap(4 * PS);
    if p.is_null() { return why(name, b"mmap", 0); }
    // Child punches out page 1, then writes the pages around the hole.
    let a = spawn(|| {
        if munmap(p.add(PS) as *mut c_void, PS) != 0 { return 1; }
        wr(p, 0, 0xA0); wr(p, 2 * PS, 0xA2); wr(p, 3 * PS, 0xA3);
        0
    });
    if wait_exit_code(a) != 0 { unmap(p, 4 * PS); return why(name, b"child a", 0); }
    if rd(p, 0) != 0xA0 || rd(p, 2 * PS) != 0xA2 || rd(p, 3 * PS) != 0xA3 {
        unmap(p, 4 * PS); return why(name, b"after hole", 0);
    }
    wr(p, PS, 0xB1); // the parent's page 1 is still there
    // Child unmaps everything and exits: nothing of the parent's goes.
    let b = spawn(|| { if munmap(p as *mut c_void, 4 * PS) != 0 { 1 } else { 0 } });
    if wait_exit_code(b) != 0 || rd(p, PS) != 0xB1 || rd(p, 0) != 0xA0 {
        unmap(p, 4 * PS); return why(name, b"after child unmap", 0);
    }
    // Parent unmaps while a child still uses the memory.
    let chan = shmap(PS);
    let q = shmap(2 * PS);
    if chan.is_null() || q.is_null() { return why(name, b"mmap 2", 0); }
    wr(chan, 0, 0);
    let c = spawn(|| {
        if !wait_for(chan, 0, 1) { return 1; }
        if rd(q, 0) != 0x55 { return 2; }
        wr(q, PS, 0x66);          // untouched page, after the parent is gone
        if rd(q, PS) != 0x66 { return 3; }
        wr(chan, 4, 2);
        0
    });
    wr(q, 0, 0x55);
    unmap(q, 2 * PS);
    wr(chan, 0, 1);
    let rc = wait_exit_code(c);
    let ok = rc == 0 && rd(chan, 4) == 2;
    unmap(chan, PS);
    unmap(p, 4 * PS);
    if !ok { return why(name, b"parent-unmapped child rc", rc as u64); }
    report(name, true)
}

unsafe fn t_madvise() -> bool {
    let name = b"madvise";
    let p = shmap(2 * PS);
    if p.is_null() { return why(name, b"mmap", 0); }
    wr(p, 0, 0x11);
    if madvise(p, 2 * PS, MADV_DONTNEED) != 0 { unmap(p, 2 * PS); return why(name, b"madvise", 0); }
    if rd(p, 0) != 0x11 { unmap(p, 2 * PS); return why(name, b"data lost after DONTNEED", rd(p, 0) as u64); }
    let pid = spawn(|| {
        if madvise(p, 2 * PS, MADV_DONTNEED) != 0 { return 1; }
        if rd(p, 0) != 0x11 { return 2; }
        wr(p, PS, 0x22);
        if madvise(p.add(PS), PS, MADV_DONTNEED) != 0 { return 3; }
        if rd(p, PS) != 0x22 { return 4; }
        0
    });
    let rc = wait_exit_code(pid);
    let ok = rc == 0 && rd(p, PS) == 0x22;
    // The parent zaps again after the child is gone: the object keeps it.
    madvise(p, 2 * PS, MADV_DONTNEED);
    let ok = ok && rd(p, PS) == 0x22 && rd(p, 0) == 0x11;
    unmap(p, 2 * PS);
    if !ok { return why(name, b"child rc", rc as u64); }
    // Private anonymous memory still reads back zero after DONTNEED.
    let q = map(PS, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1);
    if q.is_null() { return why(name, b"mmap private", 0); }
    wr(q, 0, 0x77);
    madvise(q, PS, MADV_DONTNEED);
    let z = rd(q, 0);
    unmap(q, PS);
    if z != 0 { return why(name, b"private not zeroed", z as u64); }
    report(name, true)
}

unsafe fn t_mprotect() -> bool {
    let name = b"mprotect";
    let p = shmap(4 * PS);
    let chan = shmap(PS);
    if p.is_null() || chan.is_null() { return why(name, b"mmap", 0); }
    wr(chan, 0, 0);
    let pid = spawn(|| {
        // Split: [0] RW, [1] RO, [2..4] RW — three VMAs over one object.
        if mprotect(p.add(PS), PS, PROT_READ) != 0 { return 1; }
        if !wait_for(chan, 0, 1) { return 2; }
        if rd(p, PS) != 0x31 { return 3; }
        if mprotect(p.add(PS), PS, PROT_READ | PROT_WRITE) != 0 { return 4; }
        wr(p, PS + 4, 0x32);
        wr(p, 2 * PS, 0x34);
        wr(p, 3 * PS, 0x33);
        0
    });
    wr(p, PS, 0x31);
    wr(chan, 0, 1);
    let rc = wait_exit_code(pid);
    let mut ok = rc == 0 && rd(p, PS + 4) == 0x32 && rd(p, 2 * PS) == 0x34 && rd(p, 3 * PS) == 0x33;
    // PROT_NONE round trip in the parent keeps the shared data.
    if ok {
        ok = mprotect(p, 4 * PS, PROT_NONE) == 0
            && mprotect(p, 4 * PS, PROT_READ | PROT_WRITE) == 0
            && rd(p, 3 * PS) == 0x33 && rd(p, PS) == 0x31;
    }
    unmap(p, 4 * PS);
    unmap(chan, PS);
    if !ok { return why(name, b"child rc", rc as u64); }
    report(name, true)
}

static mut TH_REGION: *mut u8 = core::ptr::null_mut();
extern "C" fn th_forker(_a: *mut c_void) -> *mut c_void {
    unsafe {
        let p = TH_REGION;
        wr(p, 0, 0x41);
        let pid = spawn(|| { wr(p, PS, 0x42); 0 });
        let rc = wait_exit_code(pid);
        rc as usize as *mut c_void
    }
}

unsafe fn t_threads_fork() -> bool {
    let name = b"threads_fork";
    let p = shmap(4 * PS);
    if p.is_null() { return why(name, b"mmap", 0); }
    TH_REGION = p;
    let mut th: *mut c_void = core::ptr::null_mut();
    if pthread_create(&mut th, core::ptr::null(), th_forker, core::ptr::null_mut()) != 0 {
        unmap(p, 4 * PS); return why(name, b"pthread_create", 0);
    }
    let pid = spawn(|| { wr(p, 2 * PS, 0x43); 0 });
    let rc = wait_exit_code(pid);
    let mut th_rc: *mut c_void = core::ptr::null_mut();
    pthread_join(th, &mut th_rc);
    let ok = rc == 0 && th_rc.is_null()
        && rd(p, 0) == 0x41 && rd(p, PS) == 0x42 && rd(p, 2 * PS) == 0x43;
    unmap(p, 4 * PS);
    if !ok { return why(name, b"rc", rc as u64 + (th_rc as u64) * 100); }
    report(name, true)
}

unsafe fn t_devzero() -> bool {
    let name = b"devzero";
    let fd = open(b"/dev/zero\0".as_ptr(), O_RDWR);
    if fd < 0 { return why(name, b"open", 0); }
    let s = map(2 * PS, PROT_READ | PROT_WRITE, MAP_SHARED, fd);
    let v = map(2 * PS, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd);
    close(fd);
    if s.is_null() || v.is_null() { return why(name, b"mmap", 0); }
    let pid = spawn(|| {
        wr(s, PS, 0x51);
        wr(v, PS, 0x52);
        if rd(v, PS) != 0x52 { return 1; }
        0
    });
    let rc = wait_exit_code(pid);
    let ok = rc == 0 && rd(s, PS) == 0x51 && rd(v, PS) == 0 && rd(s, 0) == 0;
    let got = (rd(s, PS) as u64) << 8 | rd(v, PS) as u64;
    unmap(s, 2 * PS);
    unmap(v, 2 * PS);
    if !ok { return why(name, b"shared<<8|private", got); }
    report(name, true)
}

unsafe fn t_memfd() -> bool {
    let name = b"memfd";
    let fd = sys(nr::MEMFD_CREATE, b"shmtest\0".as_ptr() as usize, 0, 0, 0, 0) as c_int;
    if fd < 0 { return why(name, b"memfd_create", (-fd) as u64); }
    if sys(nr::FTRUNCATE, fd as usize, 3 * PS, 0, 0, 0) != 0 { close(fd); return why(name, b"ftruncate", 0); }
    let p = map(3 * PS, PROT_READ | PROT_WRITE, MAP_SHARED, fd);
    if p.is_null() { close(fd); return why(name, b"mmap", 0); }
    let pid = spawn(|| { wr(p, 2 * PS, 0x61); 0 });
    let rc = wait_exit_code(pid);
    let mut w = 0u32;
    let n = sys(nr::PREAD64, fd as usize, &mut w as *mut u32 as usize, 4, 2 * PS, 0);
    let ok = rc == 0 && rd(p, 2 * PS) == 0x61 && n == 4 && w == 0x61;
    unmap(p, 3 * PS);
    close(fd);
    if !ok { return why(name, b"pread", w as u64); }
    report(name, true)
}

/// Lazy MAP_SHARED of a memfd (lane mmapB): two mappings and the fd see the
/// same frames, pages nobody touched at mmap time are found at first touch
/// (also by a kernel copy and by a forked child), and the mappings keep
/// working after the last descriptor is closed.
unsafe fn t_memfd_lazy() -> bool {
    let name = b"memfd_lazy";
    let fd = sys(nr::MEMFD_CREATE, b"lazy\0".as_ptr() as usize, 0, 0, 0, 0) as c_int;
    if fd < 0 { return why(name, b"memfd_create", (-fd) as u64); }
    if sys(nr::FTRUNCATE, fd as usize, 8 * PS, 0, 0, 0) != 0 { close(fd); return why(name, b"ftruncate", 0); }
    let a = map(8 * PS, PROT_READ | PROT_WRITE, MAP_SHARED, fd);
    let b = map(8 * PS, PROT_READ | PROT_WRITE, MAP_SHARED, fd);
    if a.is_null() || b.is_null() { close(fd); return why(name, b"mmap", 0); }
    let mut bad = 0u64;
    // pwrite before any page was touched, seen through both mappings.
    let v = 0x11223344u32;
    if sys(nr::PWRITE64, fd as usize, &v as *const u32 as usize, 4, 3 * PS, 0) != 4 { bad |= 1; }
    if rd(a, 3 * PS) != v || rd(b, 3 * PS) != v { bad |= 2; }
    // A store through one mapping: the other mapping and pread see it.
    wr(a, 5 * PS, 0x55);
    let mut w = 0u32;
    if rd(b, 5 * PS) != 0x55 { bad |= 4; }
    if sys(nr::PREAD64, fd as usize, &mut w as *mut u32 as usize, 4, 5 * PS, 0) != 4 || w != 0x55 { bad |= 8; }
    // The kernel copies into a page of the mapping nobody touched yet.
    if sys(nr::PREAD64, fd as usize, a.add(2 * PS) as usize, 4, 3 * PS, 0) != 4 { bad |= 16; }
    if rd(b, 2 * PS) != v { bad |= 32; }
    // Last descriptor closed: untouched pages are still found.
    close(fd);
    if rd(a, 6 * PS) != 0 { bad |= 64; }
    wr(b, 6 * PS, 0x66);
    if rd(a, 6 * PS) != 0x66 { bad |= 128; }
    let pid = spawn(|| { wr(a, 7 * PS, 0x77); 0 });
    let rc = wait_exit_code(pid);
    if rc != 0 || rd(b, 7 * PS) != 0x77 { bad |= 256; }
    unmap(a, 8 * PS);
    if rd(b, 6 * PS) != 0x66 || rd(b, 5 * PS) != 0x55 { bad |= 512; }
    unmap(b, 8 * PS);
    if bad != 0 { return why(name, b"bad", bad); }
    report(name, true)
}

unsafe fn t_private_cow() -> bool {
    let name = b"private_cow";
    let p = map(2 * PS, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1);
    if p.is_null() { return why(name, b"mmap", 0); }
    wr(p, 0, 1);
    let pid = spawn(|| {
        wr(p, 0, 2); wr(p, PS, 3);
        if rd(p, 0) != 2 || rd(p, PS) != 3 { 1 } else { 0 }
    });
    let rc = wait_exit_code(pid);
    let ok = rc == 0 && rd(p, 0) == 1 && rd(p, PS) == 0;
    unmap(p, 2 * PS);
    if !ok { return why(name, b"rc", rc as u64); }
    report(name, true)
}

unsafe fn t_mremap() -> bool {
    let name = b"mremap";
    let p = shmap(2 * PS);
    let chan = shmap(PS);
    if p.is_null() || chan.is_null() { return why(name, b"mmap", 0); }
    wr(chan, 0, 0);
    wr(p, 0, 0x71);
    let pid = spawn(|| {
        if !wait_for(chan, 0, 1) { return 1; }
        if rd(p, PS) != 0x72 { return 2; }
        wr(p, 0, 0x73);
        0
    });
    let r = mremap(p, 2 * PS, 8 * PS, MREMAP_MAYMOVE);
    if r < 0 { wr(chan, 0, 1); wait_exit_code(pid); return why(name, b"mremap errno", (-r) as u64); }
    let q = r as *mut u8;
    let mut bad = 0u64;
    if rd(q, 0) != 0x71 { bad |= 1; }
    wr(q, PS, 0x72);          // untouched page, written through the moved map
    wr(chan, 0, 1);
    let rc = wait_exit_code(pid);
    if rc != 0 { bad |= 2 | (rc as u64) << 8; }
    if rd(q, 0) != 0x73 { bad |= 4; }
    if rd(q, 5 * PS) != 0 { bad |= 8; }
    wr(q, 7 * PS, 0x74);
    if rd(q, 7 * PS) != 0x74 { bad |= 16; }
    unmap(q, 8 * PS);
    unmap(chan, PS);
    if bad != 0 { return why(name, b"bad", bad); }
    report(name, true)
}

unsafe fn t_release() -> bool {
    let name = b"release";
    const N: usize = 64;
    let base = shmem_kb();
    if base == u64::MAX { return why(name, b"no Shmem in /proc/meminfo", 0); }
    let p = shmap(N * PS);
    let chan = shmap(PS);
    if p.is_null() || chan.is_null() { return why(name, b"mmap", 0); }
    wr(chan, 0, 0);
    for i in 0..N { wr(p, i * PS, i as u32 + 1); }
    let s1 = shmem_kb();
    if s1 < base + (N as u64) * 4 { return why(name, b"Shmem did not grow, kB", s1); }
    let pid = spawn(|| {
        if !wait_for(chan, 0, 1) { return 1; }
        for i in 0..N { if rd(p, i * PS) != i as u32 + 1 { return 2; } }
        0
    });
    unmap(p, N * PS);
    let s2 = shmem_kb();
    wr(chan, 0, 1);
    let rc = wait_exit_code(pid);
    if rc != 0 { return why(name, b"child rc", rc as u64); }
    if s2 < base + (N as u64) * 4 { return why(name, b"freed while the child maps it, kB", s2); }
    // execve in a child drops the child's reference only.
    let q = shmap(16 * PS);
    if q.is_null() { return why(name, b"mmap q", 0); }
    for i in 0..16 { wr(q, i * PS, 0x900 + i as u32); }
    let pid = spawn(|| {
        let argv = [b"hello\0".as_ptr(), core::ptr::null()];
        let envp = [core::ptr::null::<u8>()];
        execve(b"/bin/hello\0".as_ptr(), argv.as_ptr(), envp.as_ptr());
        9
    });
    let rc = wait_exit_code(pid);
    let mut intact = true;
    for i in 0..16 { if rd(q, i * PS) != 0x900 + i as u32 { intact = false; } }
    unmap(q, 16 * PS);
    unmap(chan, PS);
    if rc == 9 || !intact { return why(name, b"exec child rc", rc as u64); }
    // Everything this case created is gone (a little slack for others).
    let s3 = shmem_kb();
    if s3 > base + 16 { return why(name, b"Shmem not released, kB above base", s3 - base); }
    report(name, true)
}

unsafe fn t_rss_shmem() -> bool {
    let name = b"rss_shmem";
    let before = proc_val(b"/proc/self/status\0", b"RssShmem:").unwrap_or(0);
    let p = shmap(16 * PS);
    if p.is_null() { return why(name, b"mmap", 0); }
    for i in 0..16 { wr(p, i * PS, 1); }
    let after = proc_val(b"/proc/self/status\0", b"RssShmem:").unwrap_or(0);
    unmap(p, 16 * PS);
    let gone = proc_val(b"/proc/self/status\0", b"RssShmem:").unwrap_or(u64::MAX);
    if after < before + 64 { return why(name, b"RssShmem kB", after); }
    if gone > before { return why(name, b"RssShmem after munmap kB", gone); }
    report(name, true)
}

#[no_mangle]
pub unsafe extern "C" fn shm_main(_argc: isize, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let cases: [unsafe fn() -> bool; 12] = [
        t_live_both_ways, t_multi_fork, t_munmap, t_madvise, t_mprotect, t_threads_fork,
        t_devzero, t_memfd, t_memfd_lazy, t_private_cow, t_mremap, t_release, t_rss_shmem,
    ];
    let mut failures = 0;
    for c in cases.iter() { if !c() { failures += 1; } }
    say(b"--- shmtest done: ");
    say_num(failures as u64);
    say(b" failed ---\n");
    failures
}
