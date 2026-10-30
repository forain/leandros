//! Boot-time kernel self-tests for the lane-kfacil facilities.
//!
//! Off unless the kernel command line (Limine `cmdline:`) names them:
//!   `ktest=all` or `ktest=irqctx,sync,kthread,workqueue,hrtimer,atomicpool,kva,pci,msi,firmware`
//!   `ktest.fw=/path/to/file`   file for the firmware test (default: the
//!                              largest of a few known big files)
//! They run in a kernel thread started just before the scheduler, print
//! `[KTEST] <name> PASS|FAIL` per test and a `[KTEST] summary` line. The
//! `msi` test wants `-device edu` (and, for MSI-X, a second
//! `-device virtio-rng-pci,disable-legacy=on`); the `firmware` test waits
//! up to 120 s for userspace init to mount the real root.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use sched::hrtimer::{HrTimer, TimerMode};
use sched::ksync::{KCompletion, KMutex, KRwLock, KSemaphore, KSpinLock, KWaitQueue};
use sched::{irqctx, kthread};

const ALL: [&str; 11] = ["irqctx", "sync", "kthread", "workqueue", "hrtimer", "atomicpool", "kva", "pci", "msi", "firmware", "gpulock"];
/// Opt-in only (not in `all`): a timing report, not a pass/fail facility test.
const EXTRA: [&str; 4] = ["memperf", "kmodwin", "kmodload", "kmodkapi"];

/// UART only (`arch_serial_putc`), never the framebuffer console: a console
/// print flushes the framebuffer through virtio-gpu, and a test thread
/// printing while another CPU initialises the GPU under `VIRTIO_GPU` would
/// make that CPU's own `[GPU]` diagnostics drain these lines into a flush
/// that needs the lock it holds (seen on x86_64 TCG during development).
fn log(s: &str) { drivers::pci::serial_debug(s); }
macro_rules! say { ($($a:tt)*) => { log(&format!("[KTEST] {}\n", format!($($a)*))) }; }

