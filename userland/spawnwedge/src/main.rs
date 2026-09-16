//! spawnwedge — regression coverage for the musl thread/fork lock handoff
//! that wedged cosmic-comp the first time a keybinding spawned a command.
//!
//! musl keys its thread-list lock (`__thread_list_lock`) on
//! `pthread_self()->tid`, and `pthread_create` gets that tid from the kernel
//! through `CLONE_PARENT_SETTID` — it never assigns the clone() return value
//! itself. A kernel that ignores ptid leaves every non-main thread with tid 0;
//! such a thread "recursively" takes a free lock without storing anything,
//! and once one of them exits (pthread_exit holds the lock for the kernel's
//! CLEARTID release) the process-wide recursion count is off by one forever.
//! The main thread's next lock/unlock pair then leaves the word set to its
//! own tid, and the next fork() from another thread parks in `__tl_lock`
//! holding `__malloc_lock`, with every allocating thread queued behind it.
//!
//! Shape of the failing program, modelled here: a main thread that keeps
//! allocating, worker threads hammering malloc/free, and a short-lived
//! spawner thread per command that forks + execs (`Command::spawn` with a
//! `pre_exec` hook forces std onto plain fork(), which runs musl's atfork
//! sequence — `__malloc_atfork`, `__tl_lock`, `_Fork`) and then exits.
//!
//! Subtests, each printing `<name>: PASS` or `<name>: FAIL`:
//!
//!   thread_tid_visible      — a spawned thread's musl tid (recovered from
//!                             pthread_getcpuclockid's clockid encoding)
//!                             equals gettid(): the CLONE_PARENT_SETTID store
//!                             itself, the direct probe.
//!   fork_spawn_from_threads — N spawner threads fork+exec /bin/true in turn
//!                             under allocation load (the cosmic-comp path).
//!   posix_spawn_from_threads— the same through the vfork-style
//!                             clone(CLONE_VM|CLONE_VFORK) fast path.
//!
//! A wedge is detected by the main thread: a spawner that has not finished
//! within the deadline is reported as FAIL with raw syscalls only (no malloc,
//! no stdio lock, so the report cannot itself deadlock) and the process
//! exit_groups with a failure status, so the harness always gets a verdict.
//! Ends with "--- spawnwedge done ---"; exit status is the failure count.
//!
//! usage: spawnwedge [workers] [heap_mb] [spawns]   (defaults 4, 16, 20)

use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(target_arch = "aarch64")]
mod nr { pub const WRITE: u64 = 64; pub const EXIT_GROUP: u64 = 94; pub const NANOSLEEP: u64 = 101; pub const GETTID: u64 = 178; }
#[cfg(target_arch = "x86_64")]
mod nr { pub const WRITE: u64 = 1; pub const EXIT_GROUP: u64 = 231; pub const NANOSLEEP: u64 = 35; pub const GETTID: u64 = 186; }

#[cfg(target_arch = "aarch64")]
unsafe fn syscall3(n: u64, a: u64, b: u64, c: u64) -> i64 {
    let r: i64;
    core::arch::asm!("svc 0", in("x8") n, inlateout("x0") a as i64 => r, in("x1") b, in("x2") c);
    r
}
#[cfg(target_arch = "x86_64")]
unsafe fn syscall3(n: u64, a: u64, b: u64, c: u64) -> i64 {
    let r: i64;
    core::arch::asm!("syscall", inlateout("rax") n as i64 => r, in("rdi") a, in("rsi") b, in("rdx") c,
                     lateout("rcx") _, lateout("r11") _);
    r
}

fn raw_write(msg: &[u8]) { unsafe { syscall3(nr::WRITE, 1, msg.as_ptr() as u64, msg.len() as u64); } }
fn raw_exit_group(code: i32) -> ! {
    unsafe { syscall3(nr::EXIT_GROUP, code as u64, 0, 0); }
    loop { let ts = [1i64, 0i64]; unsafe { syscall3(nr::NANOSLEEP, ts.as_ptr() as u64, 0, 0); } }
}
fn gettid() -> i64 { unsafe { syscall3(nr::GETTID, 0, 0, 0) } }

extern "C" {
    fn pthread_self() -> usize;
    fn pthread_getcpuclockid(t: usize, clk: *mut i32) -> i32;
}

/// musl encodes a thread's kernel tid in its CPU clock id:
/// `clockid = (-tid - 1) * 8 + 6` (src/thread/pthread_getcpuclockid.c).
fn musl_self_tid() -> Option<i64> {
    let mut clk: i32 = 0;
    let r = unsafe { pthread_getcpuclockid(pthread_self(), &mut clk) };
    if r != 0 { return None; }
    let tid = -(((clk as i64) - 6) / 8) - 1;
    Some(tid)
}

fn report(name: &str, ok: bool) -> u32 {
    println!("{name}: {}", if ok { "PASS" } else { "FAIL" });
    if ok { 0 } else { 1 }
}

