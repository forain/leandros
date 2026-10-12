//! NVMe (PCI class 01:08:02) block driver: admin queue, Identify, one I/O
//! queue pair, polled completion, PRP lists, flush, and an orderly CC.SHN
//! shutdown on power-off/restart. Every active namespace is registered with
//! the block registry as `nvme<C>n<NSID>`; the registry then scans its
//! partition table (`nvme0n1p1`, `nvme0n1p2`, ...).
//!
//! # Read-only unless `nvme.rw=1`
//!
//! The first target is a laptop whose SSD holds another OS. Without
//! `nvme.rw=1` on the kernel command line the driver never sends a command
//! that can change stored data: no Write, Write Zeroes, Dataset Management,
//! Format, Sanitize, Firmware Commit, Namespace Management, and not even a
//! Flush. The only admin commands it issues are Identify, Get Log Page
//! (SMART / health, read-only), Get/Set Features for the volatile Number of
//! Queues, Autonomous Power State Transition and Power Management settings
//! (none saved: SV is never set), and Create/Delete I/O queue. Every
//! namespace reports `read_only()`, so the block layer refuses writes with
//! EROFS before they reach this file, and `write_sectors` refuses them again
//! here (one log line per controller).
//!
//! # Bring-up (Linux `nvme_pci_configure_admin_queue` + `nvme_init_ctrl`)
//!
//! 1. Memory decode + bus master on, INTx disabled (we poll; nothing may
//!    assert a line), every interrupt vector masked in INTMS.
//! 2. The controller may be in whatever state the firmware left it: if
//!    CC.EN=1 but CSTS.RDY=0 we first wait for RDY=1 (changing EN mid
//!    transition is undefined), then clear EN and wait for RDY=0. All waits
//!    are bounded by CAP.TO (500 ms units).
//! 3. Admin queues, CC (MPS = CAP.MPSMIN, IOSQES 6, IOCQES 4, NVM command
//!    set), EN=1, wait RDY=1. CSTS.CFS at any point fails the controller.
//! 4. Identify Controller, Set Features Number of Queues (1/1), Create I/O
//!    CQ then SQ (interrupts disabled on the CQ), Identify active namespace
//!    list (or 1..=NN on a 1.0 controller), Identify Namespace for each.
//! 5. APST (Samsung PM9xx-class drives have a history of falling off the bus
//!    from their deepest idle states, Linux NVME_QUIRK_NO_DEEPEST_PS /
//!    NO_APST): if the controller supports APST and it is enabled, it is
//!    turned off (volatile, `nvme.apst=keep` leaves it alone) and the power
//!    state is moved to PS0 if it was not there. We never enable APST.
//!
//! # I/O
//!
//! One command at a time per controller (a sleeping `KMutex` around the queue
//! state): an SSD answers a 4 KiB read in ~100 us, far below the cost of any
//! queueing we could add, and the root filesystem lives on the USB stick.
//! Data goes through a 256 KiB physically contiguous bounce buffer whose PRP
//! list is built once, so a command never needs more than PRP1 + PRP2. The
//! largest command is min(256 KiB, MDTS).
//!
//! Completion is polled: spin for the first 200 us, then — when the caller
//! may sleep — park in short slices, else keep spinning with interrupt
//! windows (the `usb_storage` contract). MSI/MSI-X are deliberately unused
//! (aarch64 has no ITS yet, and polling cannot lose a wake-up on hardware we
//! have never booted); the log names the capabilities so a later boot can
//! switch.
//!
//! A command that times out (30 s I/O, 10 s admin) marks the controller dead:
//! every later request fails with EIO instead of risking a queue whose state
//! we no longer know.
//!
//! # Diagnostics
//!
//! Everything a first boot on unknown hardware needs is printed with an
//! `[NVME]` prefix: PCI ids and BAR, CAP/VS/CC/CSTS as found, every bring-up
//! step with its time, Identify (model, serial, firmware, MDTS, NN, ONCS, VWC,
//! APSTA, power states), SMART health, each namespace's size and format, the
//! partition table with type names and the filesystem found in each
//! partition, and the shutdown result.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use sched::ksync::KMutex;

use crate::blkdev::{self, BlkErr, BlockDevice};
use crate::partscan;
use crate::pci::{serial_debug, PciDevice, PciFunction};

// ── Registers ────────────────────────────────────────────────────────────────

const REG_CAP: usize = 0x00;
const REG_VS: usize = 0x08;
const REG_INTMS: usize = 0x0C;
const REG_CC: usize = 0x14;
const REG_CSTS: usize = 0x1C;
const REG_AQA: usize = 0x24;
const REG_ASQ: usize = 0x28;
const REG_ACQ: usize = 0x30;
const DOORBELL_BASE: usize = 0x1000;

const CC_EN: u32 = 1 << 0;
const CC_SHN_MASK: u32 = 3 << 14;
const CC_SHN_NORMAL: u32 = 1 << 14;
const CSTS_RDY: u32 = 1 << 0;
const CSTS_CFS: u32 = 1 << 1;
const CSTS_SHST_MASK: u32 = 3 << 2;
const CSTS_SHST_DONE: u32 = 2 << 2;

// ── Opcodes (the complete list this driver can send) ─────────────────────────

const ADM_DELETE_SQ: u8 = 0x00;
const ADM_CREATE_SQ: u8 = 0x01;
const ADM_GET_LOG: u8 = 0x02;
const ADM_DELETE_CQ: u8 = 0x04;
const ADM_CREATE_CQ: u8 = 0x05;
const ADM_IDENTIFY: u8 = 0x06;
const ADM_SET_FEATURES: u8 = 0x09;
const ADM_GET_FEATURES: u8 = 0x0A;

const IO_FLUSH: u8 = 0x00;
const IO_WRITE: u8 = 0x01;
const IO_READ: u8 = 0x02;

const FEAT_POWER_MGMT: u32 = 0x02;
const FEAT_NUM_QUEUES: u32 = 0x07;
const FEAT_APST: u32 = 0x0C;

// ── Sizes and limits ─────────────────────────────────────────────────────────

const ADMIN_QDEPTH: u16 = 32;
const IO_QDEPTH: u16 = 64;
/// Bounce buffer: 2^6 pages = 256 KiB, physically contiguous.
const DATA_ORDER: usize = 6;
const DATA_BYTES: usize = 4096 << DATA_ORDER;
const ADMIN_TIMEOUT_NS: u64 = 10_000_000_000;
const IO_TIMEOUT_NS: u64 = 30_000_000_000;
const MAX_CTRLS: usize = 8;

// ── Command line ─────────────────────────────────────────────────────────────

static RW: AtomicBool = AtomicBool::new(false);
static APST_KEEP: AtomicBool = AtomicBool::new(false);
static SHUTDOWN_ON: AtomicBool = AtomicBool::new(true);
static DISABLED: AtomicBool = AtomicBool::new(false);

fn parse_cmdline(cmdline: &str) {
    for tok in cmdline.split_ascii_whitespace() {
        match tok {
            "nvme.rw=1" => RW.store(true, Ordering::Relaxed),
            "nvme.rw=0" => RW.store(false, Ordering::Relaxed),
            "nvme.apst=keep" => APST_KEEP.store(true, Ordering::Relaxed),
            "nvme.shutdown=0" => SHUTDOWN_ON.store(false, Ordering::Relaxed),
            "nvme.disable=1" | "nvme=off" => DISABLED.store(true, Ordering::Relaxed),
            _ => {}
        }
    }
}

