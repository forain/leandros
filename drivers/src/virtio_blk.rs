//! VirtIO block device driver (PCI transport, polling mode).
//!
//! [`probe_all`] enumerates every VirtIO block PCI device and returns each as
//! a `VirtioDisk` implementing `blkdev::BlockDevice` (512-byte sectors, its
//! own lock); the runtime registry in `blkdev.rs` owns indices, names and the
//! 4096-byte block façade the rest of the kernel uses.

#[allow(unused_imports)]
use leandros_lib::mmio::{MmioRd, MmioWr};
use core::sync::atomic::AtomicU64;
use core::fmt::Write as _;
use crate::kenv::pci::{PciDevice, find_all_devices, pci_read_config_8, pci_read_config_16, pci_read_config_32, pci_write_config_16};
use crate::kenv::{mm, sched};

const VIRTIO_PCI_VENDOR: u16 = 0x1af4;
const VIRTIO_PCI_DEVICE_BLK_MODERN: u16 = 0x1042;
const VIRTIO_PCI_DEVICE_BLK_LEGACY: u16 = 0x1001;

const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
const VIRTIO_STATUS_DRIVER: u8 = 2;
const VIRTIO_STATUS_FEATURES_OK: u8 = 8;
const VIRTIO_STATUS_DRIVER_OK: u8 = 4;
const VIRTIO_STATUS_NEEDS_RESET: u8 = 0x40;
const VIRTIO_STATUS_FAILED: u8 = 0x80;

/// A request still unanswered after this long is reported as slow (and again
/// at 2x, 4x, ...). It is never abandoned for being slow: like Linux
/// virtio-blk (no request timeout), we keep waiting, and only a device that
/// reports NEEDS_RESET/FAILED (or reads all-ones: gone) is declared dead.
/// Until 2026-10-25 a 30 s stall marked the disk dead for good; a loaded
/// host running x86_64 TCG hit that, f2fs went read-only and the root shell
/// died on its next page-in (openbugs L12 #50).
const IO_TIMEOUT_NS: u64 = 30_000_000_000;

/// Test knobs (kernel command line), read once the command line is known:
/// `virtio_blk.inject_delay=N:MS` holds back the doorbell of the N-th request
/// since boot (all virtio-blk disks, counted from 1) for MS milliseconds,
/// i.e. a device that answers MS late; `virtio_blk.slow_ms=MS` replaces the
/// 30 s slow-request threshold.
struct Knobs { delay_n: u64, delay_ns: u64, slow_ns: u64, every_k: u64, every_ns: u64 }
static KNOBS_STATE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
static mut KNOBS: Knobs = Knobs { delay_n: 0, delay_ns: 0, slow_ns: IO_TIMEOUT_NS, every_k: 0, every_ns: 0 };
static REQ_SEQ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn knobs() -> &'static Knobs {
    use core::sync::atomic::Ordering;
    if KNOBS_STATE.load(Ordering::Acquire) == 0 && crate::kenv::cmdline_known() {
        let mut k = Knobs { delay_n: 0, delay_ns: 0, slow_ns: IO_TIMEOUT_NS, every_k: 0, every_ns: 0 };
        if let Some((n, ms)) = crate::kenv::cmdline_word("virtio_blk.inject_delay")
            .and_then(|w| w.split_once(':'))
            .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?))) {
            k.delay_n = n; k.delay_ns = ms * 1_000_000;
        }
        // `inject_every=K:MS`: every K-th request (from #K on) is answered MS late, like a
        // lock holder the host keeps descheduling (lane f2fswatchdog).
        if let Some((n, ms)) = crate::kenv::cmdline_word("virtio_blk.inject_every")
            .and_then(|w| w.split_once(':'))
            .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?))) {
            k.every_k = n; k.every_ns = ms * 1_000_000;
        }
        if let Some(ms) = crate::kenv::cmdline_opt("virtio_blk.slow_ms") {
            if ms > 0 { k.slow_ns = ms * 1_000_000; }
        }
        // Requests run one at a time per disk; two disks racing here write
        // the same values.
        unsafe { KNOBS = k; }
        KNOBS_STATE.store(1, Ordering::Release);
    }
    unsafe { &*core::ptr::addr_of!(KNOBS) }
}

/// One step of a bounded busy-wait: on a syscall or page-fault path with
/// IRQs masked, open an IRQ window so the tick still runs (never switching
/// tasks while the caller holds the device lock). Boot-time requests (pid 0,
/// before the scheduler) stay fully masked, as in nvme's IoWindow.
fn irq_window_if_masked() {
    if sched::irqs_masked() && sched::current_pid() != 0 {
        sched::preempt_disable();
        sched::irq_window();
        sched::preempt_enable_no_resched();
    }
}

/// Why a request never completed (`device_status` at the time).
#[derive(Clone, Copy)]
enum WaitErr { Status(u8) }

const VIRTIO_BLK_T_IN:  u32 = 0; // read from device
const VIRTIO_BLK_T_OUT: u32 = 1; // write to device
/// Commit the device's own volatile write cache. Carries no data buffer —
/// header and status descriptors only.
const VIRTIO_BLK_T_FLUSH: u32 = 4;

const VIRTQ_DESC_F_NEXT:  u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

const SECTOR: usize = 512;
const BLOCK_SIZE: usize = 4096;

/// Largest number of 4096-byte blocks a single request may carry.  The DMA
/// data buffer is one order-6 buddy allocation (64 pages = 256 KiB); a
/// multi-block read costs the same submit/notify/poll round trip as a
/// single-block one, so batching contiguous reads through this buffer cuts
/// per-block device overhead by up to 64x (large sequential file reads).
pub const MAX_IO_BLOCKS: usize = 64;
const DATA_BUF_ORDER: usize = 6; // 2^6 pages = MAX_IO_BLOCKS

/// Largest single request in bytes (the size of the DMA bounce buffer).
const MAX_IO_BYTES: usize = MAX_IO_BLOCKS * BLOCK_SIZE;

/// Virtio disks probed at boot. Was 8, which silently dropped the 9th and
/// later disks (openbugs #83: 3 default disks + 8 data volumes left the last
/// three unmounted). Each disk only costs its leaked queue + bounce buffer.
const MAX_BLK_DEVICES: usize = 32;

