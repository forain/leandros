//! Process-lifetime modes (lane proclife, 2026-10-19).
//!
//!   exit_settle — a multithreaded child (128 MiB touched, the only holder of
//!                 a pipe's write end) dies — SIGKILL landing on a parked
//!                 worker, a worker's exit_group(42) while the leader spins,
//!                 or SIGKILL on a parked leader — while its other threads
//!                 run on other CPUs. The parent spins on waitpid(WNOHANG)
//!                 and, the instant it returns, checks that the child is
//!                 really gone: the pipe reads EOF (its fd table was torn
//!                 down) and free memory is back within 64 MiB of the
//!                 pre-fork reading (its address space was dropped).
//!   exec_kill   — a non-leader thread execve()s while the leader and two
//!                 siblings run, and SIGKILL (kill() or tgkill() at the
//!                 leader) lands at a jittered point around the exec. The
//!                 parent must see exactly one death, by SIGKILL, under the
//!                 child's pid; no image may survive (the report pipe reaches
//!                 EOF) or run under another pid, and no second record may
//!                 appear.
//!   exec_state  — a non-leader execve: the new image must find the process
//!                 pid and tgid, the cwd, umask, ignored signal, reset
//!                 handler and pending alarm set before the threads were
//!                 spawned, the inherited fds through /proc/self/fd and
//!                 /proc/<pid>/fd, and working fds of its own from a new
//!                 thread.
use super::*;

extern "C" {
    fn umask(m: u32) -> u32;
    fn alarm(s: u32) -> u32;
    fn signal(sig: i32, h: usize) -> usize;
    fn getppid() -> i32;
    fn pthread_exit(v: *mut u8) -> !;
    fn readlink(p: *const u8, b: *mut u8, n: usize) -> isize;
    fn poll(fds: *mut PollFd, n: u64, timeout: i32) -> i32;
}

#[repr(C)]
struct PollFd { fd: i32, events: i16, revents: i16 }

const SIGUSR1: i32 = 10;
const SIGUSR2: i32 = 12;
const F_SETFL: i32 = 4;
const O_NONBLOCK: i32 = 0o4000;

#[cfg(target_arch = "aarch64")]
mod pnr { pub const TGKILL: u64 = 131; pub const RT_SIGACTION: u64 = 134; }
#[cfg(target_arch = "x86_64")]
mod pnr { pub const TGKILL: u64 = 234; pub const RT_SIGACTION: u64 = 13; }

#[cfg(target_arch = "aarch64")]
unsafe fn syscall4(n: u64, a: u64, b: u64, c: u64, d: u64) -> i64 {
    let r: i64;
    core::arch::asm!("svc 0", in("x8") n, inlateout("x0") a as i64 => r,
                     in("x1") b, in("x2") c, in("x3") d);
    r
}
#[cfg(target_arch = "x86_64")]
unsafe fn syscall4(n: u64, a: u64, b: u64, c: u64, d: u64) -> i64 {
    let r: i64;
    core::arch::asm!("syscall", inlateout("rax") n as i64 => r, in("rdi") a, in("rsi") b,
                     in("rdx") c, in("r10") d, lateout("rcx") _, lateout("r11") _);
    r
}

/// The current handler of `sig` (0 = SIG_DFL, 1 = SIG_IGN), via the raw
/// syscall so libc's own bookkeeping cannot answer instead of the kernel.
fn disposition(sig: i32) -> u64 {
    let mut old = [0u64; 4];
    unsafe { syscall4(pnr::RT_SIGACTION, sig as u64, 0, old.as_mut_ptr() as u64, 8); }
    old[0]
}

extern "C" fn usr2_handler(_: i32) {}

pub const NAMES: [&str; 7] = ["exit_settle", "exec_kill", "exec_sibkill", "exec_exit", "exec_state", "leaderless_exit", "exec_leaderless"];

/// Run proclife mode `name` for `iters` iterations; prints PASS/FAIL.
pub fn run(name: &str, iters: usize) -> bool {
    let r = match name {
        "exit_settle" => exit_settle(iters.min(60)),
        "exec_kill" => exec_kill(iters),
        "exec_sibkill" => exec_sibkill(iters),
        "exec_exit" => exec_exit(iters),
        "exec_state" => exec_state(iters.min(20)),
        "leaderless_exit" => leaderless_exit(iters.min(20)),
        "exec_leaderless" => exec_leaderless(iters.min(20)),
        _ => Err(format!("unknown mode {}", name)),
    };
    match r {
        Ok(s) => { println!("{}: PASS ({})", name, s); true }
        Err(e) => { println!("{}: FAIL {}", name, e); false }
    }
}

const SETTLE_BYTES: usize = 128 << 20;
const SETTLE_SLACK: u64 = 64 << 20;

