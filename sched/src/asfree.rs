//! `/proc/leandros_asfree`: the address-space teardown ring and its
//! fault-injection knobs (lane memexact).
//!
//! Reading shows `total <n>` (teardowns recorded since boot; more than 256
//! newer than the reader's baseline means the ring wrapped) and one
//! `<seq> <tgid> <pages_freed> <pt_pages_freed>` row per entry, oldest first.
//!
//! Writes (root only, checked by the VFS), one command per write:
//!   `hold <pid>`            the kernel keeps a strong `Arc` of pid's space
//!                           (without `busy`), so its teardown cannot run;
//!   `busyhold <pid> <us>`   a kernel thread takes the space's `busy` flag and
//!                           holds it `us` microseconds (<= 5000), then drops;
//!   `release <pid>`         drop the hold now (teardown runs here if last).
//! One fixed slot: a second arm while it is taken fails with EBUSY. The slot
//! releases itself after 10 s; the release runs on the slot's own kernel
//! thread (task context), never in an IRQ handler. Nothing here is on any hot
//! path: with the slot empty no code of this file runs.
//!
//! TODO (needs the fix, not implemented): `legacy`, `pause`.

use alloc::sync::Arc;
use core::sync::atomic::Ordering;
use mm::vmm::AddressSpace;
use crate::task::Pid;

const AUTO_RELEASE_NS: u64 = 10_000_000_000;
const BUSYHOLD_MAX_US: u64 = 5000;

struct Held { gen: u64, pid: Pid, _arc: Option<Arc<AddressSpace>> }
static SLOT: spin::Mutex<Option<Held>> = spin::Mutex::new(None);
static GEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// Text of the file.
pub fn render(s: &mut alloc::string::String) {
    use core::fmt::Write;
    let mut v = alloc::vec::Vec::with_capacity(mm::vmm::ASFREE_RING);
    let total = mm::vmm::asfree_snapshot(&mut v);
    let _ = writeln!(s, "total {}", total);
    for (n, t, p, q) in v { let _ = writeln!(s, "{} {} {} {}", n, t, p, q); }
}

fn num(w: Option<&str>) -> Option<u64> { w?.parse().ok() }

/// Run one command; returns 0 or a negative errno.
pub fn control(cmd: &str) -> i32 {
    let mut it = cmd.split_ascii_whitespace();
    let verb = it.next();
    let pid = match num(it.next()) { Some(p) if p > 0 && p <= u32::MAX as u64 => p as Pid, _ => return -22 };
    match verb {
        Some("release") => {
            let h = { let mut s = SLOT.lock(); if s.as_ref().map_or(false, |h| h.pid == pid) { s.take() } else { None } };
            drop(h); // the teardown, if this was the last reference, runs here
            0
        }
        Some("hold") => arm(pid, 0),
        Some("busyhold") => match num(it.next()) {
            Some(us) if us >= 1 && us <= BUSYHOLD_MAX_US => arm(pid, us),
            _ => -22,
        },
        _ => -22,
    }
}

fn arm(pid: Pid, busy_us: u64) -> i32 {
    let arc = match crate::task_as(pid) { Some(a) => a, None => return -3 };
    let gen = GEN.fetch_add(1, Ordering::Relaxed);
    {
        let mut s = SLOT.lock();
        if s.is_some() { return -16; }
        *s = Some(Held { gen, pid, _arc: if busy_us == 0 { Some(arc.clone()) } else { None } });
    }
    let deadline = crate::monotonic_ns().saturating_add(AUTO_RELEASE_NS);
    let spawned = crate::kthread::spawn("asfree_hold", move || worker(gen, arc, busy_us, deadline));
    if spawned.is_none() {
        let mut s = SLOT.lock();
        if s.as_ref().map_or(false, |h| h.gen == gen) { *s = None; }
        return -12;
    }
    0
}

fn worker(gen: u64, arc: Arc<AddressSpace>, busy_us: u64, deadline: u64) {
    if busy_us != 0 {
        // Observer-style holder: strong ref + `busy` for a short window.
        let w = Arc::downgrade(&arc);
        drop(arc);
        if let Some(a) = crate::acquire_foreign_as(&w, 0) {
            let t0 = crate::monotonic_ns();
            while crate::monotonic_ns().saturating_sub(t0) < busy_us * 1000 {
                mm::paging::tlb_service_pending();
                core::hint::spin_loop();
            }
            a.lock_epoch.fetch_add(1, Ordering::Relaxed);
            a.busy.store(false, Ordering::Release);
            drop(a);
        }
        let mut s = SLOT.lock();
        if s.as_ref().map_or(false, |h| h.gen == gen) { *s = None; }
        return;
    }
    drop(arc); // the slot holds the strong reference, not this copy
    loop {
        crate::kthread::msleep(5);
        let mut s = SLOT.lock();
        match s.as_ref() {
            Some(h) if h.gen == gen => {
                if crate::monotonic_ns() >= deadline {
                    let h = s.take();
                    drop(s);
                    drop(h); // task context: the teardown may run here
                    return;
                }
            }
            _ => return, // released explicitly
        }
    }
}