// ── On-device register layout ────────────────────────────────────────────────

#[repr(C, packed)]
struct VirtioPciCommonCfg {
    device_feature_select: u32,
    device_feature: u32,
    driver_feature_select: u32,
    driver_feature: u32,
    config_msix_vector: u16,
    num_queues: u16,
    device_status: u8,
    config_generation: u8,
    queue_select: u16,
    queue_size: u16,
    queue_msix_vector: u16,
    queue_enable: u16,
    queue_notify_off: u16,
    queue_desc: u64,
    queue_driver: u64,
    queue_device: u64,
}

// ── Virtqueue ────────────────────────────────────────────────────────────────

#[repr(C, packed)]
struct VirtqDesc { addr: u64, len: u32, flags: u16, next: u16 }

#[repr(C, packed)]
struct VirtqAvail { flags: u16, idx: u16, ring: [u16; 32] }

#[repr(C, packed)]
struct VirtqUsedElem { id: u32, len: u32 }

#[repr(C, packed)]
struct VirtqUsed { flags: u16, idx: u16, ring: [VirtqUsedElem; 32] }

struct VirtQueue {
    size: u16,
    notify_off: u16,
    free_head: u16,
    num_free: u16,
    last_used_idx: u16,
    desc: *mut VirtqDesc,
    avail: *mut VirtqAvail,
    used: *mut VirtqUsed,
}

unsafe impl Send for VirtQueue {}
unsafe impl Sync for VirtQueue {}

impl VirtQueue {
    unsafe fn alloc(id: u16, cfg: *mut VirtioPciCommonCfg) -> Option<Self> {
        core::ptr::addr_of_mut!((*cfg).queue_select).mmio_write(id);
        let reported = core::ptr::addr_of!((*cfg).queue_size).mmio_read();
        if reported == 0 { return None; }

        // The descriptor table, avail ring and used ring each live in a single
        // 4096-byte page.  A VirtqDesc is 16 bytes, so one page holds at most 256
        // descriptors.  If the device reports a larger max (or a bogus 0xFFFF,
        // which is what an unmapped BAR reads back), cap the queue and tell the
        // device via queue_size — otherwise the init loop below overruns the
        // page by up to ~1 MiB and corrupts adjacent allocations (page tables,
        // other queues), which manifests as spurious MMIO faults and QEMU
        // "Desc next is N" errors.
        const MAX_QUEUE: u16 = 256;
        let size = if reported > MAX_QUEUE { MAX_QUEUE } else { reported };
        if size != reported {
            core::ptr::addr_of_mut!((*cfg).queue_size).mmio_write(size);
        }
        let raw_notify_off = core::ptr::addr_of!((*cfg).queue_notify_off).mmio_read();
        // 0xFFFF is QEMU's signal that legacy I/O-port notification should be
        // used (not available on AArch64).  Treat it as 0: queue N's doorbell
        // is at the start of the NOTIFY_CFG region, which QEMU processes as a
        // valid queue notification for all transitional virtio-blk devices.
        let notify_off = if raw_notify_off == 0xFFFF { 0 } else { raw_notify_off };

        let desc_phys = mm::buddy::alloc(0)?;
        let avail_phys = mm::buddy::alloc(0)?;
        let used_phys  = mm::buddy::alloc(0)?;

        let desc = mm::phys_to_virt(desc_phys) as *mut VirtqDesc;
        let avail = mm::phys_to_virt(avail_phys) as *mut VirtqAvail;
        let used  = mm::phys_to_virt(used_phys) as *mut VirtqUsed;

        core::ptr::write_bytes(desc  as *mut u8, 0, 4096);
        core::ptr::write_bytes(avail as *mut u8, 0, 4096);
        core::ptr::write_bytes(used  as *mut u8, 0, 4096);

        for i in 0..(size - 1) { (*desc.add(i as usize)).next = i + 1; }
        (*desc.add((size - 1) as usize)).next = 0xFFFF;

        core::ptr::addr_of_mut!((*cfg).queue_desc).write_volatile(desc_phys as u64);
        core::ptr::addr_of_mut!((*cfg).queue_driver).write_volatile(avail_phys as u64);
        core::ptr::addr_of_mut!((*cfg).queue_device).write_volatile(used_phys as u64);
        core::ptr::addr_of_mut!((*cfg).queue_enable).mmio_write(1u16);

        Some(Self { size, notify_off, free_head: 0, num_free: size, last_used_idx: 0, desc, avail, used })
    }

    unsafe fn alloc_desc(&mut self, addr: u64, len: u32, flags: u16) -> u16 {
        let id = self.free_head;
        let d = self.desc.add(id as usize);
        self.free_head = (*d).next;
        self.num_free -= 1;
        (*d).addr = addr; (*d).len = len; (*d).flags = flags; (*d).next = 0;
        id
    }

    unsafe fn chain(&mut self, from: u16, to: u16) {
        let d = self.desc.add(from as usize);
        (*d).flags |= VIRTQ_DESC_F_NEXT;
        (*d).next = to;
    }

    unsafe fn submit(&mut self, head: u16) {
        let a = self.avail;
        let idx_ptr = core::ptr::addr_of_mut!((*a).idx);
        let cur_idx = idx_ptr.mmio_read();
        let ring_slot = cur_idx as usize % self.size as usize;
        // Use manual offset to avoid out-of-bounds on the 32-element ring array
        let ring_ptr = (a as usize + 4) as *mut u16;
        ring_ptr.add(ring_slot).write_volatile(head);

        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        idx_ptr.mmio_write(cur_idx.wrapping_add(1));
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    }

    unsafe fn free_chain(&mut self, mut head: u16) {
        loop {
            let d = self.desc.add(head as usize);
            let flags = (*d).flags;
            let next = (*d).next;
            (*d).next = self.free_head;
            self.free_head = head;
            self.num_free += 1;
            if (flags & VIRTQ_DESC_F_NEXT) != 0 { head = next; } else { break; }
        }
    }