/// True when `nvme.rw=1` was given (writes allowed).
pub fn writes_allowed() -> bool { RW.load(Ordering::Relaxed) }

// ── Logging ──────────────────────────────────────────────────────────────────

macro_rules! nlog {
    ($($arg:tt)*) => {{
        let mut s = String::new();
        let _ = write!(s, "[NVME] ");
        let _ = write!(s, $($arg)*);
        s.push('\n');
        serial_debug(&s);
    }};
}

fn us_since(t0: u64) -> u64 { sched::monotonic_ns().saturating_sub(t0) / 1000 }

/// Printable text of an Identify string field (space padded, ASCII).
fn id_str(b: &[u8]) -> String {
    let mut s: String = b.iter().map(|&c| if (0x20..0x7F).contains(&c) { c as char } else { '?' }).collect();
    while s.ends_with(' ') { s.pop(); }
    while s.starts_with(' ') { s.remove(0); }
    s
}

fn le16(b: &[u8], o: usize) -> u16 { u16::from_le_bytes([b[o], b[o + 1]]) }
fn le32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) }
fn le64(b: &[u8], o: usize) -> u64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }

/// `12.3 GiB`-style size.
fn human(bytes: u64) -> String {
    let mut s = String::new();
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as u128 * 10;
    let mut i = 0;
    while v >= 10240 && i < U.len() - 1 { v /= 1024; i += 1; }
    let _ = write!(s, "{}.{} {}", v / 10, v % 10, U[i]);
    s
}

// ── Barriers ─────────────────────────────────────────────────────────────────

/// Make queue-memory stores visible to the device before a doorbell write.
#[inline(always)]
fn wmb() {
    #[cfg(target_arch = "aarch64")]
    unsafe { core::arch::asm!("dsb st", options(nostack, preserves_flags)); }
    core::sync::atomic::fence(Ordering::SeqCst);
}

/// Order a completion's phase-bit read before reading the rest of it.
#[inline(always)]
fn rmb() {
    #[cfg(target_arch = "aarch64")]
    unsafe { core::arch::asm!("dsb ld", options(nostack, preserves_flags)); }
    core::sync::atomic::fence(Ordering::SeqCst);
}

// ── MMIO ─────────────────────────────────────────────────────────────────────
//
// Register access through explicit single-register `ldr`/`str` on aarch64:
// a `read_volatile` may compile to a pre/post-indexed (writeback) form, whose
// data abort carries no syndrome (ISV = 0), and QEMU's HVF backend aborts on
// it (`Assertion failed: (isv)`), as in hda.rs.

#[inline(always)]
fn mmio_rd32(a: usize) -> u32 {
    #[cfg(target_arch = "aarch64")]
    unsafe { let v: u32; core::arch::asm!("ldr {v:w}, [{a}]", a = in(reg) a, v = out(reg) v, options(nostack, readonly)); v }
    #[cfg(not(target_arch = "aarch64"))]
    unsafe { core::ptr::read_volatile(a as *const u32) }
}

#[inline(always)]
fn mmio_wr32(a: usize, v: u32) {
    #[cfg(target_arch = "aarch64")]
    unsafe { core::arch::asm!("str {v:w}, [{a}]", a = in(reg) a, v = in(reg) v, options(nostack)); }
    #[cfg(not(target_arch = "aarch64"))]
    unsafe { core::ptr::write_volatile(a as *mut u32, v) }
}

// ── Queues ───────────────────────────────────────────────────────────────────

struct Queue {
    qid: u16,
    depth: u16,
    sq_phys: usize,
    cq_phys: usize,
    sq: *mut u32,
    cq: *const u32,
    sq_tail: u16,
    cq_head: u16,
    phase: bool,
}

impl Queue {
    /// Allocate and zero an SQ (64-byte entries) and CQ (16-byte entries);
    /// both are aligned to the controller page (buddy blocks of order
    /// `min_order` or more are naturally aligned to their size).
    fn new(qid: u16, depth: u16, min_order: usize) -> Option<Queue> {
        let sq_bytes = depth as usize * 64;
        let cq_bytes = depth as usize * 16;
        let order = |b: usize| -> usize { let mut o = min_order; while (4096usize << o) < b { o += 1; } o };
        let sq_phys = mm::buddy::alloc(order(sq_bytes))?;
        let cq_phys = mm::buddy::alloc(order(cq_bytes))?;
        let sq = mm::phys_to_virt(sq_phys) as *mut u32;
        let cq = mm::phys_to_virt(cq_phys) as *mut u32;
        unsafe {
            core::ptr::write_bytes(sq as *mut u8, 0, 4096 << order(sq_bytes));
            core::ptr::write_bytes(cq as *mut u8, 0, 4096 << order(cq_bytes));
        }
        Some(Queue { qid, depth, sq_phys, cq_phys, sq, cq: cq as *const u32, sq_tail: 0, cq_head: 0, phase: true })
    }

    /// Place `cmd` in the next SQ slot, ring the doorbell and poll the CQ
    /// for completion `cid`.
    fn run(&mut self, regs: usize, sq_db: usize, cq_db: usize, cmd: &Cmd, cid: u16, timeout_ns: u64) -> Result<u32, CmdErr> {
        unsafe {
            let slot = self.sq.add(self.sq_tail as usize * 16);
            for (i, &d) in cmd.0.iter().enumerate() { slot.add(i).write_volatile(d); }
        }
        self.sq_tail = (self.sq_tail + 1) % self.depth;
        wmb();
        mmio_wr32(regs + sq_db, self.sq_tail as u32);

        let w = IoWindow::new();
        let t0 = sched::monotonic_ns();
        let mut spins = 0u32;
        loop {
            let e = unsafe { self.cq.add(self.cq_head as usize * 4) };
            let dw3 = unsafe { e.add(3).read_volatile() };
            if ((dw3 >> 16) & 1 != 0) == self.phase {
                rmb();
                let dw0 = unsafe { e.read_volatile() };
                let dw3 = unsafe { e.add(3).read_volatile() };
                self.cq_head += 1;
                if self.cq_head == self.depth { self.cq_head = 0; self.phase = !self.phase; }
                mmio_wr32(regs + cq_db, self.cq_head as u32);
                let got = dw3 as u16;
                if got != cid {
                    // A stale completion (should be impossible with one
                    // command in flight): note it and keep waiting for ours.
                    nlog!("unexpected completion cid {} (waiting for {}) on queue {}", got, cid, self.qid);
                    continue;
                }
                let status = (dw3 >> 17) & 0x7FFF;
                drop(w);
                if status == 0 { return Ok(dw0); }
                return Err(CmdErr::Status {
                    sct: ((status >> 8) & 7) as u8,
                    sc: (status & 0xFF) as u8,
                    dnr: status & (1 << 14) != 0,
                });
            }
            let now = sched::monotonic_ns();
            let waited = now.saturating_sub(t0);
            if waited > timeout_ns {
                drop(w);
                let csts = mmio_rd32(regs + REG_CSTS);
                nlog!("command opcode {:#04x} cid {} on queue {} TIMED OUT after {} ms (CSTS {:#x}); controller marked dead",
                    cmd.0[0] & 0xFF, cid, self.qid, waited / 1_000_000, csts);
                return Err(CmdErr::Timeout);
            }
            if w.sleepable && waited > 200_000 {
                crate::usb_storage::sleep_until(now + (waited / 8).clamp(50_000, 1_000_000));
                continue;
            }
            spins = spins.wrapping_add(1);
            if spins % 256 == 0 {
                if sched::preempt_disabled() && sched::irqs_masked() { sched::irq_window(); }
                // A dead controller reads all-ones; do not wait 30 s for it.
                let csts = mmio_rd32(regs + REG_CSTS);
                if csts == 0xFFFF_FFFF || csts & CSTS_CFS != 0 {
                    drop(w);
                    nlog!("controller fatal status while waiting (CSTS {:#x}); marked dead", csts);
                    return Err(CmdErr::Dead);
                }
            }
            core::hint::spin_loop();
        }
    }
}