fn exit_settle_child(variant: usize, ready: i32) -> ! {
    // The footprint the parent must see returned by the time wait returns.
    let mut v: Vec<u8> = Vec::with_capacity(SETTLE_BYTES);
    unsafe { v.set_len(SETTLE_BYTES); }
    let p = v.as_mut_ptr();
    let mut off = 0;
    while off < SETTLE_BYTES { unsafe { core::ptr::write_volatile(p.add(off), 1); } off += 4096; }
    std::mem::forget(v);
    let started = Arc::new(AtomicUsize::new(0));
    let go = Arc::new(AtomicBool::new(false));
    for i in 0..2 {
        let (started, go) = (started.clone(), go.clone());
        thread::spawn(move || {
            started.fetch_add(1, Ordering::SeqCst);
            match (variant, i) {
                (0, 0) => park_forever(),           // the SIGKILL target: a non-leader owner
                (1, 0) => {
                    while !go.load(Ordering::SeqCst) { std::hint::spin_loop(); }
                    spin_for(Duration::from_millis(1));
                    unsafe { syscall1(nr::EXIT_GROUP, 42); }
                    spin_forever()
                }
                _ => spin_forever(),
            }
        });
    }
    while started.load(Ordering::SeqCst) < 2 { thread::yield_now(); }
    if variant == 0 { spin_for(Duration::from_millis(5)); } // let worker 0 park
    unsafe { write(ready, b"R".as_ptr(), 1); close(ready); }
    go.store(true, Ordering::SeqCst);
    if variant == 2 { park_forever(); }
    spin_forever()
}

fn exit_settle(iters: usize) -> Result<String, String> {
    let mut worst_short: i64 = i64::MIN;
    for it in 0..iters {
        let variant = it % 3;
        let base = settled_free_ram();
        let (mut rp, mut hp) = ([0i32; 2], [0i32; 2]);
        unsafe { pipe(rp.as_mut_ptr()); pipe(hp.as_mut_ptr()); }
        let pid = unsafe { fork() };
        if pid < 0 { return Err(format!("fork failed at {}", it)); }
        if pid == 0 {
            unsafe { close(rp[0]); close(hp[0]); }
            exit_settle_child(variant, rp[1]);
        }
        unsafe { close(rp[1]); close(hp[1]); fcntl(hp[0], F_SETFL, O_NONBLOCK); }
        let mut b = 0u8;
        let n = unsafe { read(rp[0], &mut b, 1) };
        unsafe { close(rp[0]); }
        if n != 1 { unsafe { kill(pid, SIGKILL); close(hp[0]); } return Err(format!("it {}: child never ready", it)); }
        spin_for(Duration::from_micros(((it / 3) % 7) as u64 * 300));
        if variant != 1 { unsafe { kill(pid, SIGKILL); } }
        // Spin, no sleep: catch the earliest moment the child is waitable.
        let t0 = Instant::now();
        let mut st = 0i32;
        loop {
            let r = unsafe { waitpid(pid, &mut st, WNOHANG) };
            if r == pid { break; }
            if r < 0 || t0.elapsed() > Duration::from_secs(5) {
                unsafe { kill(pid, SIGKILL); close(hp[0]); }
                return Err(format!("it {} v{}: child {} not reaped (r={})", it, variant, pid, r));
            }
        }
        let freed = free_ram();
        let eof = unsafe { read(hp[0], &mut b, 1) };
        unsafe { close(hp[0]); }
        let ok_status = if variant == 1 { wifexited(st) && wexitstatus(st) == 42 }
                        else { wifsignaled(st) && wtermsig(st) == SIGKILL };
        if !ok_status { return Err(format!("it {} v{}: status 0x{:x}", it, variant, st)); }
        if eof != 0 {
            return Err(format!("it {} v{}: waitable while its fd table was still open (read={})", it, variant, eof));
        }
        let short = base as i64 - freed as i64;
        if short > worst_short { worst_short = short; }
        if short > SETTLE_SLACK as i64 {
            return Err(format!("it {} v{}: waitable with {} MiB of its memory still allocated",
                               it, variant, short >> 20));
        }
    }
    Ok(format!("{}/{}, worst {:+} MiB at wait", iters, iters, -(worst_short >> 20)))
}

/// `--exec-spin <fd>`: the image exec_kill's thread execs. Reports its pid
/// and spins until killed.
pub fn exec_spin_main(args: &[String]) -> ! {
    let fd: i32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(-1);
    let me = unsafe { getpid() };
    // A mark that needs no inherited fd: an image whose fds were torn down
    // under it (or that runs under a foreign pid) still leaves this.
    let _ = std::fs::write(format!("{}/img-{}", MARK_DIR, me), b"");
    let line = format!("P{}\n", me);
    unsafe { write(fd, line.as_ptr(), line.len()); }
    spin_forever()
}

const MARK_DIR: &str = "/tmp/killmt-exec";

/// The pids of exec'd images that left a mark (and removes the marks).
fn take_marks() -> Vec<i32> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(MARK_DIR) {
        for e in rd.flatten() {
            if let Some(p) = e.file_name().to_string_lossy().strip_prefix("img-").and_then(|p| p.parse().ok()) {
                v.push(p);
            }
            let _ = std::fs::remove_file(e.path());
        }
    }
    v
}

