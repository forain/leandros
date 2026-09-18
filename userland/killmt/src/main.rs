//! killmt — `kill -9` of a multithreaded process must reap every thread and
//! leave every CPU alive.
//!
//! The scenario this guards: a process with threads busy in *user mode* on
//! other CPUs (no syscall in flight, some of them page-faulting on fresh
//! memory) receives SIGKILL. The kernel must stop every sibling before the
//! shared address space is torn down; the failure signature is
//! `[PF] no address space for faulting task` followed by a `[WDOG]` line for
//! each CPU that a sibling wedged on (seen 2026-09-16 with cosmic-launcher
//! under greetd's SIGKILL alarm).
//!
//! Each mode forks a child, waits for it to report all threads started, then
//! terminates it and `waitpid`s it with a 5 s bound. Modes:
//!
//!   spin_all          — leader + 3 workers spin in user mode (no syscalls)
//!   touch             — leader + 3 workers allocate 4 MiB and touch every
//!                       page in a loop (page faults + mmap/munmap)
//!   leader_parked     — leader parked on a futex, workers spin: the leader
//!                       is the blocked thread the signal is delivered to
//!   worker_parked     — one worker parked, leader + 2 workers spin: a
//!                       non-leader thread runs the group teardown while the
//!                       leader is on another CPU
//!   syscall           — every thread loops getpid(): SIGKILL lands mid-syscall
//!   stopped           — SIGSTOP the group (waitpid WUNTRACED confirms), then
//!                       SIGKILL it
//!   segv              — a worker dereferences NULL while the others spin:
//!                       the fault path runs the group kill
//!   exit_group_worker — a worker calls exit_group(42) while the others spin
//!   parked_mix        — three workers parked in *syscalls* (read on an empty
//!                       pipe, nanosleep, futex) while the leader spins: the
//!                       signal lands on one parked thread, which reaps the
//!                       other two off-CPU, in place
//!   leader_kills      — same three parked workers, but the leader itself
//!                       calls kill(getpid(), SIGKILL)
//!   exec_worker       — a non-leader thread execve()s while the leader and
//!                       a sibling spin: the new image must keep the process
//!                       pid, the fds the leader opened AND the fds the
//!                       exec'ing thread opened, lose the close-on-exec fd,
//!                       be able to wait for a child the thread forked, and
//!                       its exit status must reach the parent's waitpid
//!
//! Every mode also compares `sysinfo().freeram` before and after its
//! iterations (one warm-up iteration excluded): a kernel stack or address
//! space leaked per kill shows up as a monotonic drop.
//!
//! Prints `<mode>: PASS (n/n, max reap N ms, mem ±N KiB)` or `<mode>: FAIL ...`,
//! ends with `--- killmt done ---`; exit status is the failure count.
//!
//! usage: killmt [iterations] [mode]    (defaults 100, all modes)
//!        killmt --exec-child ...       (internal: the exec_worker image)

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