/// Why a command failed.
#[derive(Clone, Copy, Debug)]
enum CmdErr {
    /// Completion status: (status code type, status code, do-not-retry).
    Status { sct: u8, sc: u8, dnr: bool },
    Timeout,
    /// The controller is marked dead (earlier timeout or fatal status).
    Dead,
}

impl core::fmt::Display for CmdErr {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            CmdErr::Status { sct, sc, dnr } => write!(f, "status SCT {:#x} SC {:#04x}{}", sct, sc, if *dnr { " DNR" } else { "" }),
            CmdErr::Timeout => write!(f, "TIMEOUT"),
            CmdErr::Dead => write!(f, "controller dead"),
        }
    }
}

/// A 64-byte submission entry.
#[derive(Clone, Copy)]
struct Cmd([u32; 16]);

impl Cmd {
    fn new(opcode: u8, nsid: u32) -> Cmd {
        let mut c = [0u32; 16];
        c[0] = opcode as u32;
        c[1] = nsid;
        Cmd(c)
    }
    fn prp(mut self, prp1: u64, prp2: u64) -> Cmd {
        self.0[6] = prp1 as u32;
        self.0[7] = (prp1 >> 32) as u32;
        self.0[8] = prp2 as u32;
        self.0[9] = (prp2 >> 32) as u32;
        self
    }
    fn dw(mut self, i: usize, v: u32) -> Cmd { self.0[i] = v; self }
}

/// Wait policy for one command (see the module docs). Preemption is off for
/// the command when the caller runs with IRQs masked and may not sleep, so
/// the interrupt windows the poll opens take the tick but never switch tasks.
struct IoWindow { open: bool, sleepable: bool }
impl IoWindow {
    fn new() -> Self {
        let masked = sched::irqs_masked() && sched::current_pid() != 0;
        let sleepable = masked && sched::irqctx::can_sleep();
        let open = masked && !sleepable;
        if open { sched::preempt_disable(); }
        IoWindow { open, sleepable }
    }
}
impl Drop for IoWindow {
    fn drop(&mut self) { if self.open { sched::preempt_enable_no_resched(); } }
}

// ── Controller ───────────────────────────────────────────────────────────────

/// Mutable controller state: only touched with `Ctrl::st` held.
struct CtrlState {
    regs: usize,
    stride: usize,
    admin: Queue,
    io: Option<Queue>,
    cid: u16,
    dead: bool,
    /// One page for Identify / log data.
    scratch_phys: usize,
    /// Bounce buffer and the PRP list describing its pages 1..N.
    data_phys: usize,
    prp_list_phys: usize,
    page: usize,
    /// Set once the controller has been shut down (power-off path).
    shut_down: bool,
}

unsafe impl Send for CtrlState {}

struct Ctrl {
    st: KMutex<CtrlState>,
    index: usize,
    pci: PciFunction,
    /// Largest data transfer per command, bytes.
    max_xfer: usize,
    rtd3e_us: u32,
    timeout_ms: u64,
    vwc: bool,
    read_only: bool,
    warned_ro: AtomicBool,
    // Statistics, printed at shutdown.
    rd_cmds: AtomicU64,
    rd_bytes: AtomicU64,
    wr_cmds: AtomicU64,
    wr_bytes: AtomicU64,
    flushes: AtomicU64,
    max_ns: AtomicU64,
    errors: AtomicU64,
}

impl CtrlState {
    fn r32(&self, off: usize) -> u32 { mmio_rd32(self.regs + off) }
    fn w32(&self, off: usize, v: u32) { mmio_wr32(self.regs + off, v) }
    fn w64(&self, off: usize, v: u64) { self.w32(off, v as u32); self.w32(off + 4, (v >> 32) as u32); }

    fn sq_doorbell(&self, qid: u16) -> usize { DOORBELL_BASE + (2 * qid as usize) * self.stride }
    fn cq_doorbell(&self, qid: u16) -> usize { DOORBELL_BASE + (2 * qid as usize + 1) * self.stride }

    /// Submit `cmd` on the admin queue (`io == false`) or the I/O queue and
    /// poll for its completion. Returns completion DW0. A timeout or a fatal
    /// controller status marks the controller dead.
    fn exec(&mut self, io: bool, mut cmd: Cmd, timeout_ns: u64) -> Result<u32, CmdErr> {
        if self.dead { return Err(CmdErr::Dead); }
        if io && self.io.is_none() { return Err(CmdErr::Dead); }
        self.cid = self.cid.wrapping_add(1);
        let cid = self.cid;
        cmd.0[0] = (cmd.0[0] & 0xFFFF) | (cid as u32) << 16;
        let qid = if io { 1 } else { 0 };
        let (regs, sq_db, cq_db) = (self.regs, self.sq_doorbell(qid), self.cq_doorbell(qid));
        let q = if io { self.io.as_mut().unwrap() } else { &mut self.admin };
        let r = q.run(regs, sq_db, cq_db, &cmd, cid, timeout_ns);
        if matches!(r, Err(CmdErr::Timeout) | Err(CmdErr::Dead)) { self.dead = true; }
        r
    }

    fn admin(&mut self, cmd: Cmd) -> Result<u32, CmdErr> { self.exec(false, cmd, ADMIN_TIMEOUT_NS) }

    /// Identify with CNS `cns` into the scratch page; returns a copy.
    fn identify(&mut self, cns: u32, nsid: u32) -> Result<Vec<u8>, CmdErr> {
        unsafe { core::ptr::write_bytes(mm::phys_to_virt(self.scratch_phys) as *mut u8, 0, 4096); }
        let c = Cmd::new(ADM_IDENTIFY, nsid).prp(self.scratch_phys as u64, 0).dw(10, cns);
        self.admin(c)?;
        Ok(self.scratch_copy(4096))
    }

    fn scratch_copy(&self, n: usize) -> Vec<u8> {
        let mut v = alloc::vec![0u8; n];
        unsafe { core::ptr::copy_nonoverlapping(mm::phys_to_virt(self.scratch_phys) as *const u8, v.as_mut_ptr(), n); }
        v
    }

    /// PRP1/PRP2 for a transfer of `len` bytes starting at the bounce buffer.
    fn prps(&self, len: usize) -> (u64, u64) {
        let pages = len.div_ceil(self.page);
        let p1 = self.data_phys as u64;
        let p2 = match pages {
            0 | 1 => 0,
            2 => (self.data_phys + self.page) as u64,
            _ => self.prp_list_phys as u64,
        };
        (p1, p2)
    }