    /// Poll the used ring for the one in-flight request.
    ///
    /// The driver takes no completion interrupt: it reads the used ring
    /// itself, so a lost IRQ cannot strand a request. Callers include the
    /// page-fault path with IRQs masked, so every 256 polls we open an IRQ
    /// window (as nvme does) and read `device_status`: all-ones (gone) or
    /// NEEDS_RESET/FAILED fails at once. A request that is merely slow (a
    /// host too loaded to schedule QEMU's block backend) is waited for, with
    /// a warning at `slow_ns` and each doubling after. Returns whether it was
    /// slow. On failure the request is still owned by the device, so the
    /// caller must not touch the ring or DMA pages again.
    unsafe fn wait_for_completion(&mut self, cfg: *mut VirtioPciCommonCfg, t0: u64, slow_ns: u64,
                                  type_: u32, sector: u64, armed: bool) -> Result<bool, WaitErr> {
        let mut spins: u32 = 0;
        let used_idx = core::ptr::addr_of!((*self.used).idx);
        let last = self.last_used_idx;
        let armed = armed && !BLK_IRQ_DEAD.load(core::sync::atomic::Ordering::Relaxed);
        let irq0 = BLK_IRQS.load(core::sync::atomic::Ordering::Relaxed);
        let mut armed = armed;
        if armed {
            // Hybrid wait: most requests finish within microseconds, and a
            // wfi/IPI wake costs far more than that. Spin first; park only
            // when the device is slower than the window.
            let win = spin_window_ns();
            let s0 = sched::monotonic_ns();
            let mut n = 0u32;
            while self.last_used_idx == used_idx.read_volatile() {
                core::hint::spin_loop();
                n = n.wrapping_add(1);
                if n % 16 == 0 && sched::monotonic_ns().saturating_sub(s0) >= win { break; }
            }
            if self.last_used_idx != used_idx.read_volatile() { armed = false; ST_SPIN_HIT.fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
        }
        ST_WAITS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let parked = armed;
        let _w = if armed { Some(WaitingCpu::enter()) } else { None };
        let mut warn_at = slow_ns;
        let mut slow = false;
        while self.last_used_idx == core::ptr::addr_of!((*self.used).idx).read_volatile() {
            spins = spins.wrapping_add(1);
            if armed {
                use core::sync::atomic::Ordering::Relaxed;
                ST_PARK.fetch_add(1, Relaxed);
                let i0 = BLK_IRQS.load(Relaxed);
                let p0 = sched::monotonic_ns();
                if park_until_irq(|| used_idx.read_volatile() != last) {
                    ST_HLT.fetch_add(1, Relaxed);
                    let dt = sched::monotonic_ns().saturating_sub(p0);
                    if BLK_IRQS.load(Relaxed) != i0 {
                        ST_WAKE_IRQ.fetch_add(1, Relaxed);
                        ST_WAKE_IRQ_NS.fetch_add(dt, Relaxed);
                    } else {
                        ST_WAKE_OTHER.fetch_add(1, Relaxed);
                        ST_WAKE_OTHER_NS.fetch_add(dt, Relaxed);
                        if used_idx.read_volatile() != last { ST_WAKE_OTHER_DONE.fetch_add(1, Relaxed); }
                    }
                } else {
                    ST_RECHECK_HIT.fetch_add(1, Relaxed);
                }
            }
            if armed || spins % 256 == 0 {
                // A syscall (aarch64 reads run IRQ-masked with preemption
                // enabled) or a page fault: take the tick.
                irq_window_if_masked();
                let st = core::ptr::addr_of!((*cfg).device_status).mmio_read();
                if st == 0xFF || st & (VIRTIO_STATUS_NEEDS_RESET | VIRTIO_STATUS_FAILED) != 0 {
                    return Err(WaitErr::Status(st));
                }
                let el = sched::monotonic_ns().saturating_sub(t0);
                if el > warn_at {
                    while warn_at <= el && warn_at != u64::MAX { warn_at = warn_at.saturating_mul(2); }
                    slow = true;
                    crate::kenv::pci::serial_debug(&alloc::format!(
                        "[VIRTIO-BLK] slow request: type {} sector {} unanswered for {} ms (device_status {:#04x}); still waiting\n",
                        type_, sector, el / 1_000_000, st));
                }
            }
            if !armed { core::hint::spin_loop(); }
        }
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        if parked { irq_health(BLK_IRQS.load(core::sync::atomic::Ordering::Relaxed) != irq0); }
        Ok(slow)
    }
}

// ── Completion interrupt ─────────────────────────────────────────────────────
//
// A request waiter parks its CPU (`wfi` / `hlt`) instead of spinning on the
// used ring; the completion interrupt (MSI-X on x86_64, the shared INTx SPI on
// aarch64) wakes it. The interrupt may land on another CPU, so the handler
// sends a reschedule IPI to every CPU with a parked waiter. The 100 Hz tick
// still wakes a parked CPU, so a lost interrupt costs one tick, never a hang.
// Callers that cannot sleep (page faults, IRQs masked) park the same way: the
// sequence opens its own interrupt window. Boot-time requests (pid 0) spin.

/// CPUs with a parked request waiter (bit per CPU id).
static WAITING_CPUS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Completion interrupts taken (diagnostic).
static BLK_IRQS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Per-disk ISR status byte and INTx INTID (0 = none), for the shared-line
/// handler; slot = arming order.
static BLK_ISR: [core::sync::atomic::AtomicUsize; MAX_BLK_DEVICES] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; MAX_BLK_DEVICES];
static BLK_INTID: [core::sync::atomic::AtomicU32; MAX_BLK_DEVICES] =
    [const { core::sync::atomic::AtomicU32::new(0) }; MAX_BLK_DEVICES];
static BLK_ARMED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Set when completions keep arriving with no interrupt (line masked by the
/// storm guard, or routing broken): waiters go back to spinning for good.
static BLK_IRQ_DEAD: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static BLK_IRQ_MISSES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
const IRQ_MISS_LIMIT: u32 = 64;

/// Record whether a parked wait saw its completion interrupt.
fn irq_health(saw_irq: bool) {
    use core::sync::atomic::Ordering::Relaxed;
    if saw_irq { BLK_IRQ_MISSES.store(0, Relaxed); return; }
    if BLK_IRQ_MISSES.fetch_add(1, Relaxed) + 1 == IRQ_MISS_LIMIT {
        BLK_IRQ_DEAD.store(true, Relaxed);
        crate::kenv::pci::serial_debug("[VIRTIO-BLK] completions arrive without interrupts; back to polling\n");
    }
}

/// `virtio_blk.irq=0` on the kernel command line forces the spin wait. Read on
/// the first task-context wait: the command line is not available at probe time.
fn irq_knob_on() -> bool {
    use core::sync::atomic::Ordering::Relaxed;
    static KNOB: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
    match KNOB.load(Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = crate::kenv::cmdline_word("virtio_blk.irq") != Some("0");
            KNOB.store(if on { 1 } else { 2 }, Relaxed);
            if !on { crate::kenv::pci::serial_debug("[VIRTIO-BLK] virtio_blk.irq=0: completion interrupt off, spinning\n"); }
            on
        }
    }
}