extern "C" {
    fn fork() -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn pipe(fds: *mut i32) -> i32;
    fn read(fd: i32, buf: *mut u8, n: usize) -> isize;
    fn write(fd: i32, buf: *const u8, n: usize) -> isize;
    fn close(fd: i32) -> i32;
    fn getpid() -> i32;
    fn _exit(code: i32) -> !;
    fn execve(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> i32;
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    fn nanosleep(req: *const Timespec, rem: *mut Timespec) -> i32;
    fn sysinfo(info: *mut SysInfo) -> i32;
}

#[repr(C)]
struct Timespec { tv_sec: i64, tv_nsec: i64 }

/// Linux `struct sysinfo` (112 bytes on 64-bit); only `freeram` is read.
#[repr(C)]
struct SysInfo {
    uptime: i64,
    loads: [u64; 3],
    totalram: u64,
    freeram: u64,
    sharedram: u64,
    bufferram: u64,
    totalswap: u64,
    freeswap: u64,
    procs: u16,
    pad: u16,
    totalhigh: u64,
    freehigh: u64,
    mem_unit: u32,
    _f: [u8; 0],
}

fn free_ram() -> u64 {
    let mut si = SysInfo {
        uptime: 0, loads: [0; 3], totalram: 0, freeram: 0, sharedram: 0, bufferram: 0,
        totalswap: 0, freeswap: 0, procs: 0, pad: 0, totalhigh: 0, freehigh: 0, mem_unit: 0, _f: [],
    };
    if unsafe { sysinfo(&mut si) } != 0 { return 0; }
    si.freeram * (si.mem_unit.max(1) as u64)
}

/// A kill loop that leaks one 128 KiB kernel stack per iteration shows up
/// as 12.5 MiB over 100 iterations; allow far less than that but enough
/// for allocator noise (pipe rings, exit-log churn).
const MEM_LEAK_BOUND: u64 = 8 << 20;

const F_GETFD: i32 = 1;
const F_SETFD: i32 = 2;
const FD_CLOEXEC: i32 = 1;

const SIGKILL: i32 = 9;
const SIGSEGV: i32 = 11;
const SIGSTOP: i32 = 19;
const WNOHANG: i32 = 1;
const WUNTRACED: i32 = 2;

fn wifexited(s: i32) -> bool { s & 0x7f == 0 }
fn wexitstatus(s: i32) -> i32 { (s >> 8) & 0xff }
fn wifsignaled(s: i32) -> bool { s & 0x7f != 0 && s & 0x7f != 0x7f }
fn wtermsig(s: i32) -> i32 { s & 0x7f }
fn wifstopped(s: i32) -> bool { s & 0xff == 0x7f }
fn wstopsig(s: i32) -> i32 { (s >> 8) & 0xff }

#[cfg(target_arch = "aarch64")]
mod nr { pub const EXIT_GROUP: u64 = 94; }
#[cfg(target_arch = "x86_64")]
mod nr { pub const EXIT_GROUP: u64 = 231; }

#[cfg(target_arch = "aarch64")]
unsafe fn syscall1(n: u64, a: u64) -> i64 {
    let r: i64;
    core::arch::asm!("svc 0", in("x8") n, inlateout("x0") a as i64 => r);
    r
}
#[cfg(target_arch = "x86_64")]
unsafe fn syscall1(n: u64, a: u64) -> i64 {
    let r: i64;
    core::arch::asm!("syscall", inlateout("rax") n as i64 => r, in("rdi") a,
                     lateout("rcx") _, lateout("r11") _);
    r
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    SpinAll,
    Touch,
    LeaderParked,
    WorkerParked,
    Syscall,
    Stopped,
    Segv,
    ExitGroupWorker,
    ParkedMix,
    LeaderKills,
    ExecWorker,
    SingleSleep,
    SingleSpin,
    SinglePipe,
}

const ALL_MODES: [Mode; 11] = [
    Mode::SpinAll, Mode::Touch, Mode::LeaderParked, Mode::WorkerParked,
    Mode::Syscall, Mode::Stopped, Mode::Segv, Mode::ExitGroupWorker,
    Mode::ParkedMix, Mode::LeaderKills, Mode::ExecWorker,
];
const EXTRA_MODES: [Mode; 3] = [Mode::SingleSleep, Mode::SingleSpin, Mode::SinglePipe];

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::SpinAll => "spin_all",
        Mode::Touch => "touch",
        Mode::LeaderParked => "leader_parked",
        Mode::WorkerParked => "worker_parked",
        Mode::Syscall => "syscall",
        Mode::Stopped => "stopped",
        Mode::Segv => "segv",
        Mode::ExitGroupWorker => "exit_group_worker",
        Mode::ParkedMix => "parked_mix",
        Mode::LeaderKills => "leader_kills",
        Mode::ExecWorker => "exec_worker",
        Mode::SingleSleep => "single_sleep",
        Mode::SingleSpin => "single_spin",
        Mode::SinglePipe => "single_pipe",
    }
}

fn mode_from_name(s: &str) -> Option<Mode> {
    ALL_MODES.iter().chain(EXTRA_MODES.iter()).copied().find(|m| mode_name(*m) == s)
}

/// Pure user-mode busy loop: no syscalls, so a SIGKILL can only reach this
/// thread through a timer tick or a reschedule IPI.
fn spin_forever() -> ! {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    loop {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        std::hint::black_box(x);
    }
}