    /// Wait for CSTS.RDY == `want`, bounded by `timeout_ms`.
    fn wait_ready(&self, want: bool, timeout_ms: u64) -> Result<u64, u32> {
        let t0 = sched::monotonic_ns();
        loop {
            let csts = self.r32(REG_CSTS);
            if csts == 0xFFFF_FFFF { return Err(csts); }
            if (csts & CSTS_RDY != 0) == want { return Ok(us_since(t0)); }
            if want && csts & CSTS_CFS != 0 { return Err(csts); }
            if sched::monotonic_ns().saturating_sub(t0) > timeout_ms * 1_000_000 { return Err(csts); }
            if sched::irqctx::can_sleep() && sched::current_pid() != 0 {
                crate::usb_storage::sleep_until(sched::monotonic_ns() + 1_000_000);
            } else {
                core::hint::spin_loop();
            }
        }
    }
}

/// One namespace as the block registry sees it.
struct Ns {
    ctrl: &'static Ctrl,
    nsid: u32,
    lba_shift: u32,
    sectors: u64,
    name: &'static str,
}

impl Ns {
    fn ss(&self) -> usize { 1usize << self.lba_shift }

    /// Read or write `len` bytes at `lba` through the bounce buffer, in
    /// commands of at most `max_xfer` bytes.
    fn rw(&self, write: bool, lba: u64, ptr: *mut u8, len: usize) -> Result<(), BlkErr> {
        let ss = self.ss();
        if len == 0 || len % ss != 0 { return Err(BlkErr::BadBuffer); }
        let n = (len / ss) as u64;
        if lba.checked_add(n).map_or(true, |e| e > self.sectors) { return Err(BlkErr::OutOfRange); }
        let c = self.ctrl;
        let mut st = c.st.lock();
        if st.shut_down { return Err(BlkErr::Io); }
        let data = mm::phys_to_virt(st.data_phys) as *mut u8;
        let mut done = 0usize;
        while done < len {
            let chunk = (len - done).min(c.max_xfer);
            let slba = lba + (done / ss) as u64;
            let nlb = (chunk / ss) as u32;
            if write {
                unsafe { core::ptr::copy_nonoverlapping(ptr.add(done), data, chunk); }
            }
            let (p1, p2) = st.prps(chunk);
            let cmd = Cmd::new(if write { IO_WRITE } else { IO_READ }, self.nsid)
                .prp(p1, p2)
                .dw(10, slba as u32)
                .dw(11, (slba >> 32) as u32)
                .dw(12, nlb - 1);
            let t0 = sched::monotonic_ns();
            let r = st.exec(true, cmd, IO_TIMEOUT_NS);
            let ns = sched::monotonic_ns().saturating_sub(t0);
            c.max_ns.fetch_max(ns, Ordering::Relaxed);
            match r {
                Ok(_) => {}
                Err(e) => {
                    c.errors.fetch_add(1, Ordering::Relaxed);
                    drop(st);
                    nlog!("{}: {} lba {} x {} failed: {}", self.name, if write { "WRITE" } else { "READ" }, slba, nlb, e);
                    return Err(BlkErr::Io);
                }
            }
            if write {
                c.wr_cmds.fetch_add(1, Ordering::Relaxed);
                c.wr_bytes.fetch_add(chunk as u64, Ordering::Relaxed);
            } else {
                unsafe { core::ptr::copy_nonoverlapping(data, ptr.add(done), chunk); }
                c.rd_cmds.fetch_add(1, Ordering::Relaxed);
                c.rd_bytes.fetch_add(chunk as u64, Ordering::Relaxed);
            }
            done += chunk;
        }
        Ok(())
    }

    fn refuse_write(&self) -> BlkErr {
        if !self.ctrl.warned_ro.swap(true, Ordering::Relaxed) {
            nlog!("{}: write refused: NVMe is read-only (boot with nvme.rw=1 to allow writes)", self.name);
        }
        BlkErr::ReadOnly
    }
}

impl BlockDevice for Ns {
    fn sector_size(&self) -> u32 { self.ss() as u32 }
    fn capacity_sectors(&self) -> u64 { self.sectors }
    fn read_sectors(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlkErr> {
        self.rw(false, lba, buf.as_mut_ptr(), buf.len())
    }
    fn write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), BlkErr> {
        if self.ctrl.read_only { return Err(self.refuse_write()); }
        // `rw` only reads from the pointer for a write.
        self.rw(true, lba, buf.as_ptr() as *mut u8, buf.len())
    }
    /// Panic path: never wait for the controller lock.
    fn try_write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), BlkErr> {
        if self.ctrl.read_only { return Err(BlkErr::ReadOnly); }
        if self.ctrl.st.is_locked() { return Err(BlkErr::Io); }
        self.rw(true, lba, buf.as_ptr() as *mut u8, buf.len())
    }
    fn flush(&self) -> Result<(), BlkErr> {
        // Read-only: nothing was written, and Flush is on the never-send list.
        if self.ctrl.read_only || !self.ctrl.vwc { return Ok(()); }
        let mut st = self.ctrl.st.lock();
        if st.shut_down { return Err(BlkErr::Io); }
        self.ctrl.flushes.fetch_add(1, Ordering::Relaxed);
        match st.exec(true, Cmd::new(IO_FLUSH, self.nsid), IO_TIMEOUT_NS) {
            Ok(_) => Ok(()),
            Err(e) => {
                drop(st);
                nlog!("{}: FLUSH failed: {}", self.name, e);
                Err(BlkErr::Io)
            }
        }
    }
    fn removable(&self) -> bool { false }
    fn name_class(&self) -> &'static str { "nvme" }
    fn read_only(&self) -> bool { self.ctrl.read_only }
    fn disk_name(&self) -> Option<&'static str> { Some(self.name) }
}

static CTRLS: spin::Mutex<Vec<&'static Ctrl>> = spin::Mutex::new(Vec::new());

// ── Probe ────────────────────────────────────────────────────────────────────

fn known_model(vendor: u16, device: u16) -> &'static str {
    match (vendor, device) {
        (0x144d, 0xa808) => " (Samsung SM981/PM981/PM983 / 970 EVO)",
        (0x144d, 0xa804) => " (Samsung SM961/PM961 / 960 EVO)",
        (0x144d, 0xa809) => " (Samsung PM991)",
        (0x144d, 0xa80a) => " (Samsung PM9A1 / 980 PRO)",
        (0x1b36, 0x0010) => " (QEMU NVMe)",
        _ => "",
    }
}

fn bar0(dev: &PciDevice) -> Option<(usize, u64, u64)> {
    let f = dev.function();
    let s = f.size_bars();
    let b = s.bars.iter().find(|b| b.index == 0 && !b.io)?;
    if b.base == 0 || b.size == 0 { return None; }
    let va = mm::kva::ioremap(b.base, b.size.max(0x2000) as usize, mm::kva::CacheMode::Uncached)?;
    Some((va, b.base, b.size))
}