/// `virtio_blk.spin_us=N`: a waiter polls the used ring this long before it
/// parks on the completion interrupt (default 50). Read lazily like `irq`.
fn spin_window_ns() -> u64 {
    use core::sync::atomic::Ordering::Relaxed;
    static NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(u64::MAX);
    let v = NS.load(Relaxed);
    if v != u64::MAX { return v; }
    let us = crate::kenv::cmdline_word("virtio_blk.spin_us")
        .and_then(|w| w.parse::<u64>().ok()).unwrap_or(50).min(100_000);
    NS.store(us * 1000, Relaxed);
    us * 1000
}

/// Registers this CPU as a parked waiter for the guard's lifetime.
struct WaitingCpu(u64);
impl WaitingCpu {
    fn enter() -> Self {
        // The device lock is a sleeping mutex: its holder can be preempted and
        // migrated mid-request. Stay on this CPU while its bit is set, so the
        // completion IRQ kick finds the CPU that really waits and Drop clears
        // the same bit.
        sched::preempt_disable();
        let cpu = crate::kenv::ipi::cpu_id();
        let bit = if cpu < 64 { 1u64 << cpu } else { 0 };
        WAITING_CPUS.fetch_or(bit, core::sync::atomic::Ordering::SeqCst);
        WaitingCpu(bit)
    }
}
impl Drop for WaitingCpu {
    fn drop(&mut self) {
        WAITING_CPUS.fetch_and(!self.0, core::sync::atomic::Ordering::SeqCst);
        sched::preempt_enable_no_resched();
    }
}

/// Park the CPU until an interrupt arrives unless `done()` already holds. The
/// check and the park are atomic against the completion interrupt (aarch64
/// `wfi` wakes on a pending masked interrupt; x86_64 `sti; hlt` recognises it
/// at the `hlt`). IRQ state is preserved: a masked caller gets an explicit
/// window (preemption off, so the tick cannot switch tasks under the device
/// lock); an unmasked one has IRQs re-enabled on return.
fn park_until_irq(done: impl Fn() -> bool) -> bool {
    let mut halted = false;
    unsafe {
        if sched::irqs_masked() {
            sched::preempt_disable();
            if !done() {
                halted = true;
                #[cfg(target_arch = "aarch64")]
                core::arch::asm!("wfi", options(nomem, nostack));
                #[cfg(target_arch = "x86_64")]
                core::arch::asm!("sti; hlt; cli", options(nomem, nostack));
            }
            sched::irq_window();
            sched::preempt_enable_no_resched();
        } else {
            #[cfg(target_arch = "aarch64")]
            {
                core::arch::asm!("msr daifset, #2", options(nomem, nostack));
                if !done() { halted = true; core::arch::asm!("wfi", options(nomem, nostack)); }
                core::arch::asm!("msr daifclr, #2", options(nomem, nostack));
            }
            #[cfg(target_arch = "x86_64")]
            {
                core::arch::asm!("cli", options(nomem, nostack));
                if !done() { halted = true; core::arch::asm!("sti; hlt", options(nomem, nostack)); }
                else { core::arch::asm!("sti", options(nomem, nostack)); }
            }
        }
    }
    halted
}

/// Wait statistics (`/proc/interrupts` "virtio-blk wait:" line): why each
/// park ended, to tell a lost or late wakeup from a timely IRQ.
static ST_WAITS: AtomicU64 = AtomicU64::new(0);
static ST_SPIN_HIT: AtomicU64 = AtomicU64::new(0);
static ST_PARK: AtomicU64 = AtomicU64::new(0);
static ST_RECHECK_HIT: AtomicU64 = AtomicU64::new(0);
static ST_HLT: AtomicU64 = AtomicU64::new(0);
static ST_WAKE_IRQ: AtomicU64 = AtomicU64::new(0);
static ST_WAKE_OTHER: AtomicU64 = AtomicU64::new(0);
static ST_WAKE_OTHER_DONE: AtomicU64 = AtomicU64::new(0);
static ST_WAKE_OTHER_NS: AtomicU64 = AtomicU64::new(0);
static ST_WAKE_IRQ_NS: AtomicU64 = AtomicU64::new(0);

/// Append the wait statistics line to `/proc/interrupts`.
pub fn wait_stats(w: &mut dyn core::fmt::Write) {
    use core::sync::atomic::Ordering::Relaxed;
    let _ = writeln!(w,
        "virtio-blk wait: waits={} spin_hit={} park={} recheck_hit={} hlt={} wake_irq={} wake_other={} wake_other_done={} wake_irq_us={} wake_other_us={} irqs={}",
        ST_WAITS.load(Relaxed), ST_SPIN_HIT.load(Relaxed), ST_PARK.load(Relaxed), ST_RECHECK_HIT.load(Relaxed),
        ST_HLT.load(Relaxed), ST_WAKE_IRQ.load(Relaxed), ST_WAKE_OTHER.load(Relaxed), ST_WAKE_OTHER_DONE.load(Relaxed),
        ST_WAKE_IRQ_NS.load(Relaxed) / 1000, ST_WAKE_OTHER_NS.load(Relaxed) / 1000, BLK_IRQS.load(Relaxed));
}