/// exec_exit's child: worker 0 execve()s while the leader calls
/// exit_group(42) `delay_us` after the worker announced the exec.
fn exec_exit_child(res: i32, delay_us: u64) -> ! {
    let go = Arc::new(AtomicBool::new(false));
    for i in 0..2 {
        let go = go.clone();
        thread::spawn(move || {
            if i == 1 { spin_forever(); }
            spin_for(Duration::from_millis(1));
            go.store(true, Ordering::SeqCst);
            let path = cstr("/bin/killmt");
            let args = [cstr("killmt"), cstr("--exec-spin"), cstr(&res.to_string())];
            let argv: [*const u8; 4] = [args[0].as_ptr(), args[1].as_ptr(), args[2].as_ptr(), core::ptr::null()];
            let envp: [*const u8; 1] = [core::ptr::null()];
            unsafe { execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr()); _exit(99); }
        });
    }
    while !go.load(Ordering::SeqCst) { std::hint::spin_loop(); }
    spin_for(Duration::from_micros(delay_us));
    unsafe { syscall1(nr::EXIT_GROUP, 42); }
    spin_forever()
}

/// exec_exit: the leader's exit_group races a non-leader execve. Either the
/// exit wins (status exit 42, no image ever runs) or the exec does (the image
/// runs under the process pid; killed, status SIGKILL). What must never
/// happen: the parent told "exit 42" while an image runs on — orphaned under
/// the exec'ing thread's own pid, with the fd table closed under it.
fn exec_exit(iters: usize) -> Result<String, String> {
    let _ = std::fs::create_dir_all(MARK_DIR);
    take_marks();
    let (mut exited, mut execd) = (0usize, 0usize);
    for it in 0..iters {
        let mut rp = [0i32; 2];
        unsafe { pipe(rp.as_mut_ptr()); }
        let delay = (it % 25) as u64 * 150;
        let pid = unsafe { fork() };
        if pid < 0 { return Err(format!("fork failed at {}", it)); }
        if pid == 0 { unsafe { close(rp[0]); } exec_exit_child(rp[1], delay); }
        unsafe { close(rp[1]); }
        // Wait for an outcome, not for a fixed time: the process is reaped
        // (exit won) or the image announced itself (exec won). Under host
        // load an exec can take longer than any fixed probe, and killing it
        // before its mark is a false "status SIGKILL, no image".
        let t0 = Instant::now();
        let mark = format!("{}/img-{}", MARK_DIR, pid);
        let mut st = None;
        loop {
            let mut s0 = 0i32;
            if unsafe { waitpid(pid, &mut s0, WNOHANG) } == pid { st = Some(s0); break; }
            if std::path::Path::new(&mark).exists() { break; }
            if t0.elapsed() > Duration::from_secs(20) {
                let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).unwrap_or_default();
                unsafe { kill(pid, SIGKILL); }
                return Err(format!("it {}: child {} neither exited nor exec'd in 20 s (stat {:?})", it, pid, stat.trim()));
            }
            thread::sleep(Duration::from_millis(1));
        }
        // Give a stray image time to leave its mark.
        thread::sleep(Duration::from_millis(30));
        let marks = take_marks();
        for &m in &marks { if m != pid { unsafe { kill(m, SIGKILL); } } }
        if st.is_none() {
            unsafe { kill(pid, SIGKILL); }
            st = wait_bounded(pid, 0, Duration::from_secs(20));
        }
        let (text, eof) = drain(rp[0], Duration::from_secs(10));
        unsafe { close(rp[0]); }
        let st = match st { Some(s) => s, None => return Err(format!("it {}: child {} not reaped", it, pid)) };
        if let Some(&m) = marks.iter().find(|&&m| m != pid) {
            return Err(format!("it {}: the exec'd image ran as pid {} (process {}, status 0x{:x})", it, m, pid, st));
        }
        if !eof { return Err(format!("it {}: pipe never closed ({:?})", it, text)); }
        if wifexited(st) && wexitstatus(st) == 42 {
            if !marks.is_empty() { return Err(format!("it {}: exit 42 reported but the image ran", it)); }
            exited += 1;
        } else if wifsignaled(st) && wtermsig(st) == SIGKILL && marks.contains(&pid) {
            execd += 1;
        } else {
            return Err(format!("it {}: status 0x{:x}, image marks {:?}", it, st, marks));
        }
        let mut st2 = 0i32;
        let r2 = unsafe { waitpid(-1, &mut st2, WNOHANG) };
        if r2 != -1 { return Err(format!("it {}: a second wait returned {} status 0x{:x}", it, r2, st2)); }
    }
    Ok(format!("{}/{}, exit won {} / exec won {}", iters, iters, exited, execd))
}