/// Allocate 4 MiB (musl mmaps allocations this size), write every page —
/// each first touch is a demand-paging fault — then free it, forever.
fn touch_forever() -> ! {
    const BYTES: usize = 4 << 20;
    loop {
        let mut v: Vec<u8> = Vec::with_capacity(BYTES);
        unsafe { v.set_len(BYTES); }
        let p = v.as_mut_ptr();
        let mut off = 0;
        while off < BYTES {
            unsafe { core::ptr::write_volatile(p.add(off), (off >> 12) as u8); }
            off += 4096;
        }
        std::hint::black_box(&v);
        drop(v);
    }
}

fn syscall_forever() -> ! {
    loop { unsafe { getpid(); } }
}

fn park_forever() -> ! {
    loop { thread::park(); }
}

/// Blocked in `read` on a pipe nobody writes to.
fn pipe_read_forever() -> ! {
    let mut fds = [0i32; 2];
    unsafe { pipe(fds.as_mut_ptr()); }
    let mut b = 0u8;
    loop { unsafe { read(fds[0], &mut b, 1); } }
}

/// Blocked in `nanosleep` (10 s at a time, restarted forever).
fn sleep_forever() -> ! {
    loop {
        let ts = Timespec { tv_sec: 10, tv_nsec: 0 };
        unsafe { nanosleep(&ts, core::ptr::null_mut()); }
    }
}

/// One of the three syscall parks, by worker index: pipe read, nanosleep,
/// futex (`thread::park`).
fn parked_body(i: usize) -> ! {
    match i {
        0 => pipe_read_forever(),
        1 => sleep_forever(),
        _ => park_forever(),
    }
}

/// Wait until the given threads' parks are visible from the outside: the
/// pipe reader and sleeper cannot signal "I am now inside the syscall", so
/// give them a generous 20 ms after they report started.
fn settle() { spin_for(Duration::from_millis(20)); }

fn cstr(s: &str) -> Vec<u8> { let mut v = s.as_bytes().to_vec(); v.push(0); v }

/// The exec_worker child: a leader-opened pipe, a close-on-exec fd, two
/// spinning siblings and one worker that opens its own pipe, forks a child
/// and then execve()s this binary (`--exec-child`). What the new image finds
/// is reported through `res_fd` (see `exec_child_main`).
fn exec_child_body(res_fd: i32) -> ! {
    let mut lp = [0i32; 2];
    unsafe { pipe(lp.as_mut_ptr()); write(lp[1], b"L".as_ptr(), 1); }
    // A close-on-exec fd the new image must NOT see.
    let mut cx = [0i32; 2];
    unsafe { pipe(cx.as_mut_ptr()); fcntl(cx[0], F_SETFD, FD_CLOEXEC); }
    let expected_pid = unsafe { getpid() };
    let started = Arc::new(AtomicUsize::new(0));
    for i in 0..3 {
        let started = started.clone();
        thread::spawn(move || {
            started.fetch_add(1, Ordering::SeqCst);
            if i != 0 { spin_forever(); }
            // Let the siblings and the leader get onto their CPUs first.
            spin_for(Duration::from_millis(2));
            let mut wp = [0i32; 2];
            unsafe { pipe(wp.as_mut_ptr()); write(wp[1], b"W".as_ptr(), 1); }
            // A child of the *thread*: after the exec it must still be a
            // waitable child of the process.
            let gc = unsafe { fork() };
            if gc == 0 {
                let ts = Timespec { tv_sec: 0, tv_nsec: 5_000_000 };
                unsafe { nanosleep(&ts, core::ptr::null_mut()); _exit(7); }
            }
            let path = cstr("/bin/killmt");
            let args: Vec<Vec<u8>> = vec![
                cstr("killmt"), cstr("--exec-child"),
                cstr(&expected_pid.to_string()), cstr(&lp[0].to_string()),
                cstr(&wp[0].to_string()), cstr(&cx[0].to_string()),
                cstr(&gc.to_string()), cstr(&res_fd.to_string()),
            ];
            let mut argv: Vec<*const u8> = args.iter().map(|a| a.as_ptr()).collect();
            argv.push(core::ptr::null());
            let envp: [*const u8; 1] = [core::ptr::null()];
            unsafe { execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr()); }
            // execve returned: report and die so the parent does not hang.
            unsafe { write(res_fd, b"E0 execve failed\n".as_ptr(), 17); _exit(99); }
        });
    }
    // The leader keeps running old-image code until the exec stops it.
    spin_forever()
}