/// Bring up controller `dev`; register its namespaces. Errors are logged.
fn probe(dev: &PciDevice, index: usize) {
    let t_all = sched::monotonic_ns();
    let f = dev.function();
    nlog!("nvme{}: PCI {:02x}:{:02x}.{} {:04x}:{:04x}{} class {:02x}{:02x}{:02x}",
        index, dev.bus, dev.dev, dev.func, dev.vendor_id, dev.device_id,
        known_model(dev.vendor_id, dev.device_id), dev.class, dev.subclass, dev.prog_if);

    // Memory decode + bus master, INTx off (polled driver).
    let cmd0 = f.command();
    f.set_command(cmd0 | crate::pci::CMD_MEMORY | crate::pci::CMD_BUS_MASTER | crate::pci::PCI_CMD_INTX_DISABLE);
    let (regs, bar_phys, bar_size) = match bar0(dev) {
        Some(b) => b,
        None => { nlog!("nvme{}: BAR0 missing or unassigned; skipped", index); return; }
    };
    let msix = f.find_capability(crate::pci::cap::MSIX).map(|c| (f.read16(c + 2) & 0x7FF) + 1);
    let msi = f.find_capability(crate::pci::cap::MSI).is_some();
    nlog!("nvme{}: BAR0 {:#x} size {} KiB; PCI command {:#06x} -> {:#06x}; MSI-X {}, MSI {}; interrupt mode: polled (INTx disabled, all vectors masked)",
        index, bar_phys, bar_size / 1024, cmd0, f.command(),
        match msix { Some(n) => alloc::format!("{} vectors", n), None => String::from("absent") },
        if msi { "present" } else { "absent" });
    if let Some(pm) = f.find_capability(crate::pci::cap::PM) {
        let pmcsr = f.read16(pm + 4);
        if pmcsr & 3 != 0 {
            nlog!("nvme{}: PCI power state D{}; moving to D0", index, pmcsr & 3);
            f.write16(pm + 4, pmcsr & !3);
            crate::usb_storage::sleep_until(sched::monotonic_ns() + 10_000_000);
        }
    }

    let rd32 = |o: usize| mmio_rd32(regs + o);
    let cap = rd32(REG_CAP) as u64 | (rd32(REG_CAP + 4) as u64) << 32;
    if cap == u64::MAX {
        nlog!("nvme{}: CAP reads all-ones (device not responding); skipped", index);
        return;
    }
    let vs = rd32(REG_VS);
    let cc0 = rd32(REG_CC);
    let csts0 = rd32(REG_CSTS);
    let mqes = (cap & 0xFFFF) as u32 + 1;
    let to_ms = ((cap >> 24) & 0xFF).max(1) * 500;
    let dstrd = ((cap >> 32) & 0xF) as u32;
    let css = (cap >> 37) & 0xFF;
    let mpsmin = ((cap >> 48) & 0xF) as u32;
    let mpsmax = ((cap >> 52) & 0xF) as u32;
    nlog!("nvme{}: CAP {:#018x}: MQES {} CQR {} AMS {:#x} TO {} ms DSTRD {} (stride {} B) NSSRS {} CSS {:#x} BPS {} MPSMIN {} KiB MPSMAX {} KiB CMBS {} PMRS {}",
        index, cap, mqes, (cap >> 16) & 1, (cap >> 17) & 3, to_ms, dstrd, 4u32 << dstrd, (cap >> 36) & 1, css,
        (cap >> 45) & 1, 4u32 << mpsmin, 4u32 << mpsmax, (cap >> 57) & 1, (cap >> 56) & 1);
    nlog!("nvme{}: VS {}.{}.{}; as found: CC {:#x} (EN {} SHN {} MPS {} IOSQES {} IOCQES {}) CSTS {:#x} (RDY {} CFS {} SHST {} PP {})",
        index, vs >> 16, (vs >> 8) & 0xFF, vs & 0xFF,
        cc0, cc0 & 1, (cc0 >> 14) & 3, (cc0 >> 7) & 0xF, (cc0 >> 16) & 0xF, (cc0 >> 20) & 0xF,
        csts0, csts0 & 1, (csts0 >> 1) & 1, (csts0 >> 2) & 3, (csts0 >> 5) & 1);
    if css & 1 == 0 {
        nlog!("nvme{}: NVM command set not supported (CSS {:#x}); skipped", index, css);
        return;
    }
    let page = 4096usize << mpsmin;
    if page > 65536 {
        nlog!("nvme{}: minimum page size {} too large; skipped", index, page);
        return;
    }

    // DMA memory: admin queues, scratch page, bounce buffer, PRP list.
    let admin_depth = (ADMIN_QDEPTH as u32).min(mqes) as u16;
    let io_depth = (IO_QDEPTH as u32).min(mqes) as u16;
    let page_order = { let mut o = 0; while (4096usize << o) < page { o += 1; } o };
    let alloc = || -> Option<(Queue, usize, usize, usize)> {
        let admin = Queue::new(0, admin_depth, page_order)?;
        let scratch = mm::buddy::alloc(page_order)?;
        let data = mm::buddy::alloc(DATA_ORDER.max(page_order))?;
        let prp = mm::buddy::alloc(page_order)?;
        Some((admin, scratch, data, prp))
    };
    let (admin, scratch_phys, data_phys, prp_list_phys) = match alloc() {
        Some(a) => a,
        None => { nlog!("nvme{}: out of memory for queues; skipped", index); return; }
    };
    // PRP list: pages 1..N of the bounce buffer (page 0 is always PRP1).
    unsafe {
        let l = mm::phys_to_virt(prp_list_phys) as *mut u64;
        core::ptr::write_bytes(l as *mut u8, 0, page);
        for i in 1..DATA_BYTES / page { l.add(i - 1).write_volatile((data_phys + i * page) as u64); }
    }

    let mut st = CtrlState {
        regs,
        stride: 4usize << dstrd,
        admin,
        io: None,
        cid: 0,
        dead: false,
        scratch_phys,
        data_phys,
        prp_list_phys,
        page,
        shut_down: false,
    };

    // Mask every vector (pin-based / single MSI): we poll.
    st.w32(REG_INTMS, 0xFFFF_FFFF);

    // ── Disable (the firmware may have left it enabled, or mid-transition).
    let t = sched::monotonic_ns();
    let cc = st.r32(REG_CC);
    if cc & CC_EN != 0 && st.r32(REG_CSTS) & CSTS_RDY == 0 {
        match st.wait_ready(true, to_ms) {
            Ok(us) => nlog!("nvme{}: enable left in progress by firmware; RDY=1 after {} us", index, us),
            Err(c) => nlog!("nvme{}: enable left in progress by firmware never completed (CSTS {:#x}); resetting anyway", index, c),
        }
    }
    if cc & CC_EN != 0 || st.r32(REG_CSTS) & CSTS_RDY != 0 {
        st.w32(REG_CC, cc & !CC_EN & !CC_SHN_MASK);
        match st.wait_ready(false, to_ms) {
            Ok(us) => nlog!("nvme{}: controller disabled (CC.EN 1 -> 0) in {} us", index, us),
            Err(c) => {
                nlog!("nvme{}: controller did not clear CSTS.RDY within CAP.TO {} ms (CSTS {:#x}); skipped", index, to_ms, c);
                return;
            }
        }
    } else {
        nlog!("nvme{}: controller already disabled ({} us)", index, us_since(t));
    }

    // ── Admin queue + enable.
    st.w32(REG_AQA, ((admin_depth as u32 - 1) << 16) | (admin_depth as u32 - 1));
    st.w64(REG_ASQ, st.admin.sq_phys as u64);
    st.w64(REG_ACQ, st.admin.cq_phys as u64);
    let cc_new = CC_EN | (mpsmin << 7) | (6 << 16) | (4 << 20); // CSS 0 (NVM), AMS 0 (RR)
    st.w32(REG_CC, cc_new);
    match st.wait_ready(true, to_ms) {
        Ok(us) => nlog!("nvme{}: controller enabled (CC {:#x}) in {} us; admin queue depth {}", index, cc_new, us, admin_depth),
        Err(c) => {
            nlog!("nvme{}: controller did not become ready within {} ms (CSTS {:#x}); skipped", index, to_ms, c);
            return;
        }
    }

    // ── Identify Controller.
    let t = sched::monotonic_ns();
    let idc = match st.identify(1, 0) {
        Ok(b) => b,
        Err(e) => { nlog!("nvme{}: Identify Controller failed: {}; skipped", index, e); disable(&st, to_ms); return; }
    };
    let model = id_str(&idc[24..64]);
    let serial = id_str(&idc[4..24]);
    let fw = id_str(&idc[64..72]);
    let mdts = idc[77];
    let ver = le32(&idc, 80);
    let rtd3r = le32(&idc, 84);
    let rtd3e = le32(&idc, 88);
    let oacs = le16(&idc, 256);
    let frmw = idc[260];
    let lpa = idc[261];
    let npss = idc[263];
    let apsta = idc[265] & 1;
    let wctemp = le16(&idc, 266);
    let cctemp = le16(&idc, 268);
    let sqes = idc[512];
    let cqes = idc[513];
    let nn = le32(&idc, 516);
    let oncs = le16(&idc, 520);
    let vwc = idc[525] & 1 != 0;
    let sgls = le32(&idc, 536);
    nlog!("nvme{}: Identify ({} us): model \"{}\" serial \"{}\" firmware \"{}\" VID {:04x} SSVID {:04x} CNTLID {} ver {}.{}.{}",
        index, us_since(t), model, serial, fw, le16(&idc, 0), le16(&idc, 2), le16(&idc, 78),
        ver >> 16, (ver >> 8) & 0xFF, ver & 0xFF);
    let mdts_bytes = if mdts == 0 { usize::MAX } else { page.saturating_mul(1usize << mdts.min(20)) };
    let max_xfer = DATA_BYTES.min(mdts_bytes);
    nlog!("nvme{}: MDTS {} ({}) -> {} KiB per command; NN {} OACS {:#x} ONCS {:#x} VWC {} FRMW {:#x} LPA {:#x} SQES {:#x} CQES {:#x} SGLS {:#x}",
        index, mdts, if mdts == 0 { String::from("unlimited") } else { human(mdts_bytes as u64) },
        max_xfer / 1024, nn, oacs, oncs, vwc as u8, frmw, lpa, sqes, cqes, sgls);
    let k2c = |k: u16| -> i32 { k as i32 - 273 };
    nlog!("nvme{}: RTD3 resume {} us entry {} us; WCTEMP {} C CCTEMP {} C; APSTA {}; {} power states",
        index, rtd3r, rtd3e, k2c(wctemp), k2c(cctemp), apsta, npss as u32 + 1);
    for ps in 0..=npss.min(31) as usize {
        let d = &idc[2048 + 32 * ps..2048 + 32 * ps + 32];
        let mp = le16(d, 0) as u32;
        let mxps = d[3] & 1 != 0;
        let nops = d[3] & 2 != 0;
        let mw = if mxps { mp / 10 } else { mp * 10 }; // milliwatts
        nlog!("nvme{}:   PS{}: max {}.{:03} W{} entry {} us exit {} us",
            index, ps, mw / 1000, mw % 1000, if nops { " non-operational" } else { "" },
            le32(d, 4), le32(d, 8));
    }
    if (sqes & 0xF) > 6 || (cqes & 0xF) > 4 {
        nlog!("nvme{}: required queue entry sizes SQ 2^{} CQ 2^{} unsupported; skipped", index, sqes & 0xF, cqes & 0xF);
        disable(&st, to_ms);
        return;
    }

    // ── Power: APST off, PS0.
    if apsta != 0 {
        match st.admin(Cmd::new(ADM_GET_FEATURES, 0).dw(10, FEAT_APST).prp(st.scratch_phys as u64, 0)) {
            Ok(dw0) => {
                let on = dw0 & 1 != 0;
                if on && !APST_KEEP.load(Ordering::Relaxed) {
                    // APSTE = 0 with an all-zero transition table, as Linux
                    // sends to turn APST off.
                    unsafe { core::ptr::write_bytes(mm::phys_to_virt(st.scratch_phys) as *mut u8, 0, 4096); }
                    match st.admin(Cmd::new(ADM_SET_FEATURES, 0).dw(10, FEAT_APST).dw(11, 0).prp(st.scratch_phys as u64, 0)) {
                        Ok(_) => nlog!("nvme{}: APST was enabled by firmware; disabled (volatile; nvme.apst=keep to leave it)", index),
                        Err(e) => nlog!("nvme{}: APST was enabled; disabling it failed: {}", index, e),
                    }
                } else {
                    nlog!("nvme{}: APST {}", index, if on { "enabled (kept: nvme.apst=keep)" } else { "disabled" });
                }
            }
            Err(e) => nlog!("nvme{}: Get Features APST failed: {}", index, e),
        }
    }
    match st.admin(Cmd::new(ADM_GET_FEATURES, 0).dw(10, FEAT_POWER_MGMT)) {
        Ok(dw0) => {
            let ps = dw0 & 0x1F;
            if ps != 0 {
                match st.admin(Cmd::new(ADM_SET_FEATURES, 0).dw(10, FEAT_POWER_MGMT).dw(11, 0)) {
                    Ok(_) => nlog!("nvme{}: power state was PS{}; set PS0", index, ps),
                    Err(e) => nlog!("nvme{}: power state PS{}; setting PS0 failed: {}", index, ps, e),
                }
            } else {
                nlog!("nvme{}: power state PS0", index);
            }
        }
        Err(e) => nlog!("nvme{}: Get Features Power Management failed: {}", index, e),
    }

    // ── SMART / health log (read-only).
    let smart = Cmd::new(ADM_GET_LOG, 0xFFFF_FFFF).prp(st.scratch_phys as u64, 0).dw(10, (127 << 16) | 0x02);
    match st.admin(smart) {
        Ok(_) => {
            let l = st.scratch_copy(512);
            let u128_lo = |o: usize| le64(&l, o); // the high halves are 0 on any real drive
            let du = |o: usize| human(u128_lo(o).saturating_mul(512_000));
            nlog!("nvme{}: SMART: critical warning {:#x}, temperature {} C, spare {}% (threshold {}%), used {}%, read {}, written {}, power cycles {}, power-on hours {}, unsafe shutdowns {}, media errors {}, error log entries {}",
                index, l[0], k2c(le16(&l, 1)), l[3], l[4], l[5], du(32), du(48),
                u128_lo(112), u128_lo(128), u128_lo(144), u128_lo(160), u128_lo(176));
        }
        Err(e) => nlog!("nvme{}: SMART log unavailable: {}", index, e),
    }

    // ── I/O queue pair.
    let t = sched::monotonic_ns();
    match st.admin(Cmd::new(ADM_SET_FEATURES, 0).dw(10, FEAT_NUM_QUEUES).dw(11, 0)) {
        Ok(dw0) => nlog!("nvme{}: Number of Queues: {} SQ / {} CQ allocated", index, (dw0 & 0xFFFF) + 1, (dw0 >> 16) + 1),
        Err(e) => nlog!("nvme{}: Set Features Number of Queues failed: {} (continuing)", index, e),
    }
    let ioq = match Queue::new(1, io_depth, page_order) {
        Some(q) => q,
        None => { nlog!("nvme{}: out of memory for the I/O queue; skipped", index); disable(&st, to_ms); return; }
    };
    let qsz = ((io_depth as u32 - 1) << 16) | 1;
    // CQ: physically contiguous, interrupts disabled (IEN = 0).
    if let Err(e) = st.admin(Cmd::new(ADM_CREATE_CQ, 0).prp(ioq.cq_phys as u64, 0).dw(10, qsz).dw(11, 1)) {
        nlog!("nvme{}: Create I/O CQ failed: {}; skipped", index, e);
        disable(&st, to_ms);
        return;
    }
    if let Err(e) = st.admin(Cmd::new(ADM_CREATE_SQ, 0).prp(ioq.sq_phys as u64, 0).dw(10, qsz).dw(11, (1 << 16) | 1)) {
        nlog!("nvme{}: Create I/O SQ failed: {}; skipped", index, e);
        disable(&st, to_ms);
        return;
    }
    st.io = Some(ioq);
    nlog!("nvme{}: I/O queue pair 1 created (depth {}, polled) in {} us", index, io_depth, us_since(t));

    // ── Namespaces.
    let mut nsids: Vec<u32> = Vec::new();
    if vs >= 0x0001_0100 {
        match st.identify(2, 0) {
            Ok(b) => {
                for i in 0..1024 {
                    let id = le32(&b, i * 4);
                    if id == 0 { break; }
                    nsids.push(id);
                }
            }
            Err(e) => nlog!("nvme{}: active namespace list failed: {}; probing 1..=NN", index, e),
        }
    }
    if nsids.is_empty() {
        for id in 1..=nn.min(32) { nsids.push(id); }
    }
    let mut found: Vec<(u32, u32, u64)> = Vec::new();
    for &nsid in &nsids {
        let b = match st.identify(0, nsid) {
            Ok(b) => b,
            Err(e) => { nlog!("nvme{}: Identify Namespace {} failed: {}", index, nsid, e); continue; }
        };
        let nsze = le64(&b, 0);
        if nsze == 0 { continue; }
        let ncap = le64(&b, 8);
        let nuse = le64(&b, 16);
        let nlbaf = b[25] as usize + 1;
        let flbas = b[26];
        let fmt = (flbas & 0xF) as usize;
        let lbaf = le32(&b, 128 + 4 * fmt);
        let ms = lbaf & 0xFFFF;
        let lbads = (lbaf >> 16) & 0xFF;
        let mut formats = String::new();
        for i in 0..nlbaf.min(16) {
            let f = le32(&b, 128 + 4 * i);
            let _ = write!(formats, "{}{}{}B+{}", if i == 0 { "" } else { "," }, if i == fmt { "*" } else { "" },
                1u64 << ((f >> 16) & 0xFF).min(63), f & 0xFFFF);
        }
        let ss = 1u64 << lbads.min(63);
        nlog!("nvme{}n{}: {} sectors x {} B = {} (capacity {}, in use {}); LBA formats [{}]; EUI64 {:02x?}",
            index, nsid, nsze, ss, human(nsze.saturating_mul(ss)), human(ncap.saturating_mul(ss)),
            human(nuse.saturating_mul(ss)), formats, &b[120..128]);
        if ms != 0 {
            nlog!("nvme{}n{}: metadata {} bytes per sector is not supported; namespace skipped", index, nsid, ms);
            continue;
        }
        if !(9..=12).contains(&lbads) {
            nlog!("nvme{}n{}: sector size {} not supported; namespace skipped", index, nsid, ss);
            continue;
        }
        found.push((nsid, lbads, nsze));
    }

    let read_only = !writes_allowed();
    let ctrl: &'static Ctrl = Box::leak(Box::new(Ctrl {
        st: KMutex::new(st),
        index,
        pci: f,
        max_xfer,
        rtd3e_us: rtd3e,
        timeout_ms: to_ms,
        vwc,
        read_only,
        warned_ro: AtomicBool::new(false),
        rd_cmds: AtomicU64::new(0),
        rd_bytes: AtomicU64::new(0),
        wr_cmds: AtomicU64::new(0),
        wr_bytes: AtomicU64::new(0),
        flushes: AtomicU64::new(0),
        max_ns: AtomicU64::new(0),
        errors: AtomicU64::new(0),
    }));
    CTRLS.lock().push(ctrl);
    nlog!("nvme{}: ready in {} ms, {} namespace(s), {}", index, us_since(t_all) / 1000, found.len(),
        if read_only { "READ-ONLY (no write/flush/format commands are ever sent; nvme.rw=1 allows writes)" }
        else { "READ-WRITE (nvme.rw=1)" });

    for (nsid, lbads, nsze) in found {
        let mut name = String::new();
        let _ = write!(name, "nvme{}n{}", index, nsid);
        let ns: &'static Ns = Box::leak(Box::new(Ns {
            ctrl,
            nsid,
            lba_shift: lbads,
            sectors: nsze,
            name: Box::leak(name.into_boxed_str()),
        }));
        let t = sched::monotonic_ns();
        match blkdev::register(ns) {
            Ok(idx) => {
                nlog!("{}: registered as block device {} (partition scan {} us)", ns.name, idx, us_since(t));
                report_partitions(ns);
            }
            Err(e) => nlog!("{}: block registration failed: {:?}", ns.name, e),
        }
    }
}