/// Completion handler. `ctx` is 0 for a private MSI-X vector, else the INTID of
/// a shared INTx line (aarch64): every disk on that line has its ISR status
/// byte read, which deasserts the level line; a line none of them owns is
/// `None` for the other sharers.
extern "C" fn virtio_blk_irq(ctx: usize) -> crate::kenv::irq::IrqReturn {
    use core::sync::atomic::Ordering::{Acquire, Relaxed, SeqCst};
    if ctx != 0 {
        let mut ours = false;
        for i in 0..MAX_BLK_DEVICES {
            if BLK_INTID[i].load(Acquire) as usize != ctx { continue; }
            let p = BLK_ISR[i].load(Acquire) as *const u8;
            if !p.is_null() && unsafe { p.mmio_read() } & 1 != 0 { ours = true; }
        }
        if !ours { return crate::kenv::irq::IrqReturn::None; }
    }
    if BLK_IRQS.fetch_add(1, Relaxed) == 0 {
        crate::kenv::pci::serial_debug("[VIRTIO-BLK] first completion interrupt\n");
    }
    let m = WAITING_CPUS.load(SeqCst);
    if m != 0 { crate::kenv::ipi::kick_cpus(m); }
    crate::kenv::irq::IrqReturn::Handled
}

impl VirtioBlkDevice {
    /// Arm the queue's completion interrupt; any failure leaves the disk on the
    /// spin path (the pre-existing, always-working fallback).
    unsafe fn arm_irq(&mut self) {
        #[allow(unused_imports)]
        use core::sync::atomic::Ordering::{Acquire, Release};
        let pci = &self._pci;
        #[allow(unused_variables)]
        let func = pci.function();
        #[cfg(target_arch = "x86_64")]
        if let Ok(tab) = crate::kenv::irq::MsixTable::enable(func) {
            const NO_VECTOR: u16 = 0xFFFF;
            if let Ok(h) = tab.request(0, virtio_blk_irq, 0) {
                let cfg = self.common_cfg;
                core::ptr::addr_of_mut!((*cfg).config_msix_vector).mmio_write(NO_VECTOR);
                core::ptr::addr_of_mut!((*cfg).queue_select).mmio_write(0);
                core::ptr::addr_of_mut!((*cfg).queue_msix_vector).mmio_write(0);
                if core::ptr::addr_of!((*cfg).queue_msix_vector).mmio_read() == 0 {
                    self.irq_armed = true;
                    crate::kenv::pci::serial_debug("[VIRTIO-BLK] MSI-X armed: vector ");
                    crate::kenv::pci::serial_debug_hex(h.vector().unwrap_or(0) as u32);
                    crate::kenv::pci::serial_debug("\n");
                }
            }
            return;
        }
        #[cfg(target_arch = "aarch64")]
        {
            let Some(isr) = self.isr_cfg else { return };
            let Some(intid) = func.intx_line() else { return };
            let slot = BLK_ARMED.load(Acquire);
            if slot >= MAX_BLK_DEVICES { return; }
            // Publish the ISR pointer, drain stale state, make sure the line has
            // a handler (one per INTID, shared by all disks on it), only then lift
            // INTx Disable.
            let _ = isr.mmio_read();
            BLK_ISR[slot].store(isr as usize, Release);
            BLK_INTID[slot].store(intid, Release);
            let have = (0..slot).any(|i| BLK_INTID[i].load(Acquire) == intid);
            if !have {
                match crate::kenv::irq::request_irq(crate::kenv::irq::IrqSource::Line(intid), virtio_blk_irq, intid as usize) {
                    Ok(h) => core::mem::forget(h),
                    Err(_) => {
                        BLK_INTID[slot].store(0, Release);
                        BLK_ISR[slot].store(0, Release);
                        crate::kenv::pci::serial_debug("[VIRTIO-BLK] INTx: line registration refused; polling only\n");
                        return;
                    }
                }
            }
            BLK_ARMED.store(slot + 1, Release);
            let cmd = pci_read_config_16(pci.bus, pci.dev, pci.func, 0x04);
            pci_write_config_16(pci.bus, pci.dev, pci.func, 0x04, cmd & !crate::kenv::pci::PCI_CMD_INTX_DISABLE);
            let _ = isr.mmio_read();
            self.irq_armed = true;
            crate::kenv::pci::serial_debug("[VIRTIO-BLK] INTx armed: INTID ");
            crate::kenv::pci::serial_debug_hex(intid);
            crate::kenv::pci::serial_debug("\n");
        }
    }
}

// ── VirtIO block request header ──────────────────────────────────────────────

#[repr(C, packed)]
struct VirtioBlkReqHdr { type_: u32, reserved: u32, sector: u64 }

// ── Per-device state ─────────────────────────────────────────────────────────

struct VirtioBlkDevice {
    _pci: PciDevice,
    common_cfg: *mut VirtioPciCommonCfg,
    notify_cfg: *mut u32,
    notify_off_multiplier: u32,
    queue: VirtQueue,
    // Pre-allocated DMA pages reused per request (single-threaded polling).
    req_phys:  usize, // 4096-byte page: [VirtioBlkReqHdr (16B)] [status (1B)]
    data_phys: usize, // 4096-byte page: sector data
    /// Device capacity in 512-byte sectors, from VIRTIO_PCI_CAP_DEVICE_CFG.
    /// 0 if the device-config capability wasn't found (capacity unknown).
    capacity_sectors: u64,
    /// Set when a request failed to complete (timeout or device failure);
    /// every later request fails fast with `BlkErr::Io`.
    dead: bool,
    /// A completion interrupt is armed (MSI-X on x86_64, shared INTx SPI on
    /// aarch64): a waiter parks the CPU instead of spinning the used ring.
    irq_armed: bool,
    isr_cfg: Option<*const u8>,
}

unsafe impl Send for VirtioBlkDevice {}
unsafe impl Sync for VirtioBlkDevice {}

// ── Cap walk helpers ──────────────────────────────────────────────────────────

/// Enable PCI Memory Space (bit 1) and Bus Master (bit 2) for `pci`.
///
/// Without these the device's BARs do not decode (MMIO reads return all-ones)
/// and it performs no virtqueue DMA.  UEFI firmware normally sets them, but it
/// does not reliably cover every device (e.g. the third virtio-blk device on
/// the aarch64 `virt` machine), so the driver must enable them itself.
unsafe fn enable_pci_mmio(pci: &PciDevice) {
    let cmd = pci_read_config_16(pci.bus, pci.dev, pci.func, 0x04);
    // Memory Space + Bus Master on; INTx Disable (bit 10) on: this driver
    // polls and never reads its ISR status, so an asserted INTx would stay
    // asserted forever and, on the virt board's shared PCIe lines, storm the
    // GPU's completion interrupt (`virtio_gpu::enable_intx`).
    pci_write_config_16(pci.bus, pci.dev, pci.func, 0x04, (cmd | 0x0006) | crate::kenv::pci::PCI_CMD_INTX_DISABLE);
}