/// `parked`: sibling 2 parks on a futex instead of spinning — the thread a
/// process-directed SIGKILL prefers, and one the takeover reaps before the
/// leader.
fn exec_kill_child(res: i32, parked: bool) -> ! {
    let started = Arc::new(AtomicUsize::new(0));
    for i in 0..3 {
        let started = started.clone();
        thread::spawn(move || {
            started.fetch_add(1, Ordering::SeqCst);
            match i {
                0 => {
                    spin_for(Duration::from_millis(1));
                    unsafe { write(res, b"X".as_ptr(), 1); }
                    let path = cstr("/bin/killmt");
                    let args = [cstr("killmt"), cstr("--exec-spin"), cstr(&res.to_string())];
                    let argv: [*const u8; 4] = [args[0].as_ptr(), args[1].as_ptr(), args[2].as_ptr(), core::ptr::null()];
                    let envp: [*const u8; 1] = [core::ptr::null()];
                    unsafe { execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr()); _exit(99); }
                }
                1 => syscall_forever(),
                _ if parked => park_forever(),
                _ => spin_forever(),
            }
        });
    }
    while started.load(Ordering::SeqCst) < 3 { thread::yield_now(); }
    spin_forever()
}

/// Read `fd` until EOF or `bound`; returns (text, reached_eof).
fn drain(fd: i32, bound: Duration) -> (String, bool) {
    let t0 = Instant::now();
    let mut out = Vec::new();
    loop {
        let left = bound.saturating_sub(t0.elapsed());
        if left.is_zero() { return (String::from_utf8_lossy(&out).to_string(), false); }
        let mut pf = PollFd { fd, events: 1, revents: 0 };
        if unsafe { poll(&mut pf, 1, left.as_millis().max(1) as i32) } <= 0 { continue; }
        let mut buf = [0u8; 64];
        let n = unsafe { read(fd, buf.as_mut_ptr(), buf.len()) };
        if n <= 0 { return (String::from_utf8_lossy(&out).to_string(), n == 0); }
        out.extend_from_slice(&buf[..n as usize]);
    }
}

fn exec_kill(iters: usize) -> Result<String, String> {
    let (mut before, mut after) = (0usize, 0usize);
    for it in 0..iters {
        let mut rp = [0i32; 2];
        unsafe { pipe(rp.as_mut_ptr()); }
        let pid = unsafe { fork() };
        if pid < 0 { return Err(format!("fork failed at {}", it)); }
        if pid == 0 { unsafe { close(rp[0]); } exec_kill_child(rp[1], it % 4 >= 2); }
        unsafe { close(rp[1]); }
        let mut b = 0u8;
        if unsafe { read(rp[0], &mut b, 1) } != 1 || b != b'X' {
            unsafe { kill(pid, SIGKILL); close(rp[0]); }
            return Err(format!("it {}: no exec marker", it));
        }
        // Land the kill before, inside and after the takeover.
        spin_for(Duration::from_micros((it % 25) as u64 * 200));
        if it % 2 == 0 { unsafe { kill(pid, SIGKILL); } }
        else { unsafe { syscall4(pnr::TGKILL, pid as u64, pid as u64, SIGKILL as u64, 0); } }
        let st = match wait_bounded(pid, 0, Duration::from_secs(5)) {
            Some(st) => st,
            None => {
                unsafe { kill(pid, SIGKILL); close(rp[0]); }
                let late = wait_bounded(pid, 0, Duration::from_secs(10)).is_some();
                return Err(format!("it {}: child {} not reaped (a second SIGKILL reaped it: {})", it, pid, late));
            }
        };
        let (text, eof) = drain(rp[0], Duration::from_secs(3));
        unsafe { close(rp[0]); }
        let mut seen_image = false;
        for l in text.lines() {
            if let Some(p) = l.strip_prefix('P') {
                let p: i32 = p.trim().parse().unwrap_or(-1);
                if p != pid {
                    unsafe { if p > 0 { kill(p, SIGKILL); } }
                    return Err(format!("it {}: the new image ran as pid {} (process {})", it, p, pid));
                }
                seen_image = true;
            }
        }
        if !eof {
            return Err(format!("it {}: child {} reported dead (0x{:x}) but its pipe never closed: something survived ({:?})",
                               it, pid, st, text));
        }
        if !(wifsignaled(st) && wtermsig(st) == SIGKILL) {
            return Err(format!("it {}: status 0x{:x}, expected SIGKILL (image ran: {})", it, st, seen_image));
        }
        let mut st2 = 0i32;
        let r2 = unsafe { waitpid(-1, &mut st2, WNOHANG) };
        if r2 != -1 {
            return Err(format!("it {}: a second wait returned {} status 0x{:x} after {} died", it, r2, st2, pid));
        }
        if seen_image { after += 1; } else { before += 1; }
    }
    Ok(format!("{}/{}, killed before image {} / after {}", iters, iters, before, after))
}