/// Entry of the image exec'd by exec_child_body. Argument order:
/// expected_pid leader_fd worker_fd cloexec_fd grandchild_pid result_fd.
fn exec_child_main(args: &[String]) -> ! {
    let n = |i: usize| -> i32 { args.get(i).and_then(|s| s.parse().ok()).unwrap_or(-1) };
    let (expected_pid, lfd, wfd, cxfd, gc, res) = (n(2), n(3), n(4), n(5), n(6), n(7));
    let mut report = String::new();
    let me = unsafe { getpid() };
    if me != expected_pid { report.push_str(&format!("E1 pid {} != {} ", me, expected_pid)); }
    let mut b = 0u8;
    let r = unsafe { read(lfd, &mut b, 1) };
    if r != 1 || b != b'L' { report.push_str(&format!("E2 leader fd {} read={} ", lfd, r)); }
    let r = unsafe { read(wfd, &mut b, 1) };
    if r != 1 || b != b'W' { report.push_str(&format!("E3 worker fd {} read={} ", wfd, r)); }
    if unsafe { fcntl(cxfd, F_GETFD) } != -1 { report.push_str(&format!("E4 cloexec fd {} survived ", cxfd)); }
    match wait_bounded(gc, 0, Duration::from_secs(5)) {
        Some(st) if wifexited(st) && wexitstatus(st) == 7 => {}
        Some(st) => report.push_str(&format!("E5 grandchild status 0x{:x} ", st)),
        None => report.push_str("E5 grandchild not reaped "),
    }
    // A process-level table that is NOT the fd table: the exe path.
    match std::fs::read_link("/proc/self/exe") {
        Ok(p) if p.to_string_lossy().ends_with("killmt") => {}
        Ok(p) => report.push_str(&format!("E6 exe {:?} ", p)),
        Err(e) => report.push_str(&format!("E6 exe {} ", e)),
    }
    let line = if report.is_empty() { "OK\n".to_string() } else { format!("{}\n", report) };
    unsafe { write(res, line.as_ptr(), line.len()); close(res); _exit(42); }
}

fn spin_for(d: Duration) {
    let t0 = Instant::now();
    while t0.elapsed() < d { std::hint::black_box(0u8); }
}