/// Best-effort CC.EN = 0 after a failed bring-up.
fn disable(st: &CtrlState, to_ms: u64) {
    let cc = st.r32(REG_CC);
    st.w32(REG_CC, cc & !CC_EN);
    let _ = st.wait_ready(false, to_ms);
}

// ── Partition report (diagnostics only; the registry has its own scan) ──────

fn gpt_type_name(g: &[u8; 16]) -> &'static str {
    // On-disk (mixed-endian) byte order.
    const T: [([u8; 16], &str); 12] = [
        ([0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B], "EFI System"),
        ([0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7], "Microsoft basic data"),
        ([0x16, 0xE3, 0xC9, 0xE3, 0x5C, 0x0B, 0xB8, 0x4D, 0x81, 0x7D, 0xF9, 0x2D, 0xF0, 0x02, 0x15, 0xAE], "Microsoft reserved"),
        ([0xA4, 0xBB, 0x94, 0xDE, 0xD1, 0x06, 0x40, 0x4D, 0xA1, 0x6A, 0xBF, 0xD5, 0x01, 0x79, 0xD6, 0xAC], "Windows recovery"),
        (partscan::LINUX_FS_GUID, "Linux filesystem"),
        ([0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F], "Linux swap"),
        ([0x79, 0xD3, 0xD6, 0xE6, 0x07, 0xF5, 0xC2, 0x44, 0xA2, 0x3C, 0x23, 0x8F, 0x2A, 0x3D, 0xF9, 0x28], "Linux LVM"),
        ([0xE3, 0xBC, 0x68, 0x4F, 0xCD, 0xE8, 0xB1, 0x4D, 0x96, 0xE7, 0xFB, 0xCA, 0xF9, 0x84, 0xB7, 0x09], "Linux root (x86-64)"),
        ([0x45, 0xB0, 0x21, 0xB9, 0xF0, 0x1D, 0xC3, 0x41, 0xAF, 0x44, 0x4C, 0x6F, 0x28, 0x0D, 0x3F, 0xAE], "Linux root (aarch64)"),
        ([0xE1, 0xC7, 0x3A, 0x93, 0xB4, 0x2E, 0x13, 0x4F, 0xB8, 0x44, 0x0E, 0x14, 0xE2, 0xAE, 0xF9, 0x15], "Linux home"),
        ([0x48, 0x61, 0x68, 0x21, 0x49, 0x64, 0x6F, 0x6E, 0x74, 0x4E, 0x65, 0x65, 0x64, 0x45, 0x46, 0x49], "BIOS boot"),
        (partscan::LOG_GPT_GUID, "LEANDROS-LOG"),
    ];
    T.iter().find(|(t, _)| t == g).map_or("unknown type", |(_, n)| *n)
}