/// Resolve a BAR index to its 64-bit MMIO base address, or 0 if I/O space.
fn bar64(pci: &PciDevice, bar_idx: usize) -> u64 {
    let raw = pci.bars[bar_idx];
    if raw & 1 != 0 { return 0; } // I/O space
    if (raw >> 1) & 3 == 2 && bar_idx + 1 < 6 {
        (raw & !0xF) as u64 | ((pci.bars[bar_idx + 1] as u64) << 32)
    } else {
        (raw & !0xF) as u64
    }
}

/// Walk the PCI cap list, map each VirtIO MMIO region, and return pointers to
/// COMMON_CFG and NOTIFY_CFG together with the notify_off_multiplier.
unsafe fn walk_caps(
    pci: &PciDevice,
) -> (Option<*mut VirtioPciCommonCfg>, Option<*mut u32>, u32, Option<*const u64>, Option<*const u8>) {
    let mut common_cfg: Option<*mut VirtioPciCommonCfg> = None;
    let mut notify_cfg: Option<*mut u32> = None;
    let mut notify_off_multiplier = 0u32;
    let mut device_cfg: Option<*const u64> = None;
    let mut isr_cfg: Option<*const u8> = None;

    let mut cap_ptr = pci_read_config_8(pci.bus, pci.dev, pci.func, 0x34);
    while cap_ptr != 0 {
        let cap_id = pci_read_config_8(pci.bus, pci.dev, pci.func, cap_ptr);
        if cap_id == 0x09 {
            let cfg_type = pci_read_config_8(pci.bus, pci.dev, pci.func, cap_ptr + 3);
            let bar_idx  = pci_read_config_8(pci.bus, pci.dev, pci.func, cap_ptr + 4) as usize;
            let offset   = pci_read_config_32(pci.bus, pci.dev, pci.func, cap_ptr + 8);
            let length   = pci_read_config_32(pci.bus, pci.dev, pci.func, cap_ptr + 12);

            if bar_idx < 6 {
                let base = bar64(pci, bar_idx);
                if base != 0 {
                    let phys = base as usize + offset as usize;
                    let virt = mm::paging::map_kernel_device(
                        phys, length as usize,
                        mm::paging::PageFlags::PRESENT
                            | mm::paging::PageFlags::WRITABLE
                            | mm::paging::PageFlags::MMIO,
                    ).unwrap_or_else(|| mm::phys_to_virt(phys));

                    match cfg_type {
                        VIRTIO_PCI_CAP_COMMON_CFG => {
                            common_cfg = Some(virt as *mut VirtioPciCommonCfg);
                        }
                        VIRTIO_PCI_CAP_NOTIFY_CFG => {
                            notify_off_multiplier =
                                pci_read_config_32(pci.bus, pci.dev, pci.func, cap_ptr + 16);
                            notify_cfg = Some(virt as *mut u32);
                        }
                        VIRTIO_PCI_CAP_DEVICE_CFG => {
                            // struct virtio_blk_config's first field is `capacity`
                            // (u64, in 512-byte sectors) at offset 0.
                            device_cfg = Some(virt as *const u64);
                        }
                        VIRTIO_PCI_CAP_ISR_CFG => { isr_cfg = Some(virt as *const u8); }
                        _ => {}
                    }
                }
            }
        }
        cap_ptr = pci_read_config_8(pci.bus, pci.dev, pci.func, cap_ptr + 1);
    }

    (common_cfg, notify_cfg, notify_off_multiplier, device_cfg, isr_cfg)
}

/// Write device_status=0 to reset `pci` without touching its virtqueues.
///
/// Called in a first pass before any queue setup to avoid the QEMU
/// blk_drain() re-entrancy deadlock: after two devices have queue_enable=1,
/// writing device_status=0 to a third triggers blk_drain() from inside an
/// MMIO write handler, where aio_poll() cannot run — causing a deadlock.
/// Resetting all devices first means blk_drain() sees no pending I/O.
unsafe fn pre_reset_device(pci: &PciDevice) {
    enable_pci_mmio(pci);
    let (common_cfg, _, _, _, _) = walk_caps(pci);
    if let Some(cfg) = common_cfg {
        core::ptr::addr_of_mut!((*cfg).device_status).mmio_write(0u8);
    }
}

impl VirtioBlkDevice {
    /// Initialize a VirtIO block device.
    ///
    /// `reset_done`: if true, device_status=0 was already written in a prior
    /// pass and this function starts from ACKNOWLEDGE.
    unsafe fn new(pci: PciDevice, reset_done: bool) -> Option<Self> {
        enable_pci_mmio(&pci);
        let (common_cfg, notify_cfg, notify_off_multiplier, device_cfg, isr_cfg) = walk_caps(&pci);
        let common_cfg = common_cfg?;
        let notify_cfg = notify_cfg?;
        let capacity_sectors = device_cfg.map_or(0, |p| p.mmio_read());

        let ds = core::ptr::addr_of_mut!((*common_cfg).device_status);

        if !reset_done {
            ds.mmio_write(0u8);
        }
        ds.mmio_write(VIRTIO_STATUS_ACKNOWLEDGE);
        ds.mmio_write(VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER);

        // Feature bits 0-31: no optional features needed.
        core::ptr::addr_of_mut!((*common_cfg).device_feature_select).mmio_write(0u32);
        let _ = core::ptr::addr_of!((*common_cfg).device_feature).mmio_read();
        core::ptr::addr_of_mut!((*common_cfg).driver_feature_select).mmio_write(0u32);
        core::ptr::addr_of_mut!((*common_cfg).driver_feature).mmio_write(0u32);

        // Feature bit 32 = VIRTIO_F_VERSION_1: mandatory for the modern PCI
        // transport.  Without it, transitional devices (0x1001) stay in legacy
        // mode and return queue_notify_off=0xFFFF to signal "use I/O-port
        // notification" — which doesn't exist on AArch64 — producing a wildly
        // wrong doorbell address.
        core::ptr::addr_of_mut!((*common_cfg).device_feature_select).mmio_write(1u32);
        let dev_feat_hi = core::ptr::addr_of!((*common_cfg).device_feature).mmio_read();
        const VIRTIO_F_VERSION_1: u32 = 1 << 0; // bit 32 of the full feature set
        if dev_feat_hi & VIRTIO_F_VERSION_1 != 0 {
            core::ptr::addr_of_mut!((*common_cfg).driver_feature_select).mmio_write(1u32);
            core::ptr::addr_of_mut!((*common_cfg).driver_feature).mmio_write(VIRTIO_F_VERSION_1);
        }

        ds.mmio_write(
            VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK,
        );
        // Verify FEATURES_OK was accepted; if not, features negotiation failed.
        if ds.mmio_read() & VIRTIO_STATUS_FEATURES_OK == 0 {
            return None;
        }

        let queue = VirtQueue::alloc(0, common_cfg)?;

        ds.mmio_write(
            VIRTIO_STATUS_ACKNOWLEDGE
                | VIRTIO_STATUS_DRIVER
                | VIRTIO_STATUS_FEATURES_OK
                | VIRTIO_STATUS_DRIVER_OK,
        );

        let req_phys  = mm::buddy::alloc(0)?;
        let data_phys = mm::buddy::alloc(DATA_BUF_ORDER)?;
        core::ptr::write_bytes(mm::phys_to_virt(req_phys)  as *mut u8, 0, 4096);
        core::ptr::write_bytes(mm::phys_to_virt(data_phys) as *mut u8, 0,
                               MAX_IO_BLOCKS * BLOCK_SIZE);

        Some(Self {
            _pci: pci,
            common_cfg,
            notify_cfg,
            notify_off_multiplier,
            queue,
            req_phys,
            data_phys,
            capacity_sectors,
            dead: false,
            irq_armed: false,
            isr_cfg,
        })
    }