struct T { name: &'static str, fails: u32, skipped: bool }

impl T {
    fn new(name: &'static str) -> T { say!("{}: start", name); T { name, fails: 0, skipped: false } }
    fn check(&mut self, ok: bool, what: &str) {
        if ok { say!("{}: ok   {}", self.name, what); } else { self.fails += 1; say!("{}: FAIL {}", self.name, what); }
    }
    fn skip(&mut self, why: &str) { self.skipped = true; say!("{}: SKIP {}", self.name, why); }
}

fn now() -> u64 { sched::monotonic_ns() }

fn opt<'a>(cmd: &'a str, key: &str) -> Option<&'a str> {
    cmd.split_ascii_whitespace().find_map(|t| t.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

/// Start the self-test thread if `ktest=` is on the command line.
pub fn maybe_start() {
    let cmd = match crate::boot_cmdline() { Some(c) => c, None => return };
    let list = match opt(cmd, "ktest") { Some(l) => l, None => return };
    let mut names: Vec<&'static str> = Vec::new();
    for n in list.split(',') {
        if n == "all" { names.extend_from_slice(&ALL); continue; }
        if let Some(k) = ALL.iter().chain(EXTRA.iter()).find(|k| **k == n) { names.push(k); } else { say!("unknown test '{}'", n); }
    }
    let fw = opt(cmd, "ktest.fw").map(String::from);
    if kthread::spawn("ktest", move || run(names, fw)).is_none() {
        log("[KTEST] could not spawn the test thread\n");
    }
}

fn run(names: Vec<&'static str>, fw: Option<String>) {
    // Let boot settle (servers, userspace init) a little.
    kthread::msleep(200);
    let (mut pass, mut fail, mut skip) = (0, 0, 0);
    for n in names {
        let t = match n {
            "irqctx" => t_irqctx(),
            "sync" => t_sync(),
            "kthread" => t_kthread(),
            "workqueue" => t_workqueue(),
            "hrtimer" => t_hrtimer(),
            "atomicpool" => t_atomicpool(),
            "kva" => t_kva(),
            "memperf" => t_memperf(),
            "kmodwin" => t_kmodwin(),
            "kmodload" => t_kmodload(),
            "kmodkapi" => t_kmodkapi(),
            "pci" => t_pci(),
            "msi" => t_msi(),
            "firmware" => t_firmware(fw.as_deref()),
            "gpulock" => t_gpulock(),
            _ => continue,
        };
        if t.skipped && t.fails == 0 { say!("{} SKIP", t.name); skip += 1; }
        else if t.fails == 0 { say!("{} PASS", t.name); pass += 1; }
        else { say!("{} FAIL ({} checks failed)", t.name, t.fails); fail += 1; }
    }
    say!("summary: {} pass, {} fail, {} skip; sleep-in-atomic detections {}", pass, fail, skip, irqctx::sleep_violations());
}

// ── irqctx ──────────────────────────────────────────────────────────────────

static IRQCTX_SEEN: AtomicU32 = AtomicU32::new(0);

extern "C" fn irqctx_cb(_c: usize) {
    let mut v = 1;
    if irqctx::in_irq() { v |= 2; }
    if !irqctx::can_sleep() { v |= 4; }
    if irqctx::irq_depth() >= 1 { v |= 8; }
    IRQCTX_SEEN.store(v, Ordering::Release);
}

fn t_irqctx() -> T {
    let mut t = T::new("irqctx");
    t.check(!irqctx::in_irq() && !irqctx::in_interrupt() && irqctx::irq_depth() == 0, "thread context is not IRQ context");
    t.check(irqctx::can_sleep(), "thread context may sleep");
    let c0 = (0..sched::active_cpu_count()).map(irqctx::irq_count).sum::<u32>();
    let tm = HrTimer::new(irqctx_cb, 0, TimerMode::Irq);
    tm.start(5_000_000, 0);
    let t0 = now();
    while IRQCTX_SEEN.load(Ordering::Acquire) == 0 && now() - t0 < 1_000_000_000 { kthread::msleep(2); }
    let v = IRQCTX_SEEN.load(Ordering::Acquire);
    t.check(v == 15, &format!("hrtimer IRQ callback sees in_irq, depth>=1, !can_sleep (flags {:#x})", v));
    let c1 = (0..sched::active_cpu_count()).map(irqctx::irq_count).sum::<u32>();
    t.check(c1 > c0, &format!("irq_enter counted interrupts ({} -> {})", c0, c1));
    t.check(irqctx::irq_depth() == 0, "nesting counter back to 0 in the thread");
    {
        let l = KSpinLock::new(0u32);
        let g = l.lock();
        t.check(!irqctx::can_sleep() && irqctx::sleep_forbidden_reason() == Some("holding a KSpinLock"),
            "holding a KSpinLock forbids sleeping");
        let v0 = irqctx::sleep_violations();
        let s = KSemaphore::new(0);
        let t1 = now();
        let got = s.down_timeout(3_000_000);
        let dt = now() - t1;
        t.check(!got && dt >= 3_000_000, &format!("down_timeout under a spinlock spins and times out ({} us)", dt / 1000));
        t.check(irqctx::sleep_violations() == v0 + 1, "sleep-in-atomic detected and counted");
        drop(g);
    }
    t.check(irqctx::can_sleep() && irqctx::atomic_depth() == 0, "released: may sleep again");
    sched::preempt_disable();
    t.check(irqctx::sleep_forbidden_reason() == Some("preemption disabled"), "preempt_disable forbids sleeping");
    sched::preempt_enable();
    t
}

// ── sync ────────────────────────────────────────────────────────────────────

struct SyncState {
    m: KMutex<u64>,
    inside: AtomicU32,
    max_inside: AtomicU32,
    rw: KRwLock<u32>,
    readers_now: AtomicU32,
    readers_max: AtomicU32,
    writer_saw_readers: AtomicU32,
    sem: KSemaphore,
    sem_waiters_seen: AtomicBool,
    wq: KWaitQueue,
    flag: AtomicU32,
    woke: AtomicU32,
}

extern "C" fn sem_up_cb(ctx: usize) {
    let s = unsafe { &*(ctx as *const SyncState) };
    s.sem_waiters_seen.store(s.sem.has_waiters(), Ordering::Release);
    s.sem.up();
}

extern "C" fn wq_set_cb(ctx: usize) {
    let s = unsafe { &*(ctx as *const SyncState) };
    s.flag.store(1, Ordering::Release);
    s.wq.wake_all();
}

fn t_sync() -> T {
    let mut t = T::new("sync");
    let st = Arc::new(SyncState {
        m: KMutex::new(0), inside: AtomicU32::new(0), max_inside: AtomicU32::new(0),
        rw: KRwLock::new(0), readers_now: AtomicU32::new(0), readers_max: AtomicU32::new(0),
        writer_saw_readers: AtomicU32::new(0),
        sem: KSemaphore::new(0), sem_waiters_seen: AtomicBool::new(false),
        wq: KWaitQueue::new(), flag: AtomicU32::new(0), woke: AtomicU32::new(0),
    });

    // Mutex: 4 threads x 200 increments, sleeping inside now and then.
    let t0 = now();
    let mut ths = Vec::new();
    for i in 0..4u32 {
        let s = st.clone();
        ths.push(kthread::spawn("ktest-mtx", move || {
            for k in 0..200u32 {
                let mut g = s.m.lock();
                let n = s.inside.fetch_add(1, Ordering::AcqRel) + 1;
                s.max_inside.fetch_max(n, Ordering::AcqRel);
                let v = *g;
                if k % 25 == (i * 5) { kthread::msleep(1); }
                *g = v + 1;
                s.inside.fetch_sub(1, Ordering::AcqRel);
                drop(g);
            }
        }).unwrap());
    }
    for th in &ths { th.join(); }
    let total = *st.m.lock();
    t.check(total == 800, &format!("mutex: 4x200 increments = {} in {} ms", total, (now() - t0) / 1_000_000));
    t.check(st.max_inside.load(Ordering::Acquire) == 1, "mutex: never two holders");

    // try_lock / lock_timeout against a holder.
    let s2 = st.clone();
    let holder = kthread::spawn("ktest-hold", move || { let _g = s2.m.lock(); kthread::msleep(100); }).unwrap();
    kthread::msleep(20);
    t.check(st.m.try_lock().is_none(), "mutex: try_lock fails while held");
    let a = now();
    let r = st.m.lock_timeout(30_000_000);
    let dt = now() - a;
    t.check(r.is_none() && dt >= 30_000_000, &format!("mutex: lock_timeout(30ms) gives up after {} ms", dt / 1_000_000));
    drop(r);
    let a = now();
    drop(st.m.lock());
    t.check(now() - a < 200_000_000, "mutex: lock succeeds after the holder releases");
    holder.join();
    t.check(st.m.try_lock().is_some(), "mutex: try_lock succeeds when free");

    // RwLock: 3 concurrent readers, then a writer that must see none.
    let mut rs = Vec::new();
    for _ in 0..3 {
        let s = st.clone();
        rs.push(kthread::spawn("ktest-rd", move || {
            let g = s.rw.read();
            let n = s.readers_now.fetch_add(1, Ordering::AcqRel) + 1;
            s.readers_max.fetch_max(n, Ordering::AcqRel);
            kthread::msleep(50);
            s.readers_now.fetch_sub(1, Ordering::AcqRel);
            let _ = *g;
        }).unwrap());
    }
    kthread::msleep(10);
    let s = st.clone();
    let w = kthread::spawn("ktest-wr", move || {
        let mut g = s.rw.write();
        s.writer_saw_readers.store(s.readers_now.load(Ordering::Acquire) + 100, Ordering::Release);
        *g += 1;
    }).unwrap();
    for r in &rs { r.join(); }
    w.join();
    t.check(st.readers_max.load(Ordering::Acquire) == 3, &format!("rwlock: readers overlap (max {})", st.readers_max.load(Ordering::Acquire)));
    t.check(st.writer_saw_readers.load(Ordering::Acquire) == 100, "rwlock: writer excluded readers");
    let wv = *st.rw.read();
    let free = st.rw.try_write().is_some();
    t.check(wv == 1 && free, "rwlock: write applied, free afterwards");

    // Semaphore: timeout, then an up() from an IRQ-context timer.
    let a = now();
    let got = st.sem.down_timeout(50_000_000);
    let dt = now() - a;
    t.check(!got && dt >= 50_000_000 && dt < 1_000_000_000, &format!("semaphore: down_timeout(50ms) timed out after {} ms", dt / 1_000_000));
    let tm = HrTimer::new(sem_up_cb, Arc::as_ptr(&st) as usize, TimerMode::Irq);
    let a = now();
    tm.start(30_000_000, 0);
    let got = st.sem.down_timeout(2_000_000_000);
    let dt = now() - a;
    t.check(got && dt >= 30_000_000, &format!("semaphore: up() from IRQ released down() after {} ms", dt / 1_000_000));
    t.check(st.sem_waiters_seen.load(Ordering::Acquire), "semaphore: the waiter was parked (queued) when up() ran");
    st.sem.up(); st.sem.up();
    t.check(st.sem.try_down() && st.sem.try_down() && !st.sem.try_down(), "semaphore: counts");

    // Wait queue: condition set + wake_all from IRQ context, 3 waiters.
    let mut ws = Vec::new();
    for _ in 0..3 {
        let s = st.clone();
        ws.push(kthread::spawn("ktest-wq", move || {
            if s.wq.wait_event_timeout(|| s.flag.load(Ordering::Acquire) == 1, 2_000_000_000) {
                s.woke.fetch_add(1, Ordering::AcqRel);
            }
        }).unwrap());
    }
    kthread::msleep(20);
    t.check(st.wq.has_waiters(), "waitqueue: waiters parked");
    let tm2 = HrTimer::new(wq_set_cb, Arc::as_ptr(&st) as usize, TimerMode::Irq);
    tm2.start(10_000_000, 0);
    for w in &ws { w.join(); }
    t.check(st.woke.load(Ordering::Acquire) == 3, &format!("waitqueue: wake_all from IRQ woke {}", st.woke.load(Ordering::Acquire)));
    let a = now();
    let r = st.wq.wait_event_timeout(|| false, 20_000_000);
    t.check(!r && now() - a >= 20_000_000, "waitqueue: timeout path");

    // Completion.
    let c = Arc::new(KCompletion::new());
    let c2 = c.clone();
    let th = kthread::spawn("ktest-cmp", move || { kthread::msleep(10); c2.complete(); }).unwrap();
    t.check(c.wait_timeout(1_000_000_000), "completion: complete() wakes wait()");
    th.join();
    t.check(!c.wait_timeout(5_000_000), "completion: consumed");
    drop(tm); drop(tm2);
    t
}

// ── kthread ─────────────────────────────────────────────────────────────────

extern "C" fn kt_c_entry(arg: usize) {
    unsafe { (*(arg as *const AtomicU64)).fetch_add(1000, Ordering::AcqRel); }
}

fn t_kthread() -> T {
    let mut t = T::new("kthread");
    let (tasks0, _) = sched::task_census();
    let sum = Arc::new(AtomicU64::new(0));
    let mut ths = Vec::new();
    for i in 1..=16u64 {
        let s = sum.clone();
        match kthread::spawn("ktest-arg", move || { s.fetch_add(i, Ordering::AcqRel); kthread::msleep(5); }) {
            Some(th) => ths.push(th),
            None => t.check(false, "spawn"),
        }
    }
    for th in &ths { t.check(th.join_timeout(2_000_000_000), &format!("join pid {}", th.pid())); }
    t.check(sum.load(Ordering::Acquire) == 136, &format!("16 closures with captured args: sum {}", sum.load(Ordering::Acquire)));
    static CSUM: AtomicU64 = AtomicU64::new(0);
    let pid = sched::kthread::kfacil_kthread_spawn(kt_c_entry, &CSUM as *const AtomicU64 as usize);
    let a = now();
    while CSUM.load(Ordering::Acquire) == 0 && now() - a < 1_000_000_000 { kthread::msleep(1); }
    t.check(pid != 0 && CSUM.load(Ordering::Acquire) == 1000, "C ABI fn+ctx spawn ran");
    let stopper = kthread::spawn("ktest-stop", || { while !kthread::should_stop() { kthread::msleep(2); } }).unwrap();
    kthread::msleep(10);
    t.check(!stopper.finished(), "stoppable thread runs");
    stopper.stop();
    t.check(stopper.finished(), "request_stop + join");
    drop(ths);
    // Exited threads are reaped: the task count returns to where it was.
    let a = now();
    let mut tasks1 = sched::task_census().0;
    while tasks1 > tasks0 && now() - a < 2_000_000_000 { kthread::msleep(10); tasks1 = sched::task_census().0; }
    t.check(tasks1 <= tasks0, &format!("exited kthreads reaped (tasks {} -> {})", tasks0, tasks1));
    let a = now();
    kthread::msleep(25);
    let dt = now() - a;
    t.check(dt >= 25_000_000 && dt < 500_000_000, &format!("msleep(25) slept {} us", dt / 1000));
    t
}

// ── workqueue ───────────────────────────────────────────────────────────────

static WQ_IRQ_RAN: AtomicU32 = AtomicU32::new(0);
extern "C" fn wq_irq_fn(_c: usize) {
    let mut v = 1;
    if !irqctx::in_irq() && irqctx::can_sleep() { v |= 2; }
    if sched::workqueue::system().map(|q| q.worker_pid()) == Some(sched::current_pid()) { v |= 4; }
    kthread::msleep(1); // work may sleep
    WQ_IRQ_RAN.store(v, Ordering::Release);
}
static WQ_IRQ_WORK: sched::workqueue::Work = sched::workqueue::Work::new(wq_irq_fn, 0);
static WQ_QUEUED_FROM_IRQ: AtomicU32 = AtomicU32::new(0);
extern "C" fn wq_irq_timer(_c: usize) {
    let ok = sched::workqueue::queue_work(&WQ_IRQ_WORK);
    WQ_QUEUED_FROM_IRQ.store(if ok { 1 } else { 2 }, Ordering::Release);
}
static WQ_BLOCK: AtomicBool = AtomicBool::new(false);
extern "C" fn wq_block_fn(_c: usize) { while WQ_BLOCK.load(Ordering::Acquire) { kthread::msleep(1); } }
static WQ_BLOCKER: sched::workqueue::Work = sched::workqueue::Work::new(wq_block_fn, 0);
static WQ_NOP_RAN: AtomicU32 = AtomicU32::new(0);
extern "C" fn wq_nop_fn(_c: usize) { WQ_NOP_RAN.fetch_add(1, Ordering::AcqRel); }
static WQ_NOP: sched::workqueue::Work = sched::workqueue::Work::new(wq_nop_fn, 0);

fn t_workqueue() -> T {
    let mut t = T::new("workqueue");
    let q = match sched::workqueue::system() { Some(q) => q, None => { t.check(false, "system workqueue exists"); return t; } };
    let order = Arc::new(KSpinLock::new(Vec::new()));
    for i in 0..100u32 {
        let o = order.clone();
        q.queue_fn(move || o.lock().push(i));
    }
    q.flush();
    let v = order.lock().clone();
    t.check(v.len() == 100 && v.iter().enumerate().all(|(i, &x)| x == i as u32), &format!("100 closures ran in FIFO order before flush returned ({})", v.len()));
    let tm = HrTimer::new(wq_irq_timer, 0, TimerMode::Irq);
    tm.start(5_000_000, 0);
    let a = now();
    while WQ_IRQ_RAN.load(Ordering::Acquire) == 0 && now() - a < 2_000_000_000 { kthread::msleep(2); }
    t.check(WQ_QUEUED_FROM_IRQ.load(Ordering::Acquire) == 1, "queue_work from IRQ context accepted");
    t.check(WQ_IRQ_RAN.load(Ordering::Acquire) == 7, &format!("work ran in the worker thread, task context, could sleep (flags {})", WQ_IRQ_RAN.load(Ordering::Acquire)));
    // Pending semantics + cancel, with the worker held busy.
    WQ_BLOCK.store(true, Ordering::Release);
    q.queue(&WQ_BLOCKER);
    kthread::msleep(5);
    let first = q.queue(&WQ_NOP);
    let second = q.queue(&WQ_NOP);
    t.check(first && !second, "re-queueing a pending item is refused");
    t.check(q.cancel(&WQ_NOP) && !WQ_NOP.is_pending(), "cancel removes a pending item");
    WQ_BLOCK.store(false, Ordering::Release);
    q.flush();
    t.check(WQ_NOP_RAN.load(Ordering::Acquire) == 0, "cancelled item never ran");
    // A private queue: create, use, destroy.
    match sched::workqueue::WorkQueue::new("ktest-wq") {
        Some(pq) => {
            let n = Arc::new(AtomicU32::new(0));
            for _ in 0..10 { let n2 = n.clone(); pq.queue_fn(move || { n2.fetch_add(1, Ordering::AcqRel); }); }
            pq.destroy();
            t.check(n.load(Ordering::Acquire) == 10, "private queue ran its items and shut down");
            t.check(!pq.queue_fn(|| {}), "destroyed queue refuses work");
        }
        None => t.check(false, "WorkQueue::new"),
    }
    t
}

// ── hrtimer ─────────────────────────────────────────────────────────────────

struct HrState { fired_at: AtomicU64, count: AtomicU32, task_ctx: AtomicU32 }
extern "C" fn hr_once(ctx: usize) {
    let s = unsafe { &*(ctx as *const HrState) };
    s.fired_at.store(now(), Ordering::Release);
    s.count.fetch_add(1, Ordering::AcqRel);
}
extern "C" fn hr_worker(ctx: usize) {
    let s = unsafe { &*(ctx as *const HrState) };
    if !irqctx::in_irq() && irqctx::can_sleep() { s.task_ctx.store(1, Ordering::Release); }
    s.count.fetch_add(1, Ordering::AcqRel);
}

fn t_hrtimer() -> T {
    let mut t = T::new("hrtimer");
    let s = Arc::new(HrState { fired_at: AtomicU64::new(0), count: AtomicU32::new(0), task_ctx: AtomicU32::new(0) });
    let ctx = Arc::as_ptr(&s) as usize;
    let mut late = Vec::new();
    for delay_ms in [2u64, 7, 20] {
        s.fired_at.store(0, Ordering::Release);
        let tm = HrTimer::new(hr_once, ctx, TimerMode::Irq);
        let start = now();
        let exp = start + delay_ms * 1_000_000;
        tm.start_at(exp, 0);
        while s.fired_at.load(Ordering::Acquire) == 0 && now() - start < 1_000_000_000 { kthread::msleep(1); }
        let f = s.fired_at.load(Ordering::Acquire);
        t.check(f >= exp, &format!("one-shot {} ms fired not early", delay_ms));
        late.push(f.saturating_sub(exp) / 1000);
        t.check(!tm.is_armed(), "one-shot disarmed after firing");
    }
    say!("hrtimer: one-shot lateness us: {:?}", late);
    t.check(late.iter().all(|&l| l < 15_000), "one-shot lateness under 15 ms (one tick)");
    s.count.store(0, Ordering::Release);
    let p = HrTimer::new(hr_once, ctx, TimerMode::Irq);
    p.start(10_000_000, 10_000_000);
    kthread::msleep(205);
    p.cancel_sync();
    let n = s.count.load(Ordering::Acquire);
    t.check((15..=22).contains(&n), &format!("periodic 10 ms over 205 ms fired {} times", n));
    kthread::msleep(30);
    t.check(s.count.load(Ordering::Acquire) == n, "nothing fires after cancel");
    s.count.store(0, Ordering::Release);
    let c = HrTimer::new(hr_once, ctx, TimerMode::Irq);
    c.start(30_000_000, 0);
    kthread::msleep(5);
    t.check(c.cancel(), "cancel of an armed timer returns true");
    kthread::msleep(40);
    t.check(s.count.load(Ordering::Acquire) == 0, "cancelled one-shot never fired");
    s.count.store(0, Ordering::Release);
    let w = HrTimer::new(hr_worker, ctx, TimerMode::Worker);
    w.start(5_000_000, 0);
    let a = now();
    while s.count.load(Ordering::Acquire) == 0 && now() - a < 1_000_000_000 { kthread::msleep(2); }
    t.check(s.task_ctx.load(Ordering::Acquire) == 1, "worker-mode callback ran in task context");
    t
}

// ── atomic pool ─────────────────────────────────────────────────────────────

static AP_PTR: AtomicUsize = AtomicUsize::new(0);
static AP_OWNED: AtomicU32 = AtomicU32::new(0);
extern "C" fn ap_cb(_c: usize) {
    let b = alloc::boxed::Box::new([7u8; 200]);
    let v: Vec<u32> = (0..64).collect();
    let ok = mm::atomic_pool::owns(&*b as *const [u8; 200] as *mut u8) && mm::atomic_pool::owns(v.as_ptr() as *mut u8);
    AP_OWNED.store(if ok { 1 } else { 2 }, Ordering::Release);
    AP_PTR.store(alloc::boxed::Box::into_raw(b) as usize, Ordering::Release);
    drop(v); // freed back to the pool from IRQ context
}

fn census() -> Vec<(usize, u32, usize)> {
    let mut v = Vec::new();
    mm::atomic_pool::census(&mut |a, b, c| v.push((a, b, c)));
    v
}

fn t_atomicpool() -> T {
    let mut t = T::new("atomicpool");
    let c0 = census();
    t.check(!c0.is_empty() && c0.iter().all(|&(_, f, n)| f as usize <= n), "pool initialised");
    let tm = HrTimer::new(ap_cb, 0, TimerMode::Irq);
    tm.start(2_000_000, 0);
    let a = now();
    while AP_OWNED.load(Ordering::Acquire) == 0 && now() - a < 1_000_000_000 { kthread::msleep(1); }
    t.check(AP_OWNED.load(Ordering::Acquire) == 1, "Box/Vec allocated in IRQ context come from the atomic pool");
    let p = AP_PTR.swap(0, Ordering::AcqRel);
    if p != 0 {
        let b = unsafe { alloc::boxed::Box::from_raw(p as *mut [u8; 200]) };
        t.check(b.iter().all(|&x| x == 7), "IRQ allocation intact in task context");
        drop(b); // global dealloc routes a pool pointer back to the pool
    }
    let c1 = census();
    t.check(c1 == c0, "everything returned to the pool");
    let mut got = Vec::new();
    while let Some(p) = mm::atomic_pool::alloc(4096, 4096) { got.push(p); if got.len() > 64 { break; } }
    t.check(got.len() == 12, &format!("4 KiB class exhausts after its 12 objects ({})", got.len()));
    t.check(got.iter().all(|&p| p as usize % 4096 == 0), "objects naturally aligned");
    let k = mm::atomic_pool::kfacil_kmalloc(4000, 1);
    t.check(!k.is_null(), "atomic kmalloc falls back to a free slab when the pool class is empty");
    unsafe { mm::atomic_pool::kfacil_kfree(k); }
    for p in got { mm::atomic_pool::free(p); }
    t.check(census() == c0, "census back to full");
    let k = mm::atomic_pool::kfacil_kmalloc(100, 1);
    t.check(!k.is_null() && mm::atomic_pool::owns(unsafe { k.sub(16) }), "atomic kmalloc served from the pool");
    unsafe { mm::atomic_pool::kfacil_kfree(k); }
    t
}

// ── kva ─────────────────────────────────────────────────────────────────────

fn t_kmodwin() -> T {
    use mm::kmodwin::{self, Perm};
    let mut t = T::new("kmodwin");
    if !crate::kmod::window_ok() { t.check(false, "module window initialised"); return t; }
    t.check(kmodwin::present_pages() == 0, "window empty at start");
    let base = kmodwin::BASE + kmodwin::TEST_SLOT_OFF;
    t.check(kmodwin::reserve(base, kmodwin::SLOT_ALIGN), "reserve test slot");
    t.check(!kmodwin::reserve(base, kmodwin::SLOT_ALIGN), "double reserve refused");
    t.check(!kmodwin::reserve(base + 1, 4096), "unaligned reserve refused");
    let (rw, rx) = (mm::buddy::alloc(0).unwrap(), mm::buddy::alloc(0).unwrap());
    let (va_rw, va_rx) = (base, base + 4096);
    let root = mm::paging::get_kernel_root();
    unsafe {
        t.check(kmodwin::map(va_rw, rw, Perm::RW), "map RW");
        (va_rw as *mut u64).write_volatile(0xC0DE_F00D_1234_5678);
        let hh = mm::phys_to_virt(rw) as *const u64;
        t.check(hh.read_volatile() == 0xC0DE_F00D_1234_5678, "write through window visible via HHDM");
        (mm::phys_to_virt(rw) as *mut u64).add(1).write_volatile(0x55AA);
        t.check((va_rw as *const u64).add(1).read_volatile() == 0x55AA, "HHDM write visible through window");
        t.check(!kmodwin::map(va_rw, rw, Perm::RW), "double map refused");
        t.check(kmodwin::map(va_rx, rx, Perm::RX), "map RX");
    }
    let p = mm::paging::leaf_pte(root, va_rx);
    #[cfg(target_arch = "x86_64")]
    t.check(p & 1 != 0 && p & 2 == 0 && p & 4 == 0 && p >> 63 == 0, "RX PTE: present, no W, no U, NX clear");
    #[cfg(target_arch = "aarch64")]
    t.check(p & 1 != 0 && p & (1 << 7) != 0 && p & (1 << 6) == 0 && p & (1 << 54) != 0 && p & (1 << 53) == 0,
        "RX PTE: valid, read-only, kernel-only, UXN=1, PXN=0");
    let pw = mm::paging::leaf_pte(root, va_rw);
    #[cfg(target_arch = "x86_64")]
    t.check(pw & 2 != 0 && pw >> 63 == 1, "RW PTE: W set, NX set");
    #[cfg(target_arch = "aarch64")]
    t.check(pw & (1 << 7) == 0 && pw & (1 << 53) != 0 && pw & (1 << 54) != 0, "RW PTE: writable, PXN+UXN");
    t.check(kmodwin::present_pages() == 2, "present_pages counts both");
    unsafe { kmodwin::unmap(va_rw, 2); }
    t.check(kmodwin::present_pages() == 0, "present_pages back to 0 after unmap");
    kmodwin::release(base, kmodwin::SLOT_ALIGN);
    t.check(kmodwin::reserve(base, kmodwin::SLOT_ALIGN), "slot reusable after release");
    kmodwin::release(base, kmodwin::SLOT_ALIGN);
    mm::buddy::free(rw, 0); mm::buddy::free(rx, 0);
    t
}

static KAPI_RNG_CALLS: AtomicU32 = AtomicU32::new(0);
extern "C" fn kapi_fake_rng(p: *mut u8, n: usize) -> usize {
    KAPI_RNG_CALLS.fetch_add(1, Ordering::SeqCst);
    let n = n.min(8);
    for i in 0..n { unsafe { p.add(i).write(0xa5); } }
    n
}

/// L5a: the PCI / memory / entropy kapi exports, called through the KSYMTAB addresses
/// (exactly what a module's relocations resolve to).
fn t_kmodkapi() -> T {
    use crate::kmod::{exports, registry};
    use kmod_abi::{sys, KPciDev, KmodId, RegKind, KMAP_MMIO, KMAP_WRITABLE};
    let mut t = T::new("kmodkapi");
    macro_rules! sym { ($n:literal, $ty:ty) => {{
        let a = exports::lookup($n);
        t.check(a.is_some(), concat!($n, " in KSYMTAB"));
        match a { Some(a) => Some(unsafe { core::mem::transmute::<usize, $ty>(a) }), None => None }
    }}; }
    t.check(exports::ksymtab().len() == 83 && exports::NEXPORTS == 83, "ksymtab has 83 exports");
    {
        use kmod_abi::{KBlkDesc, KBlkOps};
        let r = crate::kmod::exports::kapi_blk_register(KmodId(0), core::ptr::null::<KBlkDesc>(), core::ptr::null::<KBlkOps>(), 0);
        t.check(r == -22, "kapi_blk_register null desc/ops -> -22");
        t.check(crate::kmod::exports::kapi_blk_remove(-1) == -22, "kapi_blk_remove(-1) -> -22");
    }
    let find = sym!("kapi_pci_find", sys::PciFindFn);
    let rd = sym!("kapi_pci_cfg_read", sys::PciCfgReadFn);
    let wr = sym!("kapi_pci_cfg_write", sys::PciCfgWriteFn);
    let map = sym!("kapi_map_device", sys::MapDeviceFn);
    let p2v = sym!("kapi_phys_to_virt", sys::PhysToVirtFn);
    let pa = sym!("kapi_page_alloc", sys::PageAllocFn);
    let pf = sym!("kapi_page_free", sys::PageFreeFn);
    let hw = sym!("kapi_hwrng_register", sys::HwrngRegisterFn);
    let (Some(find), Some(rd), Some(wr), Some(map), Some(p2v), Some(pa), Some(pf), Some(hw)) =
        (find, rd, wr, map, p2v, pa, pf, hw) else { return t };

    unsafe {
        // PCI: virtio-rng (QEMU provides it on the default machine); any 1af4 device otherwise.
        let mut d = KPciDev::default();
        let mut rc = find(0x1af4, 0x1044, 0, &mut d);
        if rc != 0 { rc = find(0x1af4, 0xffff, 0, &mut d); }
        t.check(find(0x1af4, 0x1044, 0, core::ptr::null_mut()) == -22, "pci_find null out -> -22");
        let mut none = KPciDev::default();
        t.check(find(0xdead, 0xbeef, 0, &mut none) == -19, "pci_find no match -> -19");
        if rc == 0 {
            t.check(d.vendor == 0x1af4, "pci_find vendor");
            t.check(rd(d.bdf, 0, 2) == 0x1af4, "cfg_read vendor via bdf (16-bit)");
            t.check(rd(d.bdf, 0, 4) == (d.device as u32) << 16 | 0x1af4, "cfg_read id dword (32-bit)");
            t.check(rd(d.bdf, 0, 1) == 0xf4, "cfg_read byte");
            t.check(rd(d.bdf, 0, 3) == u32::MAX && rd(d.bdf, 0x100, 2) == u32::MAX, "bad width / offset -> all ones");
            let cmd = rd(d.bdf, 4, 2);
            wr(d.bdf, 4, 2, cmd); // write-back of the same value must be harmless
            t.check(rd(d.bdf, 4, 2) == cmd, "cfg_write command round trip");
            let bar = d.bars.iter().copied().find(|b| *b != 0 && b & 1 == 0);
            if let Some(b) = bar {
                let phys = (b & !0xf) as u64;
                let v = map(phys, 0x1000, KMAP_WRITABLE | KMAP_MMIO);
                t.check(v == 0 || v == p2v(phys), "map_device returns the HHDM alias or 0");
            } else { say!("kmodkapi: no 32-bit memory BAR on the device, map_device skipped"); }
        } else { t.skip("no 1af4 PCI device in this VM"); }
        t.check(map(0x1000, 0, KMAP_WRITABLE) == 0, "map_device len 0 -> 0");

        // Pages
        let p = pa(0, 1 << 32);
        t.check(p != 0 && p < (1 << 32) && p & 0xfff == 0, "page_alloc(0, below 4G) < 4G");
        if p != 0 {
            let v = p2v(p) as *mut u32;
            v.write_volatile(0x5eed_1234);
            t.check(v.read_volatile() == 0x5eed_1234, "page_alloc page is writable through phys_to_virt");
            pf(p, 0);
        }
        let q = pa(0, 0);
        t.check(q != 0, "page_alloc(0, any)");
        if q != 0 { pf(q, 0); }
        t.check(pa(11, 0) == 0, "page_alloc order 11 refused");

        // Hardware RNG registration reseeds at once.
        let owner = KmodId(60001);
        let name = b"ktest-hwrng";
        KAPI_RNG_CALLS.store(0, Ordering::SeqCst);
        t.check(hw(owner, core::ptr::null(), 0, kapi_fake_rng) == -22, "hwrng_register empty name -> -22");
        let r = hw(owner, name.as_ptr(), name.len(), kapi_fake_rng);
        t.check(r == 0, "hwrng_register ok");
        let mut c = KAPI_RNG_CALLS.load(Ordering::SeqCst);
        if c == 0 { sched::random::reseed_now(); c = KAPI_RNG_CALLS.load(Ordering::SeqCst); }
        t.check(c >= 1, &format!("late source was read by the reseed (calls {})", c));
        let mut seen = false;
        registry::registrations(owner, &mut |k, _| if k == RegKind::HWRNG { seen = true });
        t.check(seen, "HWRNG registration recorded for the owner");
    }
    kmodkapi_l6a(&mut t);
    kmodkapi_l7a(&mut t);
    kmodkapi_w2(&mut t);
    t
}

static KT_WAKES: AtomicU32 = AtomicU32::new(0);
static KT_NIF_SENT: AtomicU32 = AtomicU32::new(0);
static KT_PM: AtomicU32 = AtomicU32::new(0);

extern "C" fn kt_irq(_c: usize) -> u32 { 0 }
extern "C" fn kt_nsend(_c: usize, _b: *const u8, len: usize) -> bool { KT_NIF_SENT.fetch_add(len as u32, Ordering::SeqCst); true }
extern "C" fn kt_ntrue(_c: usize) -> bool { true }
extern "C" fn kt_nfalse(_c: usize) -> bool { false }
extern "C" fn kt_nrecv(_c: usize, _b: *mut u8, _cap: usize) -> i32 { -1 }
extern "C" fn kt_nmac(_c: usize, o: *mut u8) -> bool { unsafe { core::ptr::copy_nonoverlapping([2u8, 0x4b, 0x54, 0, 0, 1].as_ptr(), o, 6); } true }
extern "C" fn kt_nlink(_c: usize, up: *mut bool, mbps: *mut u32, full: *mut bool) { unsafe { *up = true; *mbps = 1000; *full = true; } }
extern "C" fn kt_ngen(_c: usize) -> u32 { 7 }
fn kt_put(s: &str, b: *mut u8, cap: usize) -> usize { let n = s.len().min(cap); unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), b, n); } n }
extern "C" fn kt_ninfo(_c: usize, b: *mut u8, cap: usize) -> usize { kt_put("driver: ktest\n", b, cap) }
extern "C" fn kt_nname(_c: usize, b: *mut u8, cap: usize) -> usize { kt_put("kt-nic", b, cap) }
extern "C" fn kt_pm_ok(c: usize, b: *mut u8, cap: usize) -> i32 { KT_PM.fetch_add(c as u32, Ordering::SeqCst); kt_put("pm ok", b, cap) as i32 }
extern "C" fn kt_pm_err(_c: usize, b: *mut u8, cap: usize) -> i32 { -(kt_put("pm bad", b, cap) as i32) }