/// What filesystem signature the first 4 KiB of a partition carries.
fn fs_signature(b: &[u8]) -> &'static str {
    if crate::blockdev::is_f2fs_block0(b) { return "f2fs"; }
    if &b[3..11] == b"NTFS    " { return "ntfs"; }
    if &b[3..11] == b"EXFAT   " { return "exfat"; }
    if b.len() >= 1082 && le16(b, 1080) == 0xEF53 { return "ext2/3/4"; }
    if &b[82..90] == b"FAT32   " || &b[54..59] == b"FAT16" || &b[54..59] == b"FAT12" { return "vfat"; }
    if &b[0..4] == b"XFSB" { return "xfs"; }
    if &b[4086..4096] == b"SWAPSPACE2" { return "swap"; }
    if &b[0..6] == b"-FVE-F" || &b[3..11] == b"-FVE-FS-" { return "bitlocker"; }
    if b.iter().all(|&x| x == 0) { return "empty (zeros)"; }
    "unknown"
}

fn report_partitions(ns: &'static Ns) {
    let ss = ns.ss() as u32;
    let res = partscan::scan(ss, ns.sectors, |lba, buf| ns.read_sectors(lba, buf).is_ok());
    match res.table {
        partscan::Table::None => { nlog!("{}: no partition table", ns.name); return; }
        partscan::Table::Mbr { disk_sig } => nlog!("{}: MBR, disk signature {:08x}, {} partition(s)", ns.name, disk_sig, res.parts.len()),
        partscan::Table::Gpt { disk_guid, backup } => {
            let mut t = [0u8; 36];
            let n = partscan::PartUuid::Gpt(disk_guid).format(&mut t);
            nlog!("{}: GPT{}, disk GUID {}, {} partition(s)", ns.name, if backup { " (BACKUP header)" } else { "" },
                core::str::from_utf8(&t[..n]).unwrap_or("?"), res.parts.len());
        }
    }
    let mut buf = alloc::vec![0u8; 4096];
    for p in &res.parts {
        let ty = match p.ptype {
            partscan::PartType::Gpt(g) => String::from(gpt_type_name(&g)),
            partscan::PartType::Mbr(t) => alloc::format!("MBR type {:#04x}", t),
        };
        let mut u = [0u8; 36];
        let un = p.uuid.format(&mut u);
        let fs = if ns.read_sectors(p.start_lba, &mut buf).is_ok() { fs_signature(&buf) } else { "unreadable" };
        nlog!("{}p{}: start {} size {} ({}), {}, PARTUUID {}, filesystem {}",
            ns.name, p.pno, p.start_lba, human(p.num_lbas * ss as u64), p.num_lbas, ty,
            core::str::from_utf8(&u[..un]).unwrap_or("?"), fs);
    }
}