fn child_body(mode: Mode, ready_fd: i32) -> ! {
    const WORKERS: usize = 3;
    if mode == Mode::ExecWorker { exec_child_body(ready_fd); }
    if matches!(mode, Mode::SingleSleep | Mode::SingleSpin | Mode::SinglePipe) {
        unsafe { write(ready_fd, b"R".as_ptr(), 1); close(ready_fd); }
        match mode {
            Mode::SingleSleep => sleep_forever(),
            Mode::SinglePipe => pipe_read_forever(),
            _ => spin_forever(),
        }
    }
    let started = Arc::new(AtomicUsize::new(0));
    // Set once the parent has been told we are ready, so a self-inflicted
    // death (segv, exit_group_worker) cannot beat the ready byte.
    let go = Arc::new(AtomicBool::new(false));
    for i in 0..WORKERS {
        let started = started.clone();
        let go = go.clone();
        thread::spawn(move || {
            started.fetch_add(1, Ordering::SeqCst);
            match mode {
                Mode::SpinAll | Mode::LeaderParked | Mode::Stopped => spin_forever(),
                Mode::Touch => touch_forever(),
                Mode::WorkerParked => if i == 0 { park_forever() } else { spin_forever() },
                Mode::Syscall => syscall_forever(),
                Mode::Segv => {
                    if i == 0 {
                        while !go.load(Ordering::SeqCst) { std::hint::spin_loop(); }
                        spin_for(Duration::from_millis(2));
                        unsafe { core::ptr::write_volatile(8usize as *mut u64, 0xdead); }
                    }
                    touch_forever()
                }
                Mode::ExitGroupWorker => {
                    if i == 0 {
                        while !go.load(Ordering::SeqCst) { std::hint::spin_loop(); }
                        spin_for(Duration::from_millis(2));
                        unsafe { syscall1(nr::EXIT_GROUP, 42); }
                    }
                    spin_forever()
                }
                Mode::ParkedMix | Mode::LeaderKills => parked_body(i),
                Mode::ExecWorker | Mode::SingleSleep | Mode::SingleSpin | Mode::SinglePipe => unreachable!(),
            }
        });
    }
    while started.load(Ordering::SeqCst) < WORKERS { thread::yield_now(); }
    if matches!(mode, Mode::ParkedMix | Mode::LeaderKills) { settle(); }
    unsafe { write(ready_fd, b"R".as_ptr(), 1); close(ready_fd); }
    go.store(true, Ordering::SeqCst);
    match mode {
        Mode::LeaderParked => park_forever(),
        Mode::Touch | Mode::Segv => touch_forever(),
        Mode::Syscall => syscall_forever(),
        Mode::LeaderKills => {
            // The leader is the one on a CPU; the parked workers are the
            // ones the group kill must reap off-CPU.
            spin_for(Duration::from_millis(1));
            unsafe { kill(getpid(), SIGKILL); }
            spin_forever()
        }
        _ => spin_forever(),
    }
}

/// waitpid with a wall-clock bound. Returns Some(status) or None on timeout.
fn wait_bounded(pid: i32, options: i32, bound: Duration) -> Option<i32> {
    let t0 = Instant::now();
    loop {
        let mut st: i32 = 0;
        let r = unsafe { waitpid(pid, &mut st, options | WNOHANG) };
        if r == pid { return Some(st); }
        if r < 0 { return None; }
        if t0.elapsed() > bound { return None; }
        thread::sleep(Duration::from_millis(1));
    }
}