/// L6a: PCI enable/caps, IRQ errors, wait queue, late netdev registration, pm hooks.
static KT_TICKS: AtomicUsize = AtomicUsize::new(0);
extern "C" fn kt_tick(ctx: usize) { KT_TICKS.fetch_add(ctx, Ordering::SeqCst); }
static KT_W2_TICKS: AtomicUsize = AtomicUsize::new(0);
extern "C" fn kt_w2_tick(ctx: usize) { KT_W2_TICKS.fetch_add(ctx, Ordering::SeqCst); }
extern "C" fn kt_probe_no(_i: *const kmod_abi::KUsbIf, _ctx: usize) -> i32 { 1 }
extern "C" fn kt_cinfo(_c: usize, o: *mut kmod_abi::KCamInfo) -> i32 {
    unsafe { *o = kmod_abi::KCamInfo { bcd: 0x0110, slot: 200, nformats: 1 }; } 0
}
extern "C" fn kt_cformat(_c: usize, i: u32, o: *mut kmod_abi::KFormat) -> i32 {
    if i != 0 { return -22; }
    unsafe { *o = kmod_abi::KFormat { index: 1, kind: kmod_abi::KFMT_MJPEG, bpp: 0, default_frame: 1, nframes: 1 }; } 0
}
extern "C" fn kt_cframe(_c: usize, f: u32, j: u32, o: *mut kmod_abi::KFrame) -> i32 {
    if f != 0 || j != 0 { return -22; }
    unsafe { *o = kmod_abi::KFrame { index: 1, _pad: 0, width: 320, height: 240, _pad2: 0, max_size: 65536, interval: 333333 }; } 0
}
extern "C" fn kt_cstart(_c: usize, _f: u8, _fr: u8, _i: u32, _b: *const kmod_abi::KCamBuf, _n: u32) -> i32 { 0 }
extern "C" fn kt_cstop(_c: usize) {}
extern "C" fn kt_cpoll(_c: usize) -> u32 { 0 }
extern "C" fn kt_cready(_c: usize) -> u32 { 0 }
extern "C" fn kt_cgone(_c: usize) -> bool { false }
extern "C" fn kt_cq(_c: usize, _i: u32) -> i32 { 0 }
extern "C" fn kt_cdq(_c: usize, _o: *mut kmod_abi::KCamDone) -> i32 { -11 }
extern "C" fn kt_cctl(_c: usize, _id: u32, _s: bool, _v: u32, _o: *mut u32) -> i32 { -22 }
static KT_CAM_OPS: kmod_abi::KCameraOps = kmod_abi::KCameraOps {
    info: kt_cinfo, format: kt_cformat, frame: kt_cframe, stream_start: kt_cstart, stream_stop: kt_cstop,
    stream_poll: kt_cpoll, stream_ready: kt_cready, stream_gone: kt_cgone, stream_qbuf: kt_cq,
    stream_dqbuf: kt_cdq, control: kt_cctl,
};

