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

const ALL: [&str; 10] = ["irqctx", "sync", "kthread", "workqueue", "hrtimer", "atomicpool", "kva", "pci", "msi", "firmware"];

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
        if let Some(k) = ALL.iter().find(|k| **k == n) { names.push(k); } else { say!("unknown test '{}'", n); }
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
            "pci" => t_pci(),
            "msi" => t_msi(),
            "firmware" => t_firmware(fw.as_deref()),
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