/// exec_sibkill's child: `spinners` threads keep every CPU busy, one thread
/// parks on a futex, and the exec'ing thread sends the process a SIGKILL
/// immediately before its execve. A process-directed SIGKILL prefers a
/// Blocked thread, so it lands on the parked one, which is made Ready but
/// waits for a CPU while the exec runs; the de-thread then reaps it.
fn exec_sibkill_child(res: i32, spinners: usize) -> ! {
    let started = Arc::new(AtomicUsize::new(0));
    for _ in 0..spinners {
        let started = started.clone();
        thread::spawn(move || { started.fetch_add(1, Ordering::SeqCst); spin_forever() });
    }
    {
        let started = started.clone();
        thread::spawn(move || { started.fetch_add(1, Ordering::SeqCst); park_forever() });
    }
    while started.load(Ordering::SeqCst) < spinners + 1 { thread::yield_now(); }
    spin_for(Duration::from_millis(2)); // let the parker reach its futex wait
    let path = cstr("/bin/killmt");
    let args = [cstr("killmt"), cstr("--exec-spin"), cstr(&res.to_string())];
    let argv: [*const u8; 4] = [args[0].as_ptr(), args[1].as_ptr(), args[2].as_ptr(), core::ptr::null()];
    let envp: [*const u8; 1] = [core::ptr::null()];
    unsafe {
        write(res, b"X".as_ptr(), 1);
        kill(getpid(), SIGKILL);
        execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr());
        _exit(99);
    }
}

/// exec_sibkill: a SIGKILL sent to the process just before a leader execve
/// (it lands on a parked sibling the de-thread reaps) must still kill the
/// process: the parent reaps it, by SIGKILL, promptly. Before lane killexec
/// the kill died with the sibling and the new image spun on unkilled.
fn exec_sibkill(iters: usize) -> Result<String, String> {
    let mut image = 0usize;
    for it in 0..iters {
        let mut rp = [0i32; 2];
        unsafe { pipe(rp.as_mut_ptr()); }
        let pid = unsafe { fork() };
        if pid < 0 { return Err(format!("fork failed at {}", it)); }
        if pid == 0 { unsafe { close(rp[0]); } exec_sibkill_child(rp[1], 8); }
        unsafe { close(rp[1]); }
        let st = wait_bounded(pid, 0, Duration::from_secs(5));
        let (text, _) = drain(rp[0], Duration::from_millis(200));
        unsafe { close(rp[0]); }
        if text.contains('P') { image += 1; }
        let st = match st {
            Some(s) => s,
            None => {
                unsafe { kill(pid, SIGKILL); }
                let _ = wait_bounded(pid, 0, Duration::from_secs(10));
                return Err(format!("it {}: child {} survived its SIGKILL (image ran: {})", it, pid, text.contains('P')));
            }
        };
        if !(wifsignaled(st) && wtermsig(st) == SIGKILL) {
            return Err(format!("it {}: status 0x{:x}, expected SIGKILL", it, st));
        }
    }
    Ok(format!("{}/{}, image started {}", iters, iters, image))
}

fn exec_state_child(res: i32) -> ! {
    let _ = std::env::set_current_dir("/tmp");
    unsafe {
        umask(0o027);
        signal(SIGUSR1, 1);
        signal(SIGUSR2, usr2_handler as extern "C" fn(i32) as usize);
        alarm(100);
    }
    let mut lp = [0i32; 2];
    unsafe { pipe(lp.as_mut_ptr()); write(lp[1], b"L".as_ptr(), 1); }
    let mut cx = [0i32; 2];
    unsafe { pipe(cx.as_mut_ptr()); fcntl(cx[0], F_SETFD, FD_CLOEXEC); }
    let me = unsafe { getpid() };
    let parent = unsafe { getppid() };
    for i in 0..2 {
        thread::spawn(move || {
            if i == 1 { spin_forever(); }
            spin_for(Duration::from_millis(2));
            let mut wp = [0i32; 2];
            unsafe { pipe(wp.as_mut_ptr()); write(wp[1], b"W".as_ptr(), 1); }
            let path = cstr("/bin/killmt");
            let args: Vec<Vec<u8>> = vec![
                cstr("killmt"), cstr("--exec-state"), cstr(&me.to_string()), cstr(&parent.to_string()),
                cstr(&lp[0].to_string()), cstr(&wp[0].to_string()), cstr(&cx[0].to_string()),
                cstr(&res.to_string()),
            ];
            let mut argv: Vec<*const u8> = args.iter().map(|a| a.as_ptr()).collect();
            argv.push(core::ptr::null());
            let envp: [*const u8; 1] = [core::ptr::null()];
            unsafe { execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr()); write(res, b"S0 execve failed\n".as_ptr(), 17); _exit(99); }
        });
    }
    spin_forever()
}

fn fd_list(dir: &str) -> Result<Vec<i32>, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("{}: {}", dir, e))?;
    Ok(rd.flatten().filter_map(|e| e.file_name().to_string_lossy().parse().ok()).collect())
}