/// L7a: camera registration, USB class registration/matching and tick callbacks through the kapi.
extern "C" fn kt_pm_w2(_: usize, _: *mut u8, _: usize) -> i32 { 0 }

/// kmodW2 W2-1: failed-init cleanup for every callback kind, virt_to_phys, pm2 ordering.
fn kmodkapi_w2(t: &mut T) {
    use crate::kmod::{exports, registry};
    use kmod_abi::{pm_phase, sys, KPmOps, KmodId, RegKind};
    let owner = KmodId(60010);
    unsafe {
        // tick: a failed init must take its callback back and leave no record.
        let treg: sys::TickRegisterFn = core::mem::transmute(exports::lookup("kapi_tick_register").unwrap());
        let tc = usb::class::tick_count();
        t.check(treg(owner, kt_w2_tick, 5) == 0, "w2: tick_register ok");
        t.check(registry::reg_count(owner) == 1, "w2: tick registration recorded");
        t.check(registry::release_all(owner), "w2: failed init releases a tick callback");
        t.check(usb::class::tick_count() == tc && registry::reg_count(owner) == 0, "w2: tick callback gone, record gone");
        let b = KT_W2_TICKS.load(Ordering::SeqCst);
        usb::class::tick();
        t.check(KT_W2_TICKS.load(Ordering::SeqCst) == b, "w2: released tick callback no longer runs");

        // pm: two owners can register (a list), each removed on its own failed init.
        let preg: sys::PmRegisterFn = core::mem::transmute(exports::lookup("kapi_pm_register").unwrap());
        let preg2: sys::PmRegister2Fn = core::mem::transmute(exports::lookup("kapi_pm_register2").unwrap());
        let other = KmodId(60011);
        t.check(preg(owner, Some(kt_pm_w2), Some(kt_pm_w2), None, 1) == 0, "w2: pm_register ok");
        t.check(preg(other, Some(kt_pm_w2), None, None, 2) == 0, "w2: second pm_register ok (list, no -16)");
        let ops = KPmOps { size: core::mem::size_of::<KPmOps>() as u32, phase: pm_phase::GPU, suspend: Some(kt_pm_w2),
                           resume: None, freeze: None, thaw: None, shutdown: Some(kt_pm_w2), kexec_quiesce: None, resume_check: None };
        t.check(preg2(owner, &ops, 3) == 0, "w2: pm_register2 ok");
        t.check(preg2(owner, core::ptr::null(), 3) == -22, "w2: pm_register2 null -> -22");
        let bad = KPmOps { phase: 99, ..ops };
        t.check(preg2(owner, &bad, 3) == -22, "w2: pm_register2 bad phase -> -22");
        t.check(exports::pm_count(owner) == 2 && exports::pm_count(other) == 1, "w2: pm records per owner");
        t.check(registry::release_all(owner), "w2: failed init releases pm hooks");
        t.check(exports::pm_count(owner) == 0 && exports::pm_count(other) == 1, "w2: only the failing owner's hooks removed");
        t.check(registry::release_all(other) && exports::pm_count(other) == 0, "w2: second owner released");

        // v4l2: a failed init unregisters the camera.
        let vreg: sys::V4l2RegisterFn = core::mem::transmute(exports::lookup("kapi_v4l2_register").unwrap());
        let n0 = crate::syscall::v4l2::camera_count();
        t.check(vreg(owner, &KT_CAM_OPS, 0x7e58) == 0, "w2: v4l2_register ok");
        t.check(registry::release_all(owner) && crate::syscall::v4l2::camera_count() == n0, "w2: failed init releases the camera");

        // kinds with no unregister path keep the slot mapped (leak-safe), and the record stays.
        for k in [RegKind::IRQ, RegKind::NETDEV, RegKind::HWRNG, RegKind::KTHREAD, RegKind::USB_CLASS] {
            registry::note_registration(owner, k, 1);
        }
        t.check(!registry::release_all(owner) && registry::reg_count(owner) == 5, "w2: irq/netdev/hwrng/kthread/usb_class stay recorded");
        registry::remove(owner);

        // virt_to_phys: HHDM yes, module window and kernel statics no.
        static PROBE: u8 = 0;
        let phys = mm::virt_to_phys(core::ptr::addr_of!(KT_TICKS) as usize);
        let _ = phys;
        let v2p: sys::VirtToPhysFn = core::mem::transmute(exports::lookup("kapi_virt_to_phys").unwrap());
        t.check(v2p(mm::kmodwin::BASE + 0x1000) == u64::MAX, "w2: virt_to_phys refuses a module-window address");
        t.check(v2p(0) == u64::MAX, "w2: virt_to_phys refuses a low address");
        let page = mm::phys_to_virt(0x1000);
        t.check(v2p(page) == 0x1000, "w2: virt_to_phys of an HHDM address");
        let _ = &PROBE;
    }
    // W2-1 session 2: the remaining common exports (bad arguments and a few real round trips).
    unsafe {
        let f = |n: &str| exports::lookup(n).unwrap_or_else(|| { say!("missing export {}", n); 0 });
        t.check(f("kapi_ioremap") != 0 && f("kapi_proc_prime") != 0, "w2b: exports present");
        let ior: sys::IoremapFn = core::mem::transmute(f("kapi_ioremap"));
        t.check(ior(0xfee0_0000, 0, 0) == 0 && ior(0xfee0_0000, 4096, 9) == 0, "w2b: ioremap zero len / bad cache refused");
        let mnew: sys::MutexNewFn = core::mem::transmute(f("kapi_mutex_new"));
        let mlock: sys::WaitNotifyFn = core::mem::transmute(f("kapi_mutex_lock"));
        let munlock: sys::WaitNotifyFn = core::mem::transmute(f("kapi_mutex_unlock"));
        mlock(0); munlock(0);
        let m = mnew(b"kt".as_ptr(), 2);
        t.check(m != 0, "w2b: mutex_new");
        mlock(m); munlock(m);
        let snew: sys::SemNewFn = core::mem::transmute(f("kapi_sem_new"));
        let sdown: sys::WaitTimeoutFn = core::mem::transmute(f("kapi_sem_down_timeout"));
        let sup: sys::WaitNotifyFn = core::mem::transmute(f("kapi_sem_up"));
        t.check(!sdown(0, 1000), "w2b: sem_down_timeout on a null handle is false");
        let sm = snew(1);
        t.check(sdown(sm, 0) && !sdown(sm, 1000), "w2b: sem takes its one permit then times out");
        sup(sm);
        t.check(sdown(sm, 0), "w2b: sem_up gives the permit back");
        let hnew: sys::HrtimerNewFn = core::mem::transmute(f("kapi_hrtimer_new"));
        t.check(hnew(None, 0, true) == 0, "w2b: hrtimer_new null callback refused");
        let rf: sys::RandomFillFn = core::mem::transmute(f("kapi_random_fill"));
        rf(core::ptr::null_mut(), 8);
        let mut rb = [0u8; 32];
        rf(rb.as_mut_ptr(), 32);
        t.check(rb.iter().any(|b| *b != 0), "w2b: random_fill fills");
        let cfu: sys::CopyFromUserFn = core::mem::transmute(f("kapi_copy_from_user"));
        let ctu: sys::CopyToUserFn = core::mem::transmute(f("kapi_copy_to_user"));
        t.check(cfu(core::ptr::null_mut(), 0x1000, 4) == -22 && ctu(0x1000, core::ptr::null(), 4) == -22, "w2b: copy_user null kernel side is -EINVAL");
        t.check(cfu(rb.as_mut_ptr(), usize::MAX - 8, 16) == -14 && ctu(0, rb.as_ptr(), 4) == -14, "w2b: copy_user bad user range is -EFAULT");
        let scan: sys::PciScanFn = core::mem::transmute(f("kapi_pci_scan"));
        let mut d = core::mem::zeroed::<kmod_abi::KPciDev>();
        t.check(scan(0, core::ptr::null_mut()) == -22 && scan(u32::MAX, &mut d) == -19, "w2b: pci_scan bad out / past the end");
        let msix: sys::IrqRequestMsixFn = core::mem::transmute(f("kapi_irq_request_msix"));
        extern "C" fn nop(_: usize) -> u32 { 0 }
        t.check(msix(KmodId(60011), 0x00ff_0000, 0, nop, 0) < 0, "w2b: irq_request_msix on a missing device fails");
        let fwr: sys::FwRequestFn = core::mem::transmute(f("kapi_fw_request"));
        let (mut n, mut p) = (0usize, core::ptr::null::<u8>());
        t.check(fwr(core::ptr::null(), 0, 0, &mut n, &mut p) == 0 && fwr(b"x".as_ptr(), 1, 0, core::ptr::null_mut(), &mut p) == 0, "w2b: fw_request bad args");
        let sp: sys::SecretPeekFn = core::mem::transmute(f("kapi_secret_peek"));
        let mut sb = [0u8; 8];
        t.check(sp(KmodId(60012), b"wifi".as_ptr(), 4, sb.as_mut_ptr(), 8) == -1, "w2b: secret_peek refused without a netdev registration");
        let ees: sys::ExtEventStatsFn = core::mem::transmute(f("kapi_ext_event_stats"));
        let mut o = [0u64; 2];
        t.check(ees(core::ptr::null_mut()) == -22 && ees(o.as_mut_ptr()) == 0, "w2b: ext_event_stats");
        let pp: sys::ProcPrimeFn = core::mem::transmute(f("kapi_proc_prime"));
        t.check(pp(core::ptr::null_mut(), 8) == -22 && pp(sb.as_mut_ptr(), 0) <= 0, "w2b: proc_prime bad args");
    }
    // pm2: suspend order is INPUT, GPU, NET, BLOCK, USB, PCI.
    let steps = crate::pm2::suspend_steps();
    let pos = |n: &str| steps.iter().position(|s| s.name == n);
    t.check(matches!((pos("i915"), pos("e1000e"), pos("block"), pos("xhci"), pos("pci")),
        (Some(a), Some(b), Some(c), Some(d), Some(e)) if a < b && b < c && c < d && d < e), "w2: pm2 suspend order i915<e1000e<block<xhci<pci");
}