    fn notify(&self) {
        unsafe {
            let notify_addr = (self.notify_cfg as usize
                + self.queue.notify_off as usize * self.notify_off_multiplier as usize)
                as *mut u16;
            leandros_lib::mmio::write(notify_addr, 0);
        }
    }

    // Returns true on success (VIRTIO_BLK_S_OK == 0).
    // `span` must be a non-zero multiple of 512 no larger than MAX_IO_BYTES;
    // `buf` covers `span` bytes. `sector` is in 512-byte units.
    fn do_io(&mut self, type_: u32, sector: u64, buf: *mut u8, span: usize) -> bool {
        // FLUSH is the one request type with no data buffer: header + status
        // only. Everything else must name at least one sector.
        let is_flush = type_ == VIRTIO_BLK_T_FLUSH;
        if !is_flush && (span == 0 || span % SECTOR != 0 || span > MAX_IO_BYTES) { return false; }
        if self.dead { return false; }
        unsafe {
            // Fill request header
            let hdr = mm::phys_to_virt(self.req_phys) as *mut VirtioBlkReqHdr;
            (*hdr).type_ = type_;
            (*hdr).reserved = 0;
            (*hdr).sector = sector;
            // Status byte at offset 16 of the request page
            let status_phys = self.req_phys + 16;
            let status_ptr = mm::phys_to_virt(status_phys) as *mut u8;
            *status_ptr = 0xFF; // sentinel; device overwrites with 0=ok

            if type_ == VIRTIO_BLK_T_OUT {
                // Copy caller data into DMA buffer before write
                core::ptr::copy_nonoverlapping(
                    buf,
                    mm::phys_to_virt(self.data_phys) as *mut u8,
                    span,
                );
            }

            let d0 = {
                let q = &mut self.queue;
                let d0 = q.alloc_desc(self.req_phys as u64, 16, 0);
                let d2 = q.alloc_desc(status_phys as u64, 1, VIRTQ_DESC_F_WRITE);
                if is_flush {
                    // No data descriptor: a FLUSH that carries one is a spec
                    // violation and QEMU rejects the whole request.
                    q.chain(d0, d2);
                } else {
                    let d1 = q.alloc_desc(
                        self.data_phys as u64,
                        span as u32,
                        if type_ == VIRTIO_BLK_T_IN { VIRTQ_DESC_F_WRITE } else { 0 },
                    );
                    q.chain(d0, d1);
                    q.chain(d1, d2);
                }
                q.submit(d0);
                d0
            };
            let t0 = sched::monotonic_ns();
            let k = knobs();
            let seq = REQ_SEQ.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
            let every = k.every_k != 0 && seq % k.every_k == 0;
            if (k.delay_n != 0 && seq == k.delay_n) || every {
                let delay_ns = if every { k.every_ns } else { k.delay_ns };
                // Injected slow device: the request sits in the ring, unseen,
                // for delay_ns. The wait below counts from t0.
                crate::kenv::pci::serial_debug(&alloc::format!(
                    "[VIRTIO-BLK] inject: request #{} (type {} sector {}) delayed {} ms\n",
                    seq, type_, sector, delay_ns / 1_000_000));
                while sched::monotonic_ns().saturating_sub(t0) < delay_ns {
                    irq_window_if_masked();
                    for _ in 0..256 { core::hint::spin_loop(); }
                }
            }
            self.notify();
            match self.queue.wait_for_completion(self.common_cfg, t0, k.slow_ns, type_, sector,
                self.irq_armed && sched::current_pid() != 0 && irq_knob_on()) {
                Ok(false) => {}
                Ok(true) => crate::kenv::pci::serial_debug(&alloc::format!(
                    "[VIRTIO-BLK] slow request: type {} sector {} completed after {} ms (status {})\n",
                    type_, sector, sched::monotonic_ns().saturating_sub(t0) / 1_000_000, *status_ptr)),
                Err(WaitErr::Status(st)) => {
                    // The descriptors and DMA pages stay with the device
                    // forever: it may still complete (and DMA) later, so
                    // never reuse them.
                    self.dead = true;
                    crate::kenv::pci::serial_debug(&alloc::format!(
                        "[VIRTIO-BLK] request type {} sector {} device failed (device_status {:#04x}); disk marked dead\n",
                        type_, sector, st));
                    return false;
                }
            }
            self.queue.free_chain(d0);

            if type_ == VIRTIO_BLK_T_IN {
                // Copy DMA buffer into caller buffer after read
                core::ptr::copy_nonoverlapping(
                    mm::phys_to_virt(self.data_phys) as *const u8,
                    buf,
                    span,
                );
            }

            *status_ptr == 0
        }
    }
}