/// `--exec-state pid ppid lfd wfd cxfd res`: the image exec_state's thread execs.
pub fn exec_state_main(args: &[String]) -> ! {
    let n = |i: usize| -> i32 { args.get(i).and_then(|s| s.parse().ok()).unwrap_or(-1) };
    let (pid, ppid, lfd, wfd, cxfd, res) = (n(2), n(3), n(4), n(5), n(6), n(7));
    let mut rep = String::new();
    let me = unsafe { getpid() };
    if me != pid { rep.push_str(&format!("S1 pid {} != {} ", me, pid)); }
    if unsafe { getppid() } != ppid { rep.push_str(&format!("S1 ppid {} != {} ", unsafe { getppid() }, ppid)); }
    match std::fs::read_to_string("/proc/self/status") {
        Ok(s) => {
            let field = |k: &str| s.lines().find_map(|l| l.strip_prefix(k)).map(|v| v.trim().to_string());
            let want = pid.to_string();
            if field("Tgid:").as_deref() != Some(&want) || field("Pid:").as_deref() != Some(&want) {
                rep.push_str(&format!("S2 status Tgid={:?} Pid={:?} ", field("Tgid:"), field("Pid:")));
            }
        }
        Err(e) => rep.push_str(&format!("S2 status {} ", e)),
    }
    match std::env::current_dir() {
        Ok(d) if d.to_string_lossy() == "/tmp" => {}
        d => rep.push_str(&format!("S3 cwd {:?} ", d)),
    }
    let um = unsafe { umask(0o022) };
    if um != 0o027 { rep.push_str(&format!("S4 umask {:o} ", um)); }
    let d1 = disposition(SIGUSR1);
    if d1 != 1 { rep.push_str(&format!("S5 SIGUSR1 disposition {} (want SIG_IGN) ", d1)); }
    let d2 = disposition(SIGUSR2);
    if d2 != 0 { rep.push_str(&format!("S6 SIGUSR2 disposition {:#x} (want SIG_DFL) ", d2)); }
    let left = unsafe { alarm(0) };
    if left == 0 || left > 100 { rep.push_str(&format!("S7 alarm left {} ", left)); }
    // The listing's
    // own directory fd may take the number the close-on-exec fd had, so that
    // one is checked with fcntl.
    if unsafe { fcntl(cxfd, F_GETFD) } != -1 { rep.push_str(&format!("S8 cloexec fd {} survived ", cxfd)); }
    for dir in ["/proc/self/fd".to_string()] {
        match fd_list(&dir) {
            Ok(v) => {
                if !v.contains(&lfd) || !v.contains(&wfd) { rep.push_str(&format!("S8 {} lacks {}/{}: {:?} ", dir, lfd, wfd, v)); }
            }
            Err(e) => rep.push_str(&format!("S8 {} ", e)),
        }
        match std::fs::read_link(format!("{}/{}", dir, lfd)) {
            Ok(p) if p.to_string_lossy().starts_with("pipe:") => {}
            p => rep.push_str(&format!("S9 {}/{} -> {:?} ", dir, lfd, p)),
        }
    }
    let mut b = 0u8;
    if unsafe { read(lfd, &mut b, 1) } != 1 || b != b'L' { rep.push_str("S10 leader fd ") }
    if unsafe { read(wfd, &mut b, 1) } != 1 || b != b'W' { rep.push_str("S10 worker fd ") }
    // Fresh fds of the new image, shared with a thread it spawns.
    let mut np = [0i32; 2];
    if unsafe { pipe(np.as_mut_ptr()) } != 0 { rep.push_str("S11 pipe ") } else {
        let w = np[1];
        let h = thread::spawn(move || unsafe { write(w, b"T".as_ptr(), 1) });
        let _ = h.join();
        if unsafe { read(np[0], &mut b, 1) } != 1 || b != b'T' { rep.push_str("S11 thread fd ") }
        match fd_list("/proc/self/fd") {
            Ok(v) if v.contains(&np[0]) && v.contains(&np[1]) => {}
            v => rep.push_str(&format!("S11 /proc/self/fd {:?} lacks {:?} ", v, np)),
        }
    }
    let line = if rep.is_empty() { "OK\n".to_string() } else { format!("{}\n", rep) };
    unsafe { write(res, line.as_ptr(), line.len()); close(res); _exit(42); }
}

fn exec_state(iters: usize) -> Result<String, String> {
    for it in 0..iters {
        let mut rp = [0i32; 2];
        unsafe { pipe(rp.as_mut_ptr()); }
        let pid = unsafe { fork() };
        if pid < 0 { return Err(format!("fork failed at {}", it)); }
        if pid == 0 { unsafe { close(rp[0]); } exec_state_child(rp[1]); }
        unsafe { close(rp[1]); }
        let (text, _) = drain(rp[0], Duration::from_secs(10));
        unsafe { close(rp[0]); }
        let st = match wait_bounded(pid, 0, Duration::from_secs(5)) {
            Some(st) => st,
            None => { unsafe { kill(pid, SIGKILL); } return Err(format!("it {}: child {} not reaped ({:?})", it, pid, text)); }
        };
        if text.trim() != "OK" || !wifexited(st) || wexitstatus(st) != 42 {
            return Err(format!("it {}: report {:?} status 0x{:x}", it, text.trim(), st));
        }
    }
    Ok(format!("{}/{}", iters, iters))
}

#[cfg(target_arch = "x86_64")]  const SYS_EXIT_THREAD: u64 = 60;
#[cfg(target_arch = "aarch64")] const SYS_EXIT_THREAD: u64 = 93;