fn kmodkapi_l7a(t: &mut T) {
    use crate::kmod::exports;
    use kmod_abi::{sys, KmodDevId, KmodId, BUS_PCI, BUS_USB};
    macro_rules! sym { ($n:literal, $ty:ty) => {{
        let a = exports::lookup($n);
        t.check(a.is_some(), concat!($n, " in KSYMTAB"));
        match a { Some(a) => unsafe { core::mem::transmute::<usize, $ty>(a) }, None => return }
    }}; }
    let vreg = sym!("kapi_v4l2_register", sys::V4l2RegisterFn);
    let treg = sym!("kapi_tick_register", sys::TickRegisterFn);
    let creg = sym!("kapi_usb_class_register", sys::UsbClassRegisterFn);
    let owner = KmodId(60003);
    unsafe {
        // v4l2: bad table, then a good one that must show up (and go away again).
        t.check(vreg(owner, core::ptr::null(), 1) == -22, "v4l2_register null ops -> -22");
        let n0 = crate::syscall::v4l2::camera_count();
        t.check(vreg(owner, &KT_CAM_OPS, 0x7e57) == 0, "v4l2_register ok");
        t.check(crate::syscall::v4l2::camera_count() == n0 + 1, "camera count +1");
        let mut seen = false;
        crate::kmod::registry::registrations(owner, &mut |k, _| if k == kmod_abi::RegKind::V4L2 { seen = true });
        t.check(seen, "V4L2 registration recorded");
        crate::syscall::v4l2::unregister_camera(0x7e57);
        t.check(crate::syscall::v4l2::camera_count() == n0, "camera count restored after unregister");

        // tick callbacks run on every usb::class::tick.
        let tc = usb::class::tick_count();
        t.check(treg(owner, kt_tick, 3) == 0, "tick_register ok");
        t.check(usb::class::tick_count() == tc + 1, "tick count +1");
        let b = KT_TICKS.load(Ordering::SeqCst);
        usb::class::tick();
        t.check(KT_TICKS.load(Ordering::SeqCst) == b + 3, "tick callback ran once with its ctx");

        // usb class: argument checks, then a vendor-class driver that matches a synthetic config.
        let id = |bus, a, b, class, mask| KmodDevId { bus, a, b, c: 0, class, class_mask: mask };
        let pci_only = [id(BUS_PCI, 1, 2, 0, 0)];
        t.check(creg(owner, core::ptr::null(), 1, kt_probe_no, None, 0) == -22, "usb_class_register null ids -> -22");
        t.check(creg(owner, pci_only.as_ptr(), 1, kt_probe_no, None, 0) == -22, "usb_class_register without a USB id -> -22");
        // device descriptor stand-in: one interface, class 0xDE sub 0xAD proto 0xBE
        let cfg: [u8; 9] = [9, 4, 0, 0, 0, 0xDE, 0xAD, 0xBE, 0];
        let uvc_cfg: [u8; 9] = [9, 4, 1, 0, 0, 0x0E, 0x02, 0x00, 0];
        t.check(usb::class::wants(&cfg, 0xdead, 0xbeef).is_none(), "vendor interface unwanted before register");
        #[cfg(not(feature = "mod-usb-video-class"))]
        t.check(usb::class::wants(&uvc_cfg, 0x1234, 0x5678) == Some(0x0E), "built-in table wants video streaming");
        #[cfg(feature = "mod-usb-video-class")]
        t.check(usb::class::wants(&uvc_cfg, 0x1234, 0x5678).is_none(), "no built-in video driver: video streaming unwanted");
        t.check(usb::class::wants(&[9, 4, 0, 0, 0, 0x0E, 0x01, 0, 0], 1, 2).is_none(), "video control alone unwanted");
        let ids = [id(BUS_USB, 0xdead, 0xbeef, 0xDE_AD_00, 0xFF_FF_00)];
        t.check(creg(owner, ids.as_ptr(), 1, kt_probe_no, None, 0) == 0, "usb_class_register ok");
        t.check(usb::class::wants(&cfg, 0xdead, 0xbeef) == Some(0xDE), "registered driver wants its interface");
        t.check(usb::class::wants(&cfg, 0xdead, 0x0001).is_none(), "pid mismatch unwanted");
        let mut seen = false;
        crate::kmod::registry::registrations(owner, &mut |k, _| if k == kmod_abi::RegKind::USB_CLASS { seen = true });
        t.check(seen, "USB_CLASS registration recorded");
        // L7b: handle-based USB calls reject handles nobody owns.
        let (uctrl, ualt, ucfg) = (sym!("kapi_usb_ctrl", sys::UsbCtrlFn), sym!("kapi_usb_set_alt", sys::UsbSetAltFn),
                                   sym!("kapi_usb_cfg", sys::UsbCfgFn));
        let (uopen, usub, urap, ucls) = (sym!("kapi_usb_isoch_open", sys::UsbIsochOpenFn), sym!("kapi_usb_isoch_submit", sys::UsbIsochSubmitFn),
                                         sym!("kapi_usb_isoch_reap", sys::UsbIsochReapFn), sym!("kapi_usb_isoch_close", sys::UsbIsochCloseFn));
        let setup = kmod_abi::KUsbSetup { request_type: 0x80, request: 6, value: 0x0100, index: 0, length: 4 };
        let mut b4 = [0u8; 4];
        t.check(unsafe { uctrl(0xdead, &setup, b4.as_mut_ptr(), 4, 100) } == -19, "usb_ctrl bad handle -> -19");
        t.check(unsafe { uctrl(0xdead, core::ptr::null(), b4.as_mut_ptr(), 4, 100) } == -22, "usb_ctrl null setup -> -22");
        t.check(unsafe { uctrl(0xdead, &setup, b4.as_mut_ptr(), 2, 100) } == -22, "usb_ctrl short buffer -> -22");
        t.check(unsafe { ualt(0xdead, 1, 1) } == -19, "usb_set_alt bad handle -> -19");
        t.check(unsafe { ucfg(0xdead, b4.as_mut_ptr(), 4) } == -19, "usb_cfg bad handle -> -19");
        t.check(unsafe { uopen(0xdead, 1, 1, 0x81, 4096, 64, true) } == -19, "usb_isoch_open bad handle -> -19");
        t.check(unsafe { usub(0xdead) } == 1, "usb_isoch_submit bad pipe -> busy");
        t.check(unsafe { urap(0xdead, b4.as_mut_ptr(), 4) } == -11, "usb_isoch_reap bad pipe -> -11");
        unsafe { ucls(0xdead) };
        t.check(exports::usb_claimed() == 0 && exports::usb_pipes() == 0, "no handles leaked");
    }
}

fn kmodkapi_l6a(t: &mut T) {
    use crate::kmod::exports;
    use kmod_abi::{sys, KNetifOps, KPciDev, KmodId};
    macro_rules! sym { ($n:literal, $ty:ty) => {{
        let a = exports::lookup($n);
        t.check(a.is_some(), concat!($n, " in KSYMTAB"));
        match a { Some(a) => unsafe { core::mem::transmute::<usize, $ty>(a) }, None => return }
    }}; }
    let find = sym!("kapi_pci_find", sys::PciFindFn);
    let enable = sym!("kapi_pci_enable", sys::PciEnableFn);
    let cap = sym!("kapi_pci_find_cap", sys::PciFindCapFn);
    let msi = sym!("kapi_irq_request_msi", sys::IrqRequestMsiFn);
    let line = sym!("kapi_irq_request_line", sys::IrqRequestLineFn);
    let wnew = sym!("kapi_wait_new", sys::WaitNewFn);
    let wto = sym!("kapi_wait_timeout", sys::WaitTimeoutFn);
    let wnot = sym!("kapi_wait_notify", sys::WaitNotifyFn);
    let nreg = sym!("kapi_netdev_register", sys::NetdevRegisterFn);
    let kick = sym!("kapi_netdev_kick", sys::NetdevKickFn);
    let sreg = sym!("kapi_snd_register", sys::SndRegisterFn);
    let kirq = sym!("kapi_netdev_kick_from_irq", sys::NetdevKickFn);
    let pmr = sym!("kapi_pm_register", sys::PmRegisterFn);
    let owner = KmodId(60002);
    unsafe {
        // PCI: an Intel/virtio function has a PM or PCIe capability on most machines; at least
        // the capability list walk must agree with the config-space status bit.
        let mut d = KPciDev::default();
        let mut rc = find(0x8086, 0xffff, 0, &mut d);
        if rc != 0 { rc = find(0x1af4, 0xffff, 0, &mut d); }
        if rc == 0 {
            t.check(enable(d.bdf, true) == 0, "pci_enable ok");
            t.check(kapi_cfg(d.bdf, 4) & 6 == 6, "pci_enable set memory + bus master");
            let has_list = kapi_cfg(d.bdf, 6) & 0x10 != 0;
            let c = cap(d.bdf, 0x05);
            let p = cap(d.bdf, 0x01);
            t.check(has_list || (c == 0 && p == 0), "find_cap consistent with the capability-list bit");
            t.check(cap(d.bdf, 0x7e) == 0, "find_cap absent id -> 0");
            let l = line(owner, d.bdf, kt_irq, 0);
            t.check(l >= -95 && l != -22, &format!("irq_request_line returns a line or an errno ({})", l));
        } else { t.skip("no PCI device for pci_enable"); }
        t.check(enable(0x00ff_ff00 | 7, false) == -19, "pci_enable absent function -> -19");
        t.check(cap(0x00ff_ff00 | 7, 0x01) == 0, "find_cap absent function -> 0");
        t.check(msi(owner, 0x00ff_ff00 | 7, kt_irq, 0) < 0, "irq_request_msi absent function fails");

        // Wait queue: timeout expires, a notify before the wait is seen by it, a notify during wakes it.
        let w = wnew();
        t.check(w != 0, "wait_new handle");
        let t0 = sched::monotonic_ns();
        t.check(!wto(w, 20_000_000), "wait_timeout expires without an event");
        t.check(sched::monotonic_ns() - t0 >= 15_000_000, "wait_timeout slept");
        wnot(w);
        t.check(wto(w, 1_000_000), "event before the wait is delivered");
        t.check(!wto(w, 1_000_000), "event is consumed once");
        KT_WAKES.store(0, Ordering::SeqCst);
        let h = w;
        let th = kthread::spawn("kt-wait", move || { sched::kthread::msleep(30); wnot(h); KT_WAKES.store(1, Ordering::SeqCst); });
        t.check(th.is_some(), "spawn notifier");
        t.check(wto(w, 2_000_000_000), "notify from another thread wakes the waiter");
        wnot(0);
        t.check(!wto(0, 1), "wait on a null handle -> false");

        // Netdev: register after boot, daemon-visible through the registry.
        let ops: &'static KNetifOps = alloc::boxed::Box::leak(alloc::boxed::Box::new(KNetifOps {
            send: kt_nsend, tx_ready: kt_ntrue, recv: kt_nrecv, rx_rearm: kt_nfalse, rx_irq_driven: kt_nfalse,
            mac: kt_nmac, link: kt_nlink, link_gen: kt_ngen, info: kt_ninfo, driver_name: kt_nname,
        }));
        t.check(nreg(owner, 0, b"Bad".as_ptr(), 3, ops, 0) == -22, "netdev_register bad prefix -> -22");
        t.check(nreg(owner, 0, b"ktn".as_ptr(), 3, core::ptr::null(), 0) == -22, "netdev_register null ops -> -22");
        { t.check(sreg(owner, core::ptr::null(), 0) == -22, "snd_register null ops -> -22"); }
        let before = drivers::netdev::count();
        let id = nreg(owner, 0, b"ktn".as_ptr(), 3, ops, 5);
        if id < 0 { say!("kmodkapi: netdev_register -> {} (table full?)", id); }
        else {
            t.check(drivers::netdev::count() == before + 1, "netdev count +1");
            let n = drivers::netdev::get(id as usize);
            t.check(n.map_or(false, |n| n.name() == "ktn0" && n.driver() == "kt-nic"), "netif ktn0 / driver kt-nic");
            if let Some(n) = n {
                let b = KT_NIF_SENT.load(Ordering::SeqCst); // the net daemon may send (DHCP) on it too
                t.check(n.send(&[0u8; 60]) && KT_NIF_SENT.load(Ordering::SeqCst).wrapping_sub(b) >= 60, "send goes through the module op");
                t.check(n.mac() == Some([2, 0x4b, 0x54, 0, 0, 1]), "mac");
                t.check(n.link() == (true, 1000, true) && n.link_generation() == 7, "link + link_gen");
                t.check(n.recv(&mut [0u8; 64]).is_none() && n.tx_ready() && !n.rx_irq_driven(), "recv none / tx_ready / rx_irq");
                let mut s = String::new(); n.info_lines(&mut s);
                t.check(s.contains("driver: ktest"), "info lines");
            }
            let mut seen = false;
            crate::kmod::registry::registrations(owner, &mut |k, _| if k == kmod_abi::RegKind::NETDEV { seen = true });
            t.check(seen, "NETDEV registration recorded");
        }
        kick(); kirq();
        t.check(true, "netdev kick / kick_from_irq callable");

        // PM hooks
        t.check(exports::pm_count(owner) == 0, "no pm hooks before register");
        t.check(pmr(owner, Some(kt_pm_ok), Some(kt_pm_err), None, 3) == 0, "pm_register ok");
        t.check(pmr(owner, None, None, None, 0) == 0, "pm_register twice -> ok (hooks are a list)");
        let mine = exports::pm_mods(kmod_abi::pm_phase::NET).into_iter().find(|m| m.owner == owner && m.ops.suspend.is_some());
        t.check(mine.is_some(), "pm hook table listed in the NET phase");
        if let Some(m) = mine {
            t.check(matches!(exports::pm_run(&m, exports::PmOp::Suspend), Some(Ok(ref s)) if s == "pm ok") && KT_PM.load(Ordering::SeqCst) == 3, "pm suspend hook ok");
            t.check(matches!(exports::pm_run(&m, exports::PmOp::Resume), Some(Err(ref s)) if s == "pm bad"), "pm resume hook error");
            t.check(exports::pm_run(&m, exports::PmOp::Shutdown).is_none(), "absent shutdown hook -> None");
        }
        crate::kmod::registry::release_all(owner);
        t.check(exports::pm_count(owner) == 0, "pm hooks released with the owner");
    }
}

fn kapi_cfg(bdf: u32, off: u16) -> u32 {
    let f = exports_cfg();
    unsafe { f(bdf, off, 2) }
}
fn exports_cfg() -> kmod_abi::sys::PciCfgReadFn {
    unsafe { core::mem::transmute::<usize, kmod_abi::sys::PciCfgReadFn>(crate::kmod::exports::lookup("kapi_pci_cfg_read").unwrap_or(0)) }
}