// ── Public entry points ──────────────────────────────────────────────────────

/// Probe every NVMe controller and register its namespaces. Task context,
/// after `blkdev::init()` (the registry) — runs in the init task, before
/// `klog::init`, so these lines reach the stick's log partition.
pub fn init(cmdline: &str) -> usize {
    parse_cmdline(cmdline);
    let devs = crate::pci::find_by_class(0x01, 0x08, 0x02);
    if devs.is_empty() { return 0; }
    if DISABLED.load(Ordering::Relaxed) {
        nlog!("{} controller(s) present; driver disabled by nvme.disable=1", devs.len());
        return 0;
    }
    nlog!("{} controller(s); mode {}", devs.len(),
        if writes_allowed() { "read-write (nvme.rw=1)" } else { "read-only (default)" });
    for (i, d) in devs.iter().enumerate().take(MAX_CTRLS) {
        probe(d, i);
    }
    CTRLS.lock().len()
}

/// Orderly shutdown of every controller (power-off and restart): delete the
/// I/O queues, CC.SHN = normal, wait for CSTS.SHST = complete (bounded by
/// RTD3E, at least 5 s and at most 20 s, like Linux's `shutdown_timeout`).
/// The drive's own write cache is committed by the shutdown itself.
/// `nvme.shutdown=0` skips it.
pub fn shutdown_all() {
    let ctrls: Vec<&'static Ctrl> = match CTRLS.try_lock() { Some(c) => c.clone(), None => return };
    for c in ctrls {
        let mib = |b: u64| b / (1024 * 1024);
        nlog!("nvme{}: I/O since boot: {} reads ({} MiB), {} writes ({} MiB), {} flushes, {} errors, slowest command {} us",
            c.index, c.rd_cmds.load(Ordering::Relaxed), mib(c.rd_bytes.load(Ordering::Relaxed)),
            c.wr_cmds.load(Ordering::Relaxed), mib(c.wr_bytes.load(Ordering::Relaxed)),
            c.flushes.load(Ordering::Relaxed), c.errors.load(Ordering::Relaxed),
            c.max_ns.load(Ordering::Relaxed) / 1000);
        if !SHUTDOWN_ON.load(Ordering::Relaxed) { continue; }
        // Up to 2 s for an in-flight command to finish.
        let deadline = sched::monotonic_ns() + 2_000_000_000;
        let mut g = loop {
            if let Some(g) = c.st.try_lock() { break Some(g); }
            if sched::monotonic_ns() > deadline { break None; }
            core::hint::spin_loop();
        };
        let st = match g.as_mut() {
            Some(s) => s,
            None => { nlog!("nvme{}: busy; shutting down without waiting for the queue", c.index); continue; }
        };
        if st.shut_down { continue; }
        let t = sched::monotonic_ns();
        if st.io.is_some() && !st.dead {
            let _ = st.admin(Cmd::new(ADM_DELETE_SQ, 0).dw(10, 1));
            let _ = st.admin(Cmd::new(ADM_DELETE_CQ, 0).dw(10, 1));
        }
        let cc = st.r32(REG_CC);
        st.w32(REG_CC, (cc & !CC_SHN_MASK) | CC_SHN_NORMAL);
        let limit_ms = (c.rtd3e_us as u64 / 1000).clamp(5_000, 20_000);
        let mut csts;
        loop {
            csts = st.r32(REG_CSTS);
            if csts & CSTS_SHST_MASK == CSTS_SHST_DONE || csts == 0xFFFF_FFFF { break; }
            if sched::monotonic_ns().saturating_sub(t) > limit_ms * 1_000_000 { break; }
            core::hint::spin_loop();
        }
        st.shut_down = true;
        if csts & CSTS_SHST_MASK == CSTS_SHST_DONE {
            nlog!("nvme{}: shutdown complete in {} us (CC.SHN normal, CSTS {:#x})", c.index, us_since(t), csts);
        } else {
            nlog!("nvme{}: shutdown NOT confirmed after {} ms (CSTS {:#x})", c.index, us_since(t) / 1000, csts);
        }
        let _ = (c.pci, c.timeout_ms);
    }
}