/// L5.12: the leader pthread_exits; a worker carries on and checks that the
/// process (its pid, sigactions, children, exe) still exists, then ends the
/// process with exit_group (variant 0, status 7) or with a plain thread exit
/// as the last thread (variant 1, status 0 = the leader's, as on Linux).
fn leaderless_exit_child(res: i32, go: i32, variant: usize) -> ! {
    unsafe { signal(SIGUSR1, 1); }                       // SIG_IGN, set on the leader
    let gc = unsafe { fork() };
    if gc == 0 { spin_for(Duration::from_millis(30)); unsafe { _exit(5); } }
    thread::spawn(move || {
        spin_for(Duration::from_millis(20));            // the leader is a Zombie by now
        await_go(go);
        let mut rep = String::new();
        let me = unsafe { getpid() };
        if unsafe { kill(me, 0) } != 0 { rep.push_str("X1 kill(pid,0) "); }
        unsafe { kill(me, SIGUSR1); }                   // must stay ignored
        spin_for(Duration::from_millis(2));
        let mut st = 0i32;
        let w = unsafe { waitpid(gc, &mut st, 0) };
        if w != gc || !wifexited(st) || wexitstatus(st) != 5 { rep.push_str(&format!("X3 grandchild wait {} 0x{:x} ", w, st)); }
        let mut b = [0u8; 64];
        // Linux: the leaderless group has no exe (ENOENT) and its leader's fd
        // directory exists but lists nothing; the worker's own view works.
        let n = unsafe { readlink(b"/proc/self/exe\0".as_ptr(), b.as_mut_ptr(), b.len()) };
        if n >= 0 { rep.push_str("X4 exe resolves ") }
        let n = unsafe { readlink(format!("/proc/{}/exe\0", me).as_ptr(), b.as_mut_ptr(), b.len()) };
        if n >= 0 { rep.push_str("X4 /proc/pid/exe resolves ") }
        match fd_list(&format!("/proc/{}/fd", me)) {
            Ok(v) if v.is_empty() => {}
            v => rep.push_str(&format!("X5 /proc/pid/fd {:?} (want empty) ", v)),
        }
        match fd_list("/proc/self/fd") {
            Ok(v) if v.is_empty() => {}
            v => rep.push_str(&format!("X5 /proc/self/fd {:?} (want empty) ", v)),
        }
        let link = format!("/proc/{}/fd/{}\0", me, res);
        if unsafe { readlink(link.as_ptr(), b.as_mut_ptr(), b.len()) } >= 0 { rep.push_str("X5 fd link resolves ") }
        match fd_list("/proc/thread-self/fd") {
            Ok(v) if v.contains(&res) => {}
            v => rep.push_str(&format!("X6 thread-self/fd {:?} lacks {} ", v, res)),
        }
        let line = if rep.is_empty() { "OK\n".to_string() } else { format!("{}\n", rep) };
        unsafe { write(res, line.as_ptr(), line.len()); close(res); }
        if variant == 0 { unsafe { _exit(7) } }
        unsafe { syscall1(SYS_EXIT_THREAD, 0); }
        loop {}
    });
    unsafe { pthread_exit(core::ptr::null_mut()) }
}

/// The X0 checks ask "is the process still unreported while a worker lives
/// on?". The worker must not finish the process before the parent looked, or
/// a parent descheduled for a few ms under load sees the (correct) final
/// status: the worker blocks on this pipe until `release`.
/// The pipe is created before fork; the read end is close-on-exec.
fn go_pipe() -> [i32; 2] {
    let mut p = [0i32; 2];
    unsafe { pipe(p.as_mut_ptr()); fcntl(p[0], F_SETFD, FD_CLOEXEC); fcntl(p[1], F_SETFD, FD_CLOEXEC); }
    p
}

fn await_go(fd: i32) {
    let mut b = 0u8;
    unsafe { read(fd, &mut b, 1); close(fd); }
}

fn release(fd: i32) {
    unsafe { write(fd, b"g".as_ptr(), 1); close(fd); }
}