fn t_kmodload() -> T {
    use crate::kmod::{self, exports, registry, KmodErr, Source};
    use kmod_elf::ElfErr;
    use mm::kmodwin;
    let mut t = T::new("kmodload");
    if !kmod::window_ok() { t.check(false, "module window initialised"); return t; }
    #[cfg(target_arch = "x86_64")]
    const M: u16 = 62;
    #[cfg(target_arch = "aarch64")]
    const M: u16 = 183;
    let bid = kmod::buildid::build_id();
    let slot_a = (kmodwin::BASE + kmodwin::TEST_SLOT_OFF) as u64;
    let slot_b = slot_a + kmodwin::SLOT_ALIGN as u64;
    let base_pages = kmodwin::present_pages();
    let ping = exports::lookup("kapi_ktest_ping");
    t.check(ping.is_some(), "kapi_ktest_ping found by KSYMTAB walk");
    let ping = ping.unwrap_or(0) as u64;
    t.check(exports::ksymtab().len() == exports::NEXPORTS, "ksymtab length == NEXPORTS");

    // positive
    let tc = kmod_elf::text_tailcall(M, ping);
    let rodata = *b"kmodload-rodata!";
    let ok_img = kmod_elf::synth(M, bid, slot_a, "kt_ok", &tc, &rodata, &[1, 2, 3, 4], 64);
    let c0 = exports::ping_state().0;
    let id = kmod::load_image(&ok_img, Source::Ktest);
    t.check(id.is_ok(), "load_image Ok");
    let id = id.unwrap_or(kmod_abi::KmodId(0));
    let (c1, own, csz, abi, _cl) = exports::ping_state();
    t.check(c1 == c0 + 1, "init ran once (ping +1)");
    t.check(own == id.0 && csz == 48 && abi == 1, "init got the right ctx (owner, size 48, abi 1)");
    let mut live = false;
    kmod::for_each(&mut |m| { if kmod::name_str(&m.name) == "kt_ok" && m.id == id && m.state == kmod::State::Live { live = true; } });
    t.check(live, "registry: kt_ok Live");
    let hdr_init = kmod_abi::KmodHeader::from_bytes(unsafe { core::slice::from_raw_parts(slot_a as *const u8, 128) });
    let init_va = hdr_init.map_or(0, |h| h.init as usize);
    let f: extern "C" fn(*const kmod_abi::KmodCtx) -> i32 = unsafe { core::mem::transmute(init_va) };
    let cx = kmod_abi::KmodCtx { ctx_size: 48, abi_version: 1, owner: id, _pad: 0, _pad2: 0,
        cmdline: core::ptr::null(), cmdline_len: 0, params: core::ptr::null(), params_len: 0 };
    t.check(f(&cx) == 42 + id.0 as i32, "init re-call returns 42 + owner");
    let ro = unsafe { core::slice::from_raw_parts((slot_a as usize + 128) as *const u8, rodata.len()) };
    t.check(ro == rodata, "rodata reads back through the window");

    // W^X over every present window page
    let root = mm::paging::get_kernel_root();
    let (mut n, mut bad_wx, mut bad_ux, mut nx, mut nw) = (0, 0, 0, 0, 0);
    let mut va = kmodwin::BASE;
    let end = kmodwin::BASE + kmodwin::SIZE;
    while va < end {
        let p = mm::paging::leaf_pte(root, va);
        if p != 0 {
            n += 1;
            #[cfg(target_arch = "x86_64")]
            { let (w, x, u) = (p & 2 != 0, p >> 63 == 0, p & 4 != 0); if w && x { bad_wx += 1; } if u { bad_ux += 1; } if x { nx += 1; } if w { nw += 1; } }
            #[cfg(target_arch = "aarch64")]
            { let (w, x) = (p & (1 << 7) == 0, p & (1 << 53) == 0); if w && x { bad_wx += 1; } if p & (1 << 54) == 0 { bad_ux += 1; } if x { nx += 1; } if w { nw += 1; } }
        }
        va += if p != 0 { 4096 } else { kmodwin::SLOT_ALIGN };
    }
    t.check(n > 0 && nx >= 1 && nw >= 1, "module has executable and writable pages");
    t.check(bad_wx == 0, "no page is W+X");
    #[cfg(target_arch = "x86_64")]
    t.check(bad_ux == 0, "no page is USER");
    #[cfg(target_arch = "aarch64")]
    t.check(bad_ux == 0, "every page is UXN");

    // cross-CPU instruction fetch
    if sched::active_cpu_count() >= 2 {
        static XR: AtomicU32 = AtomicU32::new(0);
        static XCPU: AtomicU32 = AtomicU32::new(99);
        XR.store(0, Ordering::SeqCst);
        let owner = id;
        let th = kthread::spawn("kmodx", move || {
            let me = sched::current_pid();
            let _ = sched::cred::sched_setaffinity(me, Some(2));
            let mut tries = 0;
            while unsafe { sched::cpu_id() } != 1 && tries < 200 { kthread::msleep(2); tries += 1; }
            XCPU.store(unsafe { sched::cpu_id() } as u32, Ordering::SeqCst);
            let cx = kmod_abi::KmodCtx { ctx_size: 48, abi_version: 1, owner, _pad: 0, _pad2: 0,
                cmdline: core::ptr::null(), cmdline_len: 0, params: core::ptr::null(), params_len: 0 };
            let r = f(&cx);
            XR.store(r as u32, Ordering::SeqCst);
        });
        t.check(th.is_some(), "spawn pinned thread");
        let mut w = 0;
        while XR.load(Ordering::SeqCst) == 0 && w < 500 { kthread::msleep(10); w += 1; }
        t.check(XCPU.load(Ordering::SeqCst) == 1, "thread ran on CPU1");
        t.check(XR.load(Ordering::SeqCst) == 42 + id.0 as u32, "CPU1 executed the new code (42 + owner)");
    } else { say!("kmodload: single CPU, cross-CPU fetch check skipped (use -smp 2)"); }

    // spin hook through the table
    if let (Some(a), Some(b)) = (exports::lookup("kapi_spin_held"), exports::lookup("kapi_spin_held_count")) {
        let sh: extern "C" fn(bool) = unsafe { core::mem::transmute(a) };
        let sc: extern "C" fn() -> u32 = unsafe { core::mem::transmute(b) };
        let h0 = sc();
        sh(true);
        let h1 = sc();
        sh(false);
        t.check(h1 == h0 + 1 && sc() == h0, "kapi_spin_held +1 / -1 via table");
    } else { t.check(false, "spin exports found"); }

    // owner-tagged kthread
    if let Some(a) = exports::lookup("kapi_kthread_spawn") {
        static RAN: AtomicU32 = AtomicU32::new(0);
        extern "C" fn body(p: *mut u8) { RAN.store(p as usize as u32, Ordering::SeqCst); }
        let sp: unsafe extern "C" fn(*const u8, usize, extern "C" fn(*mut u8), *mut u8, kmod_abi::KmodId) -> i64 =
            unsafe { core::mem::transmute(a) };
        let tid = unsafe { sp(b"kmodt".as_ptr(), 5, body, 7usize as *mut u8, id) };
        t.check(tid > 0, "kapi_kthread_spawn returned a tid");
        let mut seen = 0i64;
        registry::registrations(id, &mut |k, h| { if k == kmod_abi::RegKind::KTHREAD { seen = h as i64; } });
        t.check(seen == tid, "registrations yields (Kthread, tid)");
        let mut w = 0;
        while RAN.load(Ordering::SeqCst) != 7 && w < 200 { kthread::msleep(5); w += 1; }
        t.check(RAN.load(Ordering::SeqCst) == 7, "module kthread ran");
    }

    // negatives: each leaves present_pages unchanged
    let pp = kmodwin::present_pages();
    let good = |name: &str, base: u64| kmod_elf::synth(M, bid, base, name, &tc, &rodata, &[0; 8], 0);
    let mut neg = |t: &mut T, what: &str, img: &[u8], want: fn(&KmodErr) -> bool| {
        let r = kmod::load_image(img, Source::Ktest);
        let ok = matches!(&r, Err(e) if want(e));
        t.check(ok && kmodwin::present_pages() == pp, what);
    };
    let mut i = good("kt_magic", slot_b); i[4096] ^= 0xff;
    neg(&mut t, "bad magic refused", &i, |e| matches!(e, KmodErr::Elf(ElfErr::Magic)));
    let mut i = good("kt_bid", slot_b); i[4096 + 24] ^= 1;
    neg(&mut t, "wrong build-id refused", &i, |e| matches!(e, KmodErr::Elf(ElfErr::BuildId)));
    let mut i = good("kt_wx", slot_b); i[64 + 56 + 4] = 7;
    neg(&mut t, "W+X segment refused", &i, |e| matches!(e, KmodErr::Elf(ElfErr::WplusX)));
    let i = good("kt_out", (kmodwin::BASE + kmodwin::SIZE) as u64);
    neg(&mut t, "slot outside window refused", &i, |e| matches!(e, KmodErr::Elf(_)));
    let i = good("kt_ovl", slot_a);
    neg(&mut t, "overlap with live slot refused", &i, |e| matches!(e, KmodErr::SlotBusy));
    let i = good("kt_ok", slot_b);
    neg(&mut t, "duplicate name refused", &i, |e| matches!(e, KmodErr::Exists));
    let i = good("kt_trunc", slot_b);
    neg(&mut t, "truncated image refused", &i[..200], |e| matches!(e, KmodErr::Elf(_)));

    // failed init with no registrations frees the slot; a retry works
    #[cfg(target_arch = "x86_64")]
    let fail_text: [u8; 6] = [0xb8, 0xfb, 0xff, 0xff, 0xff, 0xc3];
    #[cfg(target_arch = "aarch64")]
    let fail_text: [u8; 8] = [0x80, 0x00, 0x80, 0x12, 0xc0, 0x03, 0x5f, 0xd6];
    let fi = kmod_elf::synth(M, bid, slot_b, "kt_fail", &fail_text, &rodata, &[0; 8], 0);
    for round in 0..2 {
        let r = kmod::load_image(&fi, Source::Ktest);
        t.check(matches!(r, Err(KmodErr::Init(-5))) && kmodwin::present_pages() == pp,
            if round == 0 { "failing init -> Init(-5), pages unmapped" } else { "retry after failure reaches init again" });
    }
    let mut left = false;
    kmod::for_each(&mut |m| { if kmod::name_str(&m.name) == "kt_fail" { left = true; } });
    t.check(!left, "failed module dropped from the registry");
    t
}

fn t_kva() -> T {
    use mm::kva::{self, CacheMode};
    let mut t = T::new("kva");
    let (live0, pages0, _) = kva::stats();
    let pg = mm::buddy::alloc(0).unwrap();
    let hh = mm::phys_to_virt(pg) as *mut u32;
    for i in 0..1024 { unsafe { hh.add(i).write_volatile(0xA5A5_0000 | i as u32); } }
    let va = kva::ioremap(pg as u64 + 0x10, 64, CacheMode::WriteBack);
    t.check(va.map_or(false, |v| kva::in_window(v) && v & 0xFFF == 0x10), "ioremap returns a window VA with the page offset");
    if let Some(v) = va {
        let p = (v - 0x10) as *const u32;
        t.check((0..1024).all(|i| unsafe { p.add(i).read_volatile() } == 0xA5A5_0000 | i as u32), "ioremap(WB) alias reads what the HHDM wrote");
        unsafe { (p as *mut u32).add(5).write_volatile(0x1234_5678); }
        t.check(unsafe { hh.add(5).read_volatile() } == 0x1234_5678, "write through the alias visible in the HHDM");
        t.check(kva::iounmap(v), "iounmap");
        t.check(!kva::iounmap(v), "double iounmap refused");
    }
    mm::buddy::free(pg, 0);
    // vmap of scattered pages.
    let mut pages = Vec::new();
    let mut spacers = Vec::new();
    for _ in 0..8 { pages.push(mm::buddy::alloc(0).unwrap() as u64); spacers.push(mm::buddy::alloc(0).unwrap()); }
    let contiguous = pages.windows(2).all(|w| w[1] == w[0] + 4096);
    match kva::vmap(&pages, CacheMode::WriteBack) {
        Some(v) => {
            let p = v as *mut u64;
            for i in 0..(8 * 512) { unsafe { p.add(i).write_volatile(i as u64 * 0x9E37_79B9); } }
            let ok = (0..8).all(|pi| {
                let h = mm::phys_to_virt(pages[pi] as usize) as *const u64;
                (0..512).all(|j| unsafe { h.add(j).read_volatile() } == (pi * 512 + j) as u64 * 0x9E37_79B9)
            });
            t.check(ok, &format!("vmap of 8 scattered pages is contiguous (pages physically contiguous: {})", contiguous));
            t.check(kva::vunmap(v), "vunmap");
        }
        None => t.check(false, "vmap"),
    }
    for p in pages { mm::buddy::free(p as usize, 0); }
    for p in spacers { mm::buddy::free(p, 0); }
    // A big mapping (a 64 MiB / 16 MiB buddy block, write-back) and next-fit
    // placement. Real RAM only: no Normal-memory mapping over holes.
    let (blk, order) = match mm::buddy::alloc(14) { Some(p) => (p, 14), None => (mm::buddy::alloc(12).unwrap_or(0), 12) };
    if blk != 0 {
        let len = 4096usize << order;
        let a = now();
        let big = kva::ioremap(blk as u64, len, CacheMode::WriteBack);
        let dt = now() - a;
        t.check(big.is_some(), &format!("ioremap {} MiB ({} ms)", len >> 20, dt / 1_000_000));
        if let Some(b) = big {
            let off = len - (len >> 3);
            unsafe { (mm::phys_to_virt(blk + off) as *mut u64).write_volatile(0xFEED_F00D_1234_5678); }
            t.check(unsafe { ((b + off) as *const u64).read_volatile() } == 0xFEED_F00D_1234_5678,
                "big mapping translates correctly near its end");
            kva::iounmap(b);
            let again = kva::ioremap(blk as u64, 4096, CacheMode::Uncached).unwrap_or(0);
            t.check(again != 0 && again > b, "freed range not reused immediately (next-fit)");
            kva::iounmap(again);
        }
        mm::buddy::free(blk, order);
    }
    let (live1, pages1, _) = kva::stats();
    t.check(live1 == live0 && pages1 == pages0, &format!("no mappings leaked ({} -> {} live)", live0, live1));
    t
}

