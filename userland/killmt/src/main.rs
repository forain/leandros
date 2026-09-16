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
//!
//! Prints `<mode>: PASS (n/n, max reap N ms)` or `<mode>: FAIL ...`, ends with
//! `--- killmt done ---`; exit status is the failure count.
//!
//! usage: killmt [iterations] [mode]    (defaults 100, all modes)

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
}

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
}

const ALL_MODES: [Mode; 8] = [
    Mode::SpinAll, Mode::Touch, Mode::LeaderParked, Mode::WorkerParked,
    Mode::Syscall, Mode::Stopped, Mode::Segv, Mode::ExitGroupWorker,
];

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
    }
}

fn mode_from_name(s: &str) -> Option<Mode> {
    ALL_MODES.iter().copied().find(|m| mode_name(*m) == s)
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

fn spin_for(d: Duration) {
    let t0 = Instant::now();
    while t0.elapsed() < d { std::hint::black_box(0u8); }
}

fn child_body(mode: Mode, ready_fd: i32) -> ! {
    const WORKERS: usize = 3;
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
            }
        });
    }
    while started.load(Ordering::SeqCst) < WORKERS { thread::yield_now(); }
    unsafe { write(ready_fd, b"R".as_ptr(), 1); close(ready_fd); }
    go.store(true, Ordering::SeqCst);
    match mode {
        Mode::LeaderParked => park_forever(),
        Mode::Touch | Mode::Segv => touch_forever(),
        Mode::Syscall => syscall_forever(),
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
    for it in 0..iters {
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
    println!("{}: PASS ({}/{}, max reap {} ms)", mode_name(mode), iters, iters, max_reap.as_millis());
    true
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
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