fn leaderless_exit(iters: usize) -> Result<String, String> {
    for it in 0..iters {
        let variant = it % 2;
        let mut rp = [0i32; 2];
        unsafe { pipe(rp.as_mut_ptr()); }
        let go = go_pipe();
        let pid = unsafe { fork() };
        if pid < 0 { return Err(format!("fork failed at {}", it)); }
        if pid == 0 { unsafe { close(rp[0]); close(go[1]); } leaderless_exit_child(rp[1], go[0], variant); }
        unsafe { close(rp[1]); close(go[0]); }
        // X0: the leader's pthread_exit must not be reported as the process exit.
        spin_for(Duration::from_millis(10));
        // The directory stays while a thread lives (Linux: true), for another process too.
        if !std::fs::metadata(format!("/proc/{}/fd", pid)).map_or(false, |m| m.is_dir()) {
            return Err(format!("it {}: X0 /proc/{}/fd is not a directory", it, pid));
        }
        if let Err(e) = fd_list(&format!("/proc/{}/fd", pid)) { return Err(format!("it {}: X0 {}", it, e)); }
        let me = unsafe { getpid() };
        match fd_list(&format!("/proc/{}/fd", me)) {
            Ok(v) if v.contains(&rp[0]) => {}
            v => return Err(format!("it {}: own /proc/{}/fd {:?} lacks {}", it, me, v, rp[0])),
        }
        match std::fs::read_link(format!("/proc/{}/fd/{}", me, rp[0])) {
            Ok(p) if p.to_string_lossy().starts_with("pipe:") => {}
            p => return Err(format!("it {}: own fd link {:?}", it, p)),
        }
        let mut st = 0i32;
        if unsafe { waitpid(pid, &mut st, 1 /* WNOHANG */) } != 0 {
            return Err(format!("it {}: X0 process reported after leader pthread_exit (0x{:x})", it, st));
        }
        release(go[1]);
        let (text, _) = drain(rp[0], Duration::from_secs(10));
        unsafe { close(rp[0]); }
        let st = match wait_bounded(pid, 0, Duration::from_secs(5)) {
            Some(st) => st,
            None => { unsafe { kill(pid, SIGKILL); } return Err(format!("it {}: {} not reaped ({:?})", it, pid, text)); }
        };
        let want = if variant == 0 { 7 } else { 0 };
        if text.trim() != "OK" || !wifexited(st) || wexitstatus(st) != want {
            return Err(format!("it {} v{}: report {:?} status 0x{:x}", it, variant, text.trim(), st));
        }
    }
    Ok(format!("{}/{}", iters, iters))
}

/// L5.12: like exec_state_child, but the leader pthread_exits first and a
/// worker execs 20 ms later: the new image must run under the leader's pid
/// with the leader's sigactions, and the parent must see one exit (42).
fn exec_leaderless_child(res: i32, go: i32) -> ! {
    let _ = std::env::set_current_dir("/tmp");
    unsafe {
        umask(0o027);
        signal(SIGUSR1, 1);
        signal(SIGUSR2, usr2_handler as extern "C" fn(i32) as usize);
        alarm(100);
    }
    let mut lp = [0i32; 2];
    unsafe { pipe(lp.as_mut_ptr()); write(lp[1], b"L".as_ptr(), 1); }
    let mut cx = [0i32; 2];
    unsafe { pipe(cx.as_mut_ptr()); fcntl(cx[0], F_SETFD, FD_CLOEXEC); }
    let me = unsafe { getpid() };
    let parent = unsafe { getppid() };
    thread::spawn(move || {
        spin_for(Duration::from_millis(20));
        await_go(go);
        let mut wp = [0i32; 2];
        unsafe { pipe(wp.as_mut_ptr()); write(wp[1], b"W".as_ptr(), 1); }
        let path = cstr("/bin/killmt");
        let args: Vec<Vec<u8>> = vec![
            cstr("killmt"), cstr("--exec-state"), cstr(&me.to_string()), cstr(&parent.to_string()),
            cstr(&lp[0].to_string()), cstr(&wp[0].to_string()), cstr(&cx[0].to_string()),
            cstr(&res.to_string()),
        ];
        let mut argv: Vec<*const u8> = args.iter().map(|a| a.as_ptr()).collect();
        argv.push(core::ptr::null());
        let envp: [*const u8; 1] = [core::ptr::null()];
        unsafe { execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr()); write(res, b"S0 execve failed\n".as_ptr(), 17); _exit(99); }
    });
    unsafe { pthread_exit(core::ptr::null_mut()) }
}

fn exec_leaderless(iters: usize) -> Result<String, String> {
    for it in 0..iters {
        let mut rp = [0i32; 2];
        unsafe { pipe(rp.as_mut_ptr()); }
        let go = go_pipe();
        let pid = unsafe { fork() };
        if pid < 0 { return Err(format!("fork failed at {}", it)); }
        if pid == 0 { unsafe { close(rp[0]); close(go[1]); } exec_leaderless_child(rp[1], go[0]); }
        unsafe { close(rp[1]); close(go[0]); }
        // X0: the leader's pthread_exit must not be reported as the process exit.
        spin_for(Duration::from_millis(10));
        let mut st0 = 0i32;
        if unsafe { waitpid(pid, &mut st0, 1 /* WNOHANG */) } != 0 {
            return Err(format!("it {}: X0 process reported after leader pthread_exit (0x{:x})", it, st0));
        }
        release(go[1]);
        let (text, _) = drain(rp[0], Duration::from_secs(10));
        unsafe { close(rp[0]); }
        let st = match wait_bounded(pid, 0, Duration::from_secs(5)) {
            Some(st) => st,
            None => { unsafe { kill(pid, SIGKILL); } return Err(format!("it {}: child {} not reaped ({:?})", it, pid, text)); }
        };
        if text.trim() != "OK" || !wifexited(st) || wexitstatus(st) != 42 {
            return Err(format!("it {}: report {:?} status 0x{:x}", it, text.trim(), st));
        }
    }
    Ok(format!("{}/{}", iters, iters))
}