// ── pci ─────────────────────────────────────────────────────────────────────

fn t_pci() -> T {
    use drivers::pci::{self, PciFunction};
    let mut t = T::new("pci");
    let devs = pci::scan();
    t.check(!devs.is_empty(), &format!("{} functions scanned", devs.len()));
    let mut ok_id = true;
    for d in &devs {
        let f = d.function();
        let legacy = unsafe { pci::pci_read_config_32(d.bus, d.dev, d.func, 0) };
        if f.read32(0) != legacy || f.vendor_id() != d.vendor_id || f.device_id() != d.device_id { ok_id = false; }
    }
    t.check(ok_id, "PciFunction::read32/16 agree with the legacy accessors on every function");
    // Capabilities of the first virtio device.
    if let Some(d) = devs.iter().find(|d| d.vendor_id == 0x1af4) {
        let f = d.function();
        let caps = f.capabilities();
        say!("pci: {:02x}:{:02x}.{} caps {:?}", d.bus, d.dev, d.func, caps.iter().map(|c| c.0).collect::<Vec<_>>());
        t.check(caps.iter().any(|c| c.0 == pci::cap::VENDOR), "virtio function lists vendor capabilities");
        t.check(f.find_capability(pci::cap::VENDOR).is_some(), "find_capability");
        let s = f.size_bars();
        let mut agree = true;
        for b in &s.bars {
            if !b.io && b.base != 0 {
                let cur = f.bar(b.index).unwrap();
                if cur.base != b.base { agree = false; }
            }
        }
        say!("pci: bars {:?}", s.bars.iter().map(|b| (b.index, b.base, b.size)).collect::<Vec<_>>());
        t.check(!s.bars.is_empty() && agree, "size_bars: sizes found, bases restored");
        t.check(f.command() & pci::CMD_MEMORY != 0, "COMMAND restored (memory decode on)");
        let ext = f.has_extended_config();
        let e0 = f.read32(0x100);
        say!("pci: extended config {} (0x100 = {:#010x}), ext caps {:?}", ext, e0, f.ext_capabilities());
        if ext {
            // The ECAM view of the first dwords must match the legacy view.
            let page = f.ecam_page();
            let via_ecam = unsafe { core::ptr::read_volatile(page as *const u32) };
            t.check(via_ecam == f.read32(0), "ECAM page agrees with legacy config dword 0");
        } else {
            t.check(e0 == 0xFFFF_FFFF, "no ECAM: extended space reads all-ones");
        }
    }
    // Byte write on a scratch register (Interrupt Line) of some function.
    if let Some(d) = devs.iter().find(|d| d.vendor_id == 0x1234 && d.device_id == 0x11e8).or(devs.last()) {
        let f = PciFunction::new(d.bus, d.dev, d.func);
        let orig = f.read8(0x3C);
        let pin = f.read8(0x3D);
        f.write8(0x3C, 0x5A);
        let back = f.read8(0x3C);
        let pin2 = f.read8(0x3D);
        f.write8(0x3C, orig);
        t.check(back == 0x5A && pin == pin2 && f.read8(0x3C) == orig, "write8 hits one byte only (neighbour unchanged), restored");
    }
    #[cfg(target_arch = "x86_64")]
    t.check(PciFunction::new(0, 0, 0).has_extended_config(), "x86_64: ACPI MCFG ECAM available on q35");
    // Extended-capability walk on a PCIe root port (QEMU: -device pcie-root-port).
    match devs.iter().find(|d| d.class == 0x06 && d.subclass == 0x04) {
        Some(rp) => {
            let f = rp.function();
            let ext = f.ext_capabilities();
            say!("pci: root port {:02x}:{:02x}.{} ext caps {:?}", rp.bus, rp.dev, rp.func,
                ext.iter().map(|e| (e.0, e.2)).collect::<Vec<_>>());
            t.check(!ext.is_empty() && ext.iter().all(|e| e.2 >= 0x100 && e.2 < 0x1000)
                && f.find_ext_capability(ext[0].0) == Some(ext[0].2),
                "extended capabilities walked through ECAM");
            let pcie = f.find_capability(pci::cap::PCIE);
            t.check(pcie.is_some(), "root port has a PCIe capability");
        }
        None => say!("pci: no PCIe root port to walk extended caps on (add -device pcie-root-port,chassis=1)"),
    }
    t
}

// ── msi / intx ──────────────────────────────────────────────────────────────

struct EduState { base: usize, irqs: AtomicU32, in_irq_ok: AtomicU32, done: KCompletion, last_status: AtomicU32 }

extern "C" fn edu_handler(ctx: usize) -> drivers::irq::IrqReturn {
    let s = unsafe { &*(ctx as *const EduState) };
    let st = unsafe { ((s.base + 0x24) as *const u32).read_volatile() };
    if st == 0 { return drivers::irq::IrqReturn::None; }
    unsafe { ((s.base + 0x64) as *mut u32).write_volatile(st); }
    s.last_status.store(st, Ordering::Release);
    if irqctx::in_irq() { s.in_irq_ok.fetch_add(1, Ordering::AcqRel); }
    s.irqs.fetch_add(1, Ordering::AcqRel);
    s.done.complete();
    drivers::irq::IrqReturn::Handled
}

fn edu_test(t: &mut T, f: drivers::pci::PciFunction) {
    use drivers::irq;
    f.enable_mem_and_bus_master();
    let (base, size) = match f.map_bar(0, mm::kva::CacheMode::Uncached) { Some(x) => x, None => { t.check(false, "map edu BAR0"); return; } };
    let id = unsafe { (base as *const u32).read_volatile() };
    t.check(id == 0x010000ed, &format!("edu BAR0 ({} KiB, uncached KVA) id {:#x}", size >> 10, id));
    unsafe { ((base + 4) as *mut u32).write_volatile(0x1234_5678); }
    t.check(unsafe { ((base + 4) as *const u32).read_volatile() } == !0x1234_5678u32, "edu liveness register inverts");
    let st = Arc::new(EduState { base, irqs: AtomicU32::new(0), in_irq_ok: AtomicU32::new(0), done: KCompletion::new(), last_status: AtomicU32::new(0) });
    let ctx = Arc::as_ptr(&st) as usize;
    let h = if cfg!(target_arch = "x86_64") {
        match irq::request_msi(f, edu_handler, ctx) {
            Ok(h) => { say!("msi: edu MSI on vector {:#x}", h.vector().unwrap_or(0)); h }
            Err(e) => { t.check(false, &format!("request_msi: {:?}", e)); return; }
        }
    } else {
        let line = match f.intx_line() { Some(l) => l, None => { t.check(false, "edu INTx line"); return; } };
        f.set_intx_disable(false);
        match irq::request_irq(irq::IrqSource::Line(line), edu_handler, ctx) {
            Ok(h) => { say!("msi: edu INTx on GIC INTID {}", line); h }
            Err(e) => { t.check(false, &format!("request_irq(Line): {:?}", e)); return; }
        }
    };
    // 1. Raise-by-register interrupt.
    unsafe { ((base + 0x60) as *mut u32).write_volatile(0x1234); }
    let got = st.done.wait_timeout(1_000_000_000);
    t.check(got && st.last_status.load(Ordering::Acquire) == 0x1234, "edu interrupt delivered, handler acked status 0x1234");
    t.check(st.in_irq_ok.load(Ordering::Acquire) == 1, "handler ran in hard-IRQ context");
    // 2. DMA RAM -> edu -> RAM with completion interrupts (edu DMA mask: 28 bits).
    // edu masks DMA addresses to 28 bits unless started with a wider
    // `dma_mask` (aarch64 RAM starts at 1 GiB, so it needs one).
    let edu_alloc = || mm::buddy::alloc_below(0, 1 << 28).or_else(|| mm::buddy::alloc(0));
    let (src, dst) = (edu_alloc(), edu_alloc());
    let mut expect = 1;
    if let (Some(src), Some(dst)) = (src, dst) {
        expect = 3;
        if src >= 1 << 28 || dst >= 1 << 28 {
            say!("msi: DMA pages above 256 MiB: needs -device edu,dma_mask=0xffffffffffffffff");
        }
        let sp = mm::phys_to_virt(src) as *mut u8;
        let dp = mm::phys_to_virt(dst) as *mut u8;
        for i in 0..4096 { unsafe { sp.add(i).write((i * 7 + 3) as u8); dp.add(i).write(0); } }
        let dma = |from: u64, to: u64, cmd: u64| unsafe {
            ((base + 0x80) as *mut u64).write_volatile(from);
            ((base + 0x88) as *mut u64).write_volatile(to);
            ((base + 0x90) as *mut u64).write_volatile(4096);
            ((base + 0x98) as *mut u64).write_volatile(cmd);
        };
        let a = now();
        dma(src as u64, 0x40000, 1 | 4);
        let r1 = st.done.wait_timeout(1_000_000_000);
        dma(0x40000, dst as u64, 1 | 2 | 4);
        let r2 = st.done.wait_timeout(1_000_000_000);
        let same = (0..4096).all(|i| unsafe { dp.add(i).read() == ((i * 7 + 3) as u8) });
        t.check(r1 && r2 && same, &format!("edu DMA round trip with completion IRQs ({} us)", (now() - a) / 1000));
        mm::buddy::free(src, 0);
        mm::buddy::free(dst, 0);
    }
    t.check(st.irqs.load(Ordering::Acquire) == expect, &format!("exactly {} interrupts ({})", expect, st.irqs.load(Ordering::Acquire)));
    irq::free_irq(h);
    if cfg!(target_arch = "aarch64") { f.set_intx_disable(true); }
    unsafe { ((base + 0x60) as *mut u32).write_volatile(0x1); }
    kthread::msleep(20);
    t.check(st.irqs.load(Ordering::Acquire) == expect, "no delivery after free_irq");
    unsafe { ((base + 0x64) as *mut u32).write_volatile(0xFFFF_FFFF); }
    mm::kva::iounmap(base);
}

/// MSI-X test on a spare modern virtio-rng function (not the one the kernel's
/// rng driver claimed): one request on queue 0, its completion on MSI-X entry
/// 0; then the same with the entry masked (no delivery, PBA pending) and
/// unmasked (delivery).
#[cfg(target_arch = "x86_64")]
fn virtio_rng_msix_test(t: &mut T, f: drivers::pci::PciFunction) {
    use drivers::irq::{self, MsixTable};
    f.enable_mem_and_bus_master();
    // Find common / notify cfg via the vendor capabilities.
    let (mut common, mut notify, mut mult) = (0usize, 0usize, 0u32);
    for (id, off) in f.capabilities() {
        if id != drivers::pci::cap::VENDOR { continue; }
        let ty = f.read8(off + 3);
        let bar = f.read8(off + 4) as usize;
        let o = f.read32(off + 8) as u64;
        let len = f.read32(off + 12) as usize;
        let b = match f.bar(bar) { Some(b) if !b.io && b.base != 0 => b, _ => continue };
        let va = mm::kva::ioremap(b.base + o, len.max(4), mm::kva::CacheMode::Uncached).unwrap_or(0);
        match ty { 1 => common = va, 2 => { notify = va; mult = f.read32(off + 16); } _ => { if va != 0 { mm::kva::iounmap(va); } } }
    }
    if common == 0 || notify == 0 { t.check(false, "virtio-rng common/notify cfg"); return; }
    let w8 = |o: usize, v: u8| unsafe { ((common + o) as *mut u8).write_volatile(v) };
    let r8 = |o: usize| unsafe { ((common + o) as *const u8).read_volatile() };
    let w16 = |o: usize, v: u16| unsafe { ((common + o) as *mut u16).write_volatile(v) };
    let r16 = |o: usize| unsafe { ((common + o) as *const u16).read_volatile() };
    let w32 = |o: usize, v: u32| unsafe { ((common + o) as *mut u32).write_volatile(v) };
    let w64 = |o: usize, v: u64| unsafe { ((common + o) as *mut u64).write_volatile(v) };
    w8(0x14, 0);
    let a = now();
    while r8(0x14) != 0 && now() - a < 100_000_000 { core::hint::spin_loop(); }
    w8(0x14, 1 | 2);
    w32(0x08, 1); w32(0x0C, 1); // VERSION_1
    w32(0x08, 0); w32(0x0C, 0);
    w8(0x14, 1 | 2 | 8);
    if r8(0x14) & 8 == 0 { t.check(false, "virtio-rng FEATURES_OK"); return; }
    let tbl = match MsixTable::enable(f) { Ok(x) => x, Err(e) => { t.check(false, &format!("MsixTable::enable {:?}", e)); return; } };
    static RNG_IRQS: AtomicU32 = AtomicU32::new(0);
    static RNG_DONE: KCompletion = KCompletion::new();
    extern "C" fn rng_handler(_c: usize) -> irq::IrqReturn {
        RNG_IRQS.fetch_add(1, Ordering::AcqRel);
        RNG_DONE.complete();
        irq::IrqReturn::Handled
    }
    let h = match tbl.request(0, rng_handler, 0) { Ok(h) => h, Err(e) => { t.check(false, &format!("msix request {:?}", e)); tbl.disable(); return; } };
    say!("msi: virtio-rng MSI-X entry 0 of {} on vector {:#x}", tbl.entries(), h.vector().unwrap_or(0));
    w16(0x10, 0xFFFF);
    w16(0x16, 0);
    let qsz = r16(0x18).min(8).max(1);
    w16(0x18, qsz);
    w16(0x1A, 0);
    t.check(r16(0x1A) == 0, "device accepted MSI-X vector 0 for queue 0");
    let ring = mm::buddy::alloc(0).unwrap();
    let buf = mm::buddy::alloc(0).unwrap();
    let rv = mm::phys_to_virt(ring);
    unsafe { core::ptr::write_bytes(rv as *mut u8, 0, 4096); }
    let (desc, avail, used) = (ring, ring + 0x400, ring + 0x800);
    w64(0x20, desc as u64); w64(0x28, avail as u64); w64(0x30, used as u64);
    let qnoff = r16(0x1E) as usize;
    w16(0x1C, 1);
    w8(0x14, 1 | 2 | 8 | 4);
    let post = |n: u16| unsafe {
        let d = (rv as *mut u64, (rv + 8) as *mut u32, (rv + 12) as *mut u16);
        d.0.write_volatile(buf as u64 + n as u64 * 64);
        d.1.write_volatile(64);
        d.2.write_volatile(2); // VIRTQ_DESC_F_WRITE
        let av = rv + 0x400;
        ((av + 4 + 2 * (n as usize % qsz as usize)) as *mut u16).write_volatile(0);
        core::sync::atomic::fence(Ordering::SeqCst);
        ((av + 2) as *mut u16).write_volatile(n + 1);
        core::sync::atomic::fence(Ordering::SeqCst);
        ((notify + qnoff * mult as usize) as *mut u16).write_volatile(0);
    };
    let used_idx = || unsafe { ((rv + 0x800 + 2) as *const u16).read_volatile() };
    post(0);
    let ok1 = RNG_DONE.wait_timeout(1_000_000_000);
    t.check(ok1 && used_idx() == 1 && RNG_IRQS.load(Ordering::Acquire) == 1, "MSI-X: queue completion interrupt delivered");
    tbl.set_masked(0, true);
    post(1);
    kthread::msleep(100);
    let pend = tbl.pending(0);
    t.check(used_idx() == 2 && RNG_IRQS.load(Ordering::Acquire) == 1 && pend, &format!("MSI-X: masked entry holds the interrupt (pending bit {})", pend));
    tbl.set_masked(0, false);
    let ok3 = RNG_DONE.wait_timeout(1_000_000_000);
    t.check(ok3 && RNG_IRQS.load(Ordering::Acquire) == 2, "MSI-X: unmask delivers the pending interrupt");
    w8(0x14, 0);
    irq::free_irq(h);
    tbl.disable();
    mm::buddy::free(ring, 0);
    mm::buddy::free(buf, 0);
    mm::kva::iounmap(common);
    mm::kva::iounmap(notify);
}