// ── BlockDevice adapter ───────────────────────────────────────────────────────

/// One probed virtio-blk disk as registered with `crate::blkdev`. The device
/// state (virtqueue + DMA bounce pages) sits behind its own lock, so I/O on
/// vdb never waits for I/O on vdc.
pub struct VirtioDisk {
    /// A sleeping FIFO mutex (`sched::iolock`): the holder waits for the device, possibly for
    /// seconds when the host is starved, and a `spin::Mutex` made every waiter spin with IRQs
    /// masked, so CPUs missed their timer ticks (`[WDOG]`; lane f2fswatchdog). Callers that
    /// cannot sleep spin with IRQ windows instead.
    dev: sched::iolock::IoMutex<VirtioBlkDevice>,
    capacity_sectors: u64,
}

impl VirtioDisk {
    fn rw(&self, type_: u32, lba: u64, buf: *mut u8, len: usize) -> Result<(), crate::blockdev::BlkErr> {
        self.rw_impl(type_, lba, buf, len, false)
    }
    /// `try`: fail with `Io` instead of waiting for the device lock (panic path).
    fn rw_impl(&self, type_: u32, lba: u64, buf: *mut u8, len: usize, try_only: bool) -> Result<(), crate::blockdev::BlkErr> {
        use crate::blockdev::BlkErr;
        if len == 0 || len % SECTOR != 0 { return Err(BlkErr::BadBuffer); }
        if self.capacity_sectors != 0 && lba + (len / SECTOR) as u64 > self.capacity_sectors {
            return Err(BlkErr::OutOfRange);
        }
        let mut dev = if try_only {
            match self.dev.try_lock() { Some(g) => g, None => return Err(BlkErr::Io) }
        } else {
            match self.dev.lock_bounded() { Some(g) => g, None => return Err(BlkErr::Io) }
        };
        let mut done = 0usize;
        while done < len {
            let n = (len - done).min(MAX_IO_BYTES);
            let ok = dev.do_io(type_, lba + (done / SECTOR) as u64,
                               unsafe { buf.add(done) }, n);
            if !ok { return Err(BlkErr::Io); }
            done += n;
        }
        Ok(())
    }
}

impl crate::blockdev::BlockDevice for VirtioDisk {
    fn sector_size(&self) -> u32 { SECTOR as u32 }
    fn capacity_sectors(&self) -> u64 { self.capacity_sectors }
    fn read_sectors(&self, lba: u64, buf: &mut [u8]) -> Result<(), crate::blockdev::BlkErr> {
        self.rw(VIRTIO_BLK_T_IN, lba, buf.as_mut_ptr(), buf.len())
    }
    fn write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), crate::blockdev::BlkErr> {
        // do_io only reads from `buf` for T_OUT (copies it into the DMA page).
        self.rw(VIRTIO_BLK_T_OUT, lba, buf.as_ptr() as *mut u8, buf.len())
    }
    /// Panic-path write: never parks and never waits for the device lock.
    fn try_write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), crate::blockdev::BlkErr> {
        self.rw_impl(VIRTIO_BLK_T_OUT, lba, buf.as_ptr() as *mut u8, buf.len(), true)
    }
    /// Commit the device's volatile write cache to stable storage.
    ///
    /// A completed write request is not the same as the data being durable:
    /// the backend may still hold it in a writeback cache. Without this an
    /// `fsync` that flushed every kernel-side buffer would still be making a
    /// promise it could not keep.
    fn flush(&self) -> Result<(), crate::blockdev::BlkErr> {
        if self.dev.lock().do_io(VIRTIO_BLK_T_FLUSH, 0, core::ptr::null_mut(), 0) {
            Ok(())
        } else {
            Err(crate::blockdev::BlkErr::Io)
        }
    }
    fn removable(&self) -> bool { false }
    fn name_class(&self) -> &'static str { "vd" }
}

// ── Probe ─────────────────────────────────────────────────────────────────────

/// Probe every virtio-blk PCI function, in PCI scan order (modern IDs first,
/// then transitional — the order the old fixed table used, so QEMU's
/// vda/vdb/vdc keep their indices). Each device is leaked: disks are never
/// unplugged, and the registry holds `&'static` references.
pub fn probe_all() -> alloc::vec::Vec<&'static VirtioDisk> {
    // Pass 1: reset all devices before any queue setup.  This ensures that
    // when QEMU's blk_drain() runs for each reset it finds no pending I/O and
    // returns immediately, avoiding the aio_poll() re-entrancy deadlock that
    // occurs when device_status=0 is written after other queues are enabled.
    for device_id in [VIRTIO_PCI_DEVICE_BLK_MODERN, VIRTIO_PCI_DEVICE_BLK_LEGACY] {
        for pci in find_all_devices(VIRTIO_PCI_VENDOR, device_id) {
            unsafe { pre_reset_device(&pci); }
        }
    }

    // Pass 2: full initialization (reset already done).
    let mut out = alloc::vec::Vec::new();
    for device_id in [VIRTIO_PCI_DEVICE_BLK_MODERN, VIRTIO_PCI_DEVICE_BLK_LEGACY] {
        for pci in find_all_devices(VIRTIO_PCI_VENDOR, device_id) {
            if out.len() >= MAX_BLK_DEVICES {
                crate::kenv::pci::serial_debug("[VIRTIO-BLK] device limit reached (");
                crate::kenv::pci::serial_debug_dec(MAX_BLK_DEVICES as u64);
                crate::kenv::pci::serial_debug("); skipping a virtio-blk PCI function\n");
                continue;
            }
            if let Some(mut d) = unsafe { VirtioBlkDevice::new(pci, true) } {
                // The kernel command line is not handed over yet at probe time,
                // so `virtio_blk.irq=0` is checked per wait (`irq_knob_on`).
                unsafe { d.arm_irq(); }
                let capacity_sectors = d.capacity_sectors;
                let disk: &'static VirtioDisk = alloc::boxed::Box::leak(alloc::boxed::Box::new(
                    VirtioDisk { dev: sched::iolock::IoMutex::new(0, "VIRTIO_BLK", d), capacity_sectors },
                ));
                out.push(disk);
            }
        }
    }
    out
}