fn run_mode(mode: Mode, iters: usize) -> bool {
    let mut max_reap = Duration::ZERO;
    let mut mem_before = 0u64;
    for it in 0..iters {
        // Iteration 0 is the warm-up: first-use allocations (pipe rings,
        // fd tables, exit-log slots) are not leaks.
        if it == 1 { mem_before = free_ram(); }
        let mut fds = [0i32; 2];
        if unsafe { pipe(fds.as_mut_ptr()) } != 0 {
            println!("{}: FAIL pipe() at iteration {}", mode_name(mode), it);
            return false;
        }
        let pid = unsafe { fork() };
        if pid < 0 {
            println!("{}: FAIL fork() at iteration {}", mode_name(mode), it);
            return false;
        }
        if pid == 0 {
            unsafe { close(fds[0]); }
            child_body(mode, fds[1]);
        }
        unsafe { close(fds[1]); }
        let mut b = 0u8;
        let n = unsafe { read(fds[0], &mut b, 1) };
        if mode == Mode::ExecWorker {
            // The ready byte is the exec'd image's report line.
            let mut line = Vec::new();
            if n == 1 { line.push(b); }
            loop {
                let mut c = 0u8;
                let r = unsafe { read(fds[0], &mut c, 1) };
                if r != 1 || c == b'\n' { break; }
                line.push(c);
            }
            unsafe { close(fds[0]); }
            let text = String::from_utf8_lossy(&line).to_string();
            let t0 = Instant::now();
            let st = match wait_bounded(pid, 0, Duration::from_secs(5)) {
                Some(st) => st,
                None => {
                    println!("{}: FAIL iteration {}: child {} not reaped within 5 s (report {:?})", mode_name(mode), it, pid, text);
                    unsafe { kill(pid, SIGKILL); }
                    return false;
                }
            };
            let reap = t0.elapsed();
            if reap > max_reap { max_reap = reap; }
            if text != "OK" || !wifexited(st) || wexitstatus(st) != 42 {
                println!("{}: FAIL iteration {}: child {} report {:?} status=0x{:x} (expected OK / exit 42)",
                         mode_name(mode), it, pid, text, st);
                return false;
            }
            continue;
        }
        unsafe { close(fds[0]); }
        if n != 1 {
            println!("{}: FAIL child {} never reported ready (read={})", mode_name(mode), pid, n);
            unsafe { kill(pid, SIGKILL); }
            return false;
        }
        // Jitter the kill relative to the threads' start so it lands at
        // different points of the loops (mid-fault, mid-mmap, mid-spin).
        spin_for(Duration::from_micros(((it % 7) as u64) * 350));

        let expect_sig: Option<i32>;
        match mode {
            Mode::Segv => expect_sig = Some(SIGSEGV),
            Mode::ExitGroupWorker => expect_sig = None,
            Mode::LeaderKills => expect_sig = Some(SIGKILL),
            Mode::Stopped => {
                unsafe { kill(pid, SIGSTOP); }
                match wait_bounded(pid, WUNTRACED, Duration::from_secs(5)) {
                    Some(st) if wifstopped(st) && wstopsig(st) == SIGSTOP => {}
                    Some(st) => {
                        println!("{}: FAIL iteration {}: expected WIFSTOPPED, status=0x{:x}", mode_name(mode), it, st);
                        unsafe { kill(pid, SIGKILL); }
                        return false;
                    }
                    None => {
                        println!("{}: FAIL iteration {}: child {} did not stop within 5 s", mode_name(mode), it, pid);
                        unsafe { kill(pid, SIGKILL); }
                        return false;
                    }
                }
                unsafe { kill(pid, SIGKILL); }
                expect_sig = Some(SIGKILL);
            }
            _ => {
                unsafe { kill(pid, SIGKILL); }
                expect_sig = Some(SIGKILL);
            }
        }

        let t0 = Instant::now();
        let st = match wait_bounded(pid, 0, Duration::from_secs(5)) {
            Some(st) => st,
            None => {
                println!("{}: FAIL iteration {}: child {} not reaped within 5 s", mode_name(mode), it, pid);
                return false;
            }
        };
        let reap = t0.elapsed();
        if reap > max_reap { max_reap = reap; }

        let ok = match expect_sig {
            Some(sig) => wifsignaled(st) && wtermsig(st) == sig,
            None => wifexited(st) && wexitstatus(st) == 42,
        };
        if !ok {
            println!("{}: FAIL iteration {}: child {} status=0x{:x} (expected {})",
                     mode_name(mode), it, pid, st,
                     match expect_sig { Some(s) => format!("signal {}", s), None => "exit 42".to_string() });
            return false;
        }
    }
    let mem_after = free_ram();
    let delta = mem_after as i64 - mem_before as i64;
    if iters > 1 && mem_before != 0 && mem_after != 0 && mem_before.saturating_sub(mem_after) > MEM_LEAK_BOUND {
        println!("{}: FAIL free memory dropped {} KiB over {} iterations (bound {} KiB)",
                 mode_name(mode), (mem_before - mem_after) / 1024, iters - 1, MEM_LEAK_BOUND / 1024);
        return false;
    }
    println!("{}: PASS ({}/{}, max reap {} ms, mem {:+} KiB)", mode_name(mode), iters, iters,
             max_reap.as_millis(), delta / 1024);
    true
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("--exec-child") { exec_child_main(&args); }
    let iters: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(100);
    let modes: Vec<Mode> = match args.get(2) {
        Some(name) => match mode_from_name(name) {
            Some(m) => vec![m],
            None => {
                println!("unknown mode {:?}; modes: {}", name,
                         ALL_MODES.iter().map(|m| mode_name(*m)).collect::<Vec<_>>().join(" "));
                std::process::exit(2);
            }
        },
        None => ALL_MODES.to_vec(),
    };
    println!("killmt: {} iterations x {} modes, pid {}", iters, modes.len(), unsafe { getpid() });
    let mut failures = 0;
    for m in modes {
        if !run_mode(m, iters) { failures += 1; }
    }
    println!("--- killmt done --- failures={}", failures);
    unsafe { _exit(failures); }
}