fn t_msi() -> T {
    let mut t = T::new("msi");
    let devs = drivers::pci::scan();
    match devs.iter().find(|d| d.vendor_id == 0x1234 && d.device_id == 0x11e8) {
        Some(d) => edu_test(&mut t, d.function()),
        None => t.skip("no edu device (QEMU: -device edu)"),
    }
    // Vector allocator basics (x86_64).
    #[cfg(target_arch = "x86_64")]
    {
        let a = drivers::irq::alloc_vector();
        let b = drivers::irq::alloc_vector();
        t.check(a.is_some() && b.is_some() && a != b && a.unwrap() >= drivers::irq::ALLOC_FIRST, &format!("alloc_vector hands out distinct vectors ({:?}, {:?})", a, b));
        if let (Some(a), Some(b)) = (a, b) {
            drivers::irq::free_vector(a);
            let c = drivers::irq::alloc_vector();
            t.check(c == Some(a), "freed vector is reused");
            drivers::irq::free_vector(b);
            if let Some(c) = c { drivers::irq::free_vector(c); }
        }
        let gpu = drivers::virtio_gpu::MSIX_VECTOR_CTRLQ.load(Ordering::Acquire);
        say!("msi: virtio-gpu control queue vector {:#x}, {} interrupts so far", gpu, if gpu != 0 { drivers::irq::vector_count(gpu) } else { 0 });
        let rngs: Vec<_> = devs.iter().filter(|d| d.vendor_id == 0x1af4 && d.device_id == 0x1044).collect();
        if rngs.len() >= 2 {
            virtio_rng_msix_test(&mut t, rngs[rngs.len() - 1].function());
        } else {
            say!("msi: no spare virtio-rng for the MSI-X test (add -device virtio-rng-pci,disable-legacy=on)");
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        let r = drivers::irq::request_msi(drivers::pci::PciFunction::new(0, 0, 0), edu_handler, 0);
        t.check(matches!(r, Err(drivers::irq::IrqError::Unsupported) | Err(drivers::irq::IrqError::NotFound)), "aarch64: MSI reports Unsupported (no ITS yet)");
    }
    t
}

// ── firmware ────────────────────────────────────────────────────────────────

/// POSIX `cksum` (CRC-32/CKSUM over the data then the length), comparable
/// with the guest's `cksum` tool.
struct Cksum { crc: u32, len: u64, tab: [u32; 256] }
impl Cksum {
    fn new() -> Cksum {
        let mut tab = [0u32; 256];
        for i in 0..256u32 {
            let mut c = i << 24;
            for _ in 0..8 { c = if c & 0x8000_0000 != 0 { (c << 1) ^ 0x04C1_1DB7 } else { c << 1 }; }
            tab[i as usize] = c;
        }
        Cksum { crc: 0, len: 0, tab }
    }
    #[inline]
    fn byte(&mut self, b: u8) { self.crc = (self.crc << 8) ^ self.tab[((self.crc >> 24) as u8 ^ b) as usize]; }
    fn update(&mut self, d: &[u8]) {
        for (i, &b) in d.iter().enumerate() {
            self.byte(b);
            if i & 0xFFFF == 0xFFFF { kthread::cond_resched(); }
        }
        self.len += d.len() as u64;
    }
    fn finish(mut self) -> u32 {
        let mut n = self.len;
        while n != 0 { self.byte(n as u8); n >>= 8; }
        !self.crc
    }
}

// ── gpulock ─────────────────────────────────────────────────────────────────

/// A framebuffer-console print made while this CPU holds `VIRTIO_GPU`
/// (bug L5.2): the print drains the console outbox, whose framebuffer flush
/// needs `VIRTIO_GPU`. Before the fix this spun forever (`[WDOG] ... spinning
/// for VIRTIO_GPU held by` this CPU) and the test never finished; now the
/// flush is put back and sent once the lock is free.
fn t_gpulock() -> T {
    let mut t = T::new("gpulock");
    let before = drivers::framebuffer::FLUSH_DEFERRED_REENTRY.load(Ordering::Relaxed);
    let t0 = now();
    {
        let _g = drivers::virtio_gpu::VIRTIO_GPU.lock();
        t.check(drivers::framebuffer::gpu_held_here(), "VIRTIO_GPU held by this CPU");
        // Through the framebuffer console, not `log` (raw UART).
        crate::serial_print_str("[KTEST] gpulock: console line printed under VIRTIO_GPU\n");
    }
    let dt = now() - t0;
    t.check(true, "console print under VIRTIO_GPU returned");
    say!("gpulock: print under the lock took {} us, re-entrant flushes put back {}",
         dt / 1000, drivers::framebuffer::FLUSH_DEFERRED_REENTRY.load(Ordering::Relaxed) - before);
    t.check(!drivers::framebuffer::gpu_held_here(), "VIRTIO_GPU released");
    drivers::framebuffer::fb_flush_deferred();
    t
}

fn t_firmware(path: Option<&str>) -> T {
    let mut t = T::new("firmware");
    let a = now();
    while !drivers::firmware::root_ready() && now() - a < 120_000_000_000 { kthread::msleep(100); }
    if !drivers::firmware::root_ready() { t.check(false, "real root mounted within 120 s"); return t; }
    say!("firmware: root ready after {} ms", (now() - a) / 1_000_000);
    t.check(matches!(drivers::firmware::firmware_size("/no/such/firmware.bin"), Err(drivers::firmware::FwError::Fs(-2))), "missing file -> ENOENT");
    let candidates = ["/bin/mame", "/usr/lib/firefox/libxul.so", "/usr/lib/libxul.so", "/bin/brush", "/bin/init"];
    let p: String = match path {
        Some(p) => String::from(p),
        None => {
            let mut best = (0u64, "");
            for c in candidates { if let Ok(s) = drivers::firmware::firmware_size(c) { if s > best.0 { best = (s, c); } } }
            String::from(best.1)
        }
    };
    let size = match drivers::firmware::firmware_size(&p) { Ok(s) => s, Err(e) => { t.check(false, &format!("size of {}: {:?}", p, e)); return t; } };
    say!("firmware: {} is {} bytes", p, size);
    let a = now();
    let mut ck = Cksum::new();
    let streamed = drivers::firmware::read_firmware_chunked(&p, &mut |_, d| ck.update(d));
    let c_stream = ck.finish();
    let dt_s = now() - a;
    t.check(streamed == Ok(size), "chunked read delivered every byte");
    let a = now();
    let fw = drivers::firmware::request_firmware(&p, usize::MAX);
    let dt_l = now() - a;
    match fw {
        Ok(mut fw) => {
            let mut ck = Cksum::new();
            for c in fw.chunks() { ck.update(c); }
            let c_pages = ck.finish();
            t.check(fw.len() as u64 == size && fw.pages().len() == (fw.len() + 4095) / 4096, &format!("page list: {} pages", fw.pages().len()));
            t.check(c_pages == c_stream, "page-list checksum == streamed checksum");
            match fw.map() {
                Some(v) => {
                    let mut ck = Cksum::new();
                    ck.update(unsafe { core::slice::from_raw_parts(v as *const u8, fw.len()) });
                    t.check(ck.finish() == c_stream, "contiguous vmap view checksum matches");
                }
                None => t.check(false, "Firmware::map"),
            }
            let mut b = [0u8; 16];
            let n = fw.read_at(fw.len().saturating_sub(10), &mut b);
            t.check(n == 10.min(fw.len()), "read_at clamps at EOF");
            say!("firmware: {} cksum {} {} (stream {} ms, load {} ms) -- compare with guest `cksum {}`",
                p, c_stream, size, dt_s / 1_000_000, dt_l / 1_000_000, p);
        }
        Err(e) => t.check(false, &format!("request_firmware: {:?}", e)),
    }
    let (free0, _) = (mm::buddy::free_pages(), 0);
    say!("firmware: buddy free pages after release {}", free0);
    t
}

/// `ktest=memperf`: throughput of the kernel memcpy/memset/memmove on RAM
/// (HHDM, write-back), aligned and misaligned, plus a content check.
fn t_memperf() -> T {
    use core::hint::black_box;
    let mut t = T::new("memperf");
    const ORDER: usize = 8; // 1 MiB
    const SZ: usize = 4096 << ORDER;
    let (pa, pb) = match (mm::buddy::alloc(ORDER), mm::buddy::alloc(ORDER)) {
        (Some(a), Some(b)) => (a, b),
        _ => { t.skip("no 1 MiB block"); return t; }
    };
    let a = mm::phys_to_virt(pa) as *mut u8;
    let b = mm::phys_to_virt(pb) as *mut u8;
    for i in 0..SZ { unsafe { a.add(i).write_volatile((i * 7 + 3) as u8); } }
    let bench = |what: &str, reps: usize, bytes: usize, f: &mut dyn FnMut()| {
        f();
        let t0 = now();
        for _ in 0..reps { f(); }
        let ns = (now() - t0).max(1);
        say!("memperf {:<22} {:>6} MB/s", what, (reps * bytes) as u64 * 1000 / ns);
    };
    unsafe {
        bench("memcpy 4K aligned", 4096, 4096, &mut || {
            core::ptr::copy_nonoverlapping(black_box(a as *const u8), black_box(b), black_box(4096));
        });
        bench("memcpy 1M aligned", 32, SZ, &mut || {
            core::ptr::copy_nonoverlapping(black_box(a as *const u8), black_box(b), black_box(SZ));
        });
        bench("memcpy 1M src+3", 32, SZ - 8, &mut || {
            core::ptr::copy_nonoverlapping(black_box(a.add(3) as *const u8), black_box(b), black_box(SZ - 8));
        });
        bench("memset 1M", 32, SZ, &mut || {
            core::ptr::write_bytes(black_box(b), black_box(0x5a), black_box(SZ));
        });
        bench("memmove 1M up+8", 32, SZ - 8, &mut || {
            core::ptr::copy(black_box(a as *const u8), black_box(a.add(8)), black_box(SZ - 8));
        });
        bench("memmove 1M down+5", 32, SZ - 8, &mut || {
            core::ptr::copy(black_box(a.add(5) as *const u8), black_box(a), black_box(SZ - 8));
        });
        // Content check: misaligned copy, then compare byte by byte.
        for i in 0..SZ { a.add(i).write_volatile((i * 13 + 1) as u8); }
        core::ptr::copy_nonoverlapping(black_box(a.add(5) as *const u8), black_box(b.add(2)), black_box(SZ - 16));
        let mut bad = 0usize;
        for i in 0..SZ - 16 {
            if b.add(2 + i).read_volatile() != a.add(5 + i).read_volatile() { bad += 1; }
        }
        t.check(bad == 0, "misaligned 1 MiB memcpy content");
    }
    mm::buddy::free(pa, ORDER);
    mm::buddy::free(pb, ORDER);
    t
}