fn thread_tid_visible() -> u32 {
    let main_ok = musl_self_tid() == Some(gettid());
    let handle = thread::spawn(|| (musl_self_tid(), gettid()));
    let (musl, kernel) = handle.join().unwrap_or((None, -1));
    let ok = main_ok && musl == Some(kernel) && kernel > 0;
    if !ok { println!("  main_ok={main_ok} thread musl_tid={musl:?} gettid={kernel}"); }
    report("thread_tid_visible", ok)
}

/// One spawner thread per command, exactly like cosmic-comp's spawn_command,
/// while `workers` threads allocate continuously and the main thread
/// allocates between spawns. `pre_exec` selects fork() over posix_spawn.
fn spawn_from_threads(name: &str, pre_exec: bool, workers: usize, heap_mb: usize, spawns: usize) -> u32 {
    // Resident writable heap the fork has to copy — keeps the fork long
    // enough for the siblings to pile onto the locks it holds.
    let mut ballast: Vec<Vec<u8>> = Vec::new();
    for _ in 0..heap_mb {
        let mut v = vec![0u8; 1 << 20];
        let mut i = 0; while i < v.len() { v[i] = (i >> 12) as u8; i += 4096; }
        ballast.push(v);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let allocs = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    for _ in 0..workers {
        let stop = stop.clone(); let allocs = allocs.clone();
        hs.push(thread::spawn(move || {
            let mut bufs: Vec<Vec<u8>> = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                bufs.push(vec![0u8; 4096 + (allocs.load(Ordering::Relaxed) as usize & 0x3fff)]);
                if bufs.len() > 64 { bufs.clear(); }
                allocs.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    let t0 = Instant::now();
    let mut spawn_errs = 0;
    for i in 0..spawns {
        let done = Arc::new(AtomicBool::new(false));
        let err = Arc::new(AtomicBool::new(false));
        let (d2, e2) = (done.clone(), err.clone());
        let jh = thread::spawn(move || {
            let mut c = Command::new("/bin/true");
            if pre_exec { unsafe { c.pre_exec(|| Ok(())); } }
            match c.spawn() {
                Ok(mut ch) => { if !matches!(ch.wait(), Ok(s) if s.success()) { e2.store(true, Ordering::Release); } }
                Err(_) => e2.store(true, Ordering::Release),
            }
            d2.store(true, Ordering::Release);
        });
        // The main thread allocates while waiting, like an event loop; the
        // deadline turns a wedge into a verdict. Report with raw syscalls
        // only — the wedge holds __malloc_lock and the stdio lock may be
        // behind it — then exit_group, since a joined-on wedged thread never
        // returns.
        let st = Instant::now();
        while !done.load(Ordering::Acquire) {
            let _scratch = vec![0u8; 1024 + (i & 0xff)];
            if st.elapsed() > Duration::from_secs(10) {
                let mut msg = Vec::new();
                msg.extend_from_slice(name.as_bytes());
                msg.extend_from_slice(b": FAIL (wedged at spawn #");
                msg.extend_from_slice(i.to_string().as_bytes());
                msg.extend_from_slice(b")\n--- spawnwedge done ---\n");
                raw_write(&msg);
                raw_exit_group(1);
            }
            thread::sleep(Duration::from_millis(5));
        }
        // Join the spawner, but never block forever on it: its pthread_exit
        // takes the thread-list lock too and is the other place a broken
        // handoff shows up.
        let jt = Instant::now();
        while !jh.is_finished() {
            if jt.elapsed() > Duration::from_secs(10) {
                let mut msg = Vec::new();
                msg.extend_from_slice(name.as_bytes());
                msg.extend_from_slice(b": FAIL (spawner thread never exited, spawn #");
                msg.extend_from_slice(i.to_string().as_bytes());
                msg.extend_from_slice(b")\n--- spawnwedge done ---\n");
                raw_write(&msg);
                raw_exit_group(1);
            }
            thread::sleep(Duration::from_millis(5));
        }
        let _ = jh.join();
        if err.load(Ordering::Acquire) { spawn_errs += 1; }
    }
    stop.store(true, Ordering::Relaxed);
    for h in hs { let _ = h.join(); }
    println!("  {name}: spawns={spawns} spawn_errs={spawn_errs} allocs={} ballast={}MiB t={}ms",
             allocs.load(Ordering::Relaxed), ballast.len(), t0.elapsed().as_millis());
    report(name, spawn_errs == 0)
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let workers: usize = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(4);
    let heap_mb: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(16);
    let spawns:  usize = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(20);
    println!("--- spawnwedge: workers={workers} heap_mb={heap_mb} spawns={spawns} pid={} ---", std::process::id());

    let mut failures = 0;
    failures += thread_tid_visible();
    failures += spawn_from_threads("fork_spawn_from_threads", true, workers, heap_mb, spawns);
    failures += spawn_from_threads("posix_spawn_from_threads", false, workers, heap_mb, spawns);

    println!("--- spawnwedge done ---");
    std::process::exit(failures as i32);
}
