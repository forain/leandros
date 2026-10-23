//! Runtime interrupt registration for drivers: `request_irq` / `free_irq`,
//! an x86_64 IDT vector allocator, and MSI / MSI-X programming for any PCI
//! function.
//!
//! # Model
//!
//! * x86_64: device interrupts are IDT vectors. Vectors `0x21..=0xEF` (minus
//!   0x40, the reschedule IPI) all enter one generic stub
//!   (`arch_x86_64::idt::irq_dyn_dispatch`), which sends the LAPIC EOI, enters
//!   hard-IRQ context and calls [`dispatch_vector`]. Drivers get a vector from
//!   [`alloc_vector`] (`0x41` upwards) and normally never see it: they call
//!   [`request_msi`] or [`MsixTable::request`]. MSI messages target the BSP
//!   (APIC id 0), fixed delivery, edge — the same message virtio-gpu always
//!   used. Legacy INTx is not routed on x86_64 (no ACPI _PRT parser):
//!   `IrqSource::Line` returns `Unsupported` there.
//! * aarch64: device interrupts are GIC INTIDs ("lines"). `IrqSource::Line`
//!   registers a handler in this module's line table (up to 4 sharers per
//!   line, for shared PCI INTx) and enables the SPI level-triggered; the GIC
//!   dispatcher falls back to [`dispatch_line`] for INTIDs it does not own.
//!   MSI needs a GICv3 ITS, which this kernel does not drive yet:
//!   `request_msi`/MSI-X return `Unsupported` on aarch64 (TODO: ITS).
//!
//! # Handler contract
//!
//! Handlers are `extern "C" fn(ctx) -> IrqReturn` and run in hard-IRQ context
//! (`sched::irqctx::in_irq()` is true), IRQs masked, on whatever task was
//! interrupted. They must not sleep; they may use `KSpinLock`, the `ksync`
//! wake/up/complete calls, `workqueue::queue`, and the atomic allocation
//! pool. EOI is central (x86_64: before the handler, the vector being edge;
//! aarch64: after it), so a level-triggered INTx handler must quiesce its
//! device before returning. A line on which no sharer returns `Handled` for
//! 1000 deliveries in a row is masked and logged (storm guard).

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use crate::pci::PciFunction;

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IrqReturn {
    /// Not this device's interrupt (shared lines).
    None = 0,
    Handled = 1,
}

/// Interrupt handler: `ctx` is the value given at registration.
pub type IrqHandler = extern "C" fn(ctx: usize) -> IrqReturn;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IrqError {
    /// No free vector / line sharer slot / table entry.
    NoSpace,
    /// The source does not exist (capability missing, bad entry index).
    NotFound,
    /// Not available on this architecture (MSI on aarch64, INTx on x86_64).
    Unsupported,
    /// Mapping the MSI-X table failed.
    NoMemory,
    /// Already registered.
    Busy,
}

/// What to attach a handler to (see [`request_irq`]).
#[derive(Clone, Copy, Debug)]
pub enum IrqSource {
    /// An IDT vector previously obtained from [`alloc_vector`] (x86_64).
    Vector(u8),
    /// A GIC INTID (aarch64), e.g. from `PciFunction::intx_line`.
    Line(u32),
}

/// A registered interrupt; give it back to [`free_irq`].
#[derive(Debug)]
pub struct IrqHandle {
    kind: HandleKind,
}

#[derive(Debug)]
enum HandleKind {
    Vector { vector: u8 },
    Line { id: u32, slot: usize },
    Msi { vector: u8, func: PciFunction, cap: u16 },
    MsiX { vector: u8, table: usize, entry: u16 },
}

impl IrqHandle {
    /// The IDT vector (x86_64 kinds), if any.
    pub fn vector(&self) -> Option<u8> {
        match self.kind {
            HandleKind::Vector { vector } | HandleKind::Msi { vector, .. } | HandleKind::MsiX { vector, .. } => Some(vector),
            HandleKind::Line { .. } => None,
        }
    }
    /// The GIC INTID (aarch64 line), if any.
    pub fn line(&self) -> Option<u32> {
        match self.kind { HandleKind::Line { id, .. } => Some(id), _ => None }
    }
}

// ── vector table (x86_64) ────────────────────────────────────────────────────

const VEC_FREE: u8 = 0;
/// Allocated by `alloc_vector`, no handler yet / handler installed.
const VEC_ALLOC: u8 = 1;
/// Fixed (legacy) vector registered by the arch.
const VEC_FIXED: u8 = 2;
/// System vector (timer, IPIs, exceptions): never handed out.
const VEC_SYSTEM: u8 = 3;

/// First vector `alloc_vector` hands out (0x41: the vector virtio-gpu has
/// always used is the first allocation, as it is the first MSI user).
pub const ALLOC_FIRST: u8 = 0x41;
/// Last allocatable vector (0xF0..0xFF stay for IPIs/spurious).
pub const ALLOC_LAST: u8 = 0xEF;

struct VecSlot {
    state: AtomicU8,
    handler: AtomicUsize,
    ctx: AtomicUsize,
    count: AtomicU64,
}

static VECTORS: [VecSlot; 256] = [const { VecSlot {
    state: AtomicU8::new(VEC_FREE), handler: AtomicUsize::new(0), ctx: AtomicUsize::new(0), count: AtomicU64::new(0),
} }; 256];
static SPURIOUS_VEC: AtomicU64 = AtomicU64::new(0);

/// Reserve an unused device vector (x86_64). Any context. None when all
/// `ALLOC_FIRST..=ALLOC_LAST` are taken.
pub fn alloc_vector() -> Option<u8> {
    for v in ALLOC_FIRST..=ALLOC_LAST {
        if v == 0x40 { continue; }
        if VECTORS[v as usize].state.compare_exchange(VEC_FREE, VEC_ALLOC, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            return Some(v);
        }
    }
    None
}

/// Return a vector from [`alloc_vector`] (its handler must be gone).
pub fn free_vector(v: u8) {
    let s = &VECTORS[v as usize];
    s.handler.store(0, Ordering::Release);
    let _ = s.state.compare_exchange(VEC_ALLOC, VEC_FREE, Ordering::AcqRel, Ordering::Relaxed);
}

/// Register a handler on a fixed, legacy vector (arch init: the PS/2
/// keyboard's IRQ1 = vector 33). Not for MSI.
pub fn request_fixed_vector(v: u8, h: IrqHandler, ctx: usize) -> bool {
    let s = &VECTORS[v as usize];
    if s.state.compare_exchange(VEC_FREE, VEC_FIXED, Ordering::AcqRel, Ordering::Relaxed).is_err() { return false; }
    s.ctx.store(ctx, Ordering::Release);
    s.handler.store(h as usize, Ordering::Release);
    true
}

/// Mark a vector as owned by the system (never allocated).
pub fn reserve_system_vector(v: u8) {
    VECTORS[v as usize].state.store(VEC_SYSTEM, Ordering::Release);
}

fn set_vector_handler(v: u8, h: IrqHandler, ctx: usize) -> Result<(), IrqError> {
    let s = &VECTORS[v as usize];
    if s.state.load(Ordering::Acquire) != VEC_ALLOC { return Err(IrqError::NotFound); }
    if s.handler.load(Ordering::Acquire) != 0 { return Err(IrqError::Busy); }
    s.ctx.store(ctx, Ordering::Release);
    s.handler.store(h as usize, Ordering::Release);
    Ok(())
}

/// Called by the arch for every dynamic vector, in hard-IRQ context, after
/// EOI. Unknown vectors are counted (and reported once).
pub fn dispatch_vector(v: u8) {
    let s = &VECTORS[v as usize];
    let h = s.handler.load(Ordering::Acquire);
    if h == 0 {
        if SPURIOUS_VEC.fetch_add(1, Ordering::Relaxed) == 0 {
            crate::pci::serial_debug("[IRQ] interrupt on unregistered vector ");
            crate::pci::serial_debug_hex(v as u32);
            crate::pci::serial_debug(" (ignored; reported once)\n");
        }
        return;
    }
    s.count.fetch_add(1, Ordering::Relaxed);
    let f: IrqHandler = unsafe { core::mem::transmute::<usize, IrqHandler>(h) };
    let _ = f(s.ctx.load(Ordering::Acquire));
}

/// Interrupts delivered to vector `v` since boot.
pub fn vector_count(v: u8) -> u64 { VECTORS[v as usize].count.load(Ordering::Relaxed) }

// ── line table (aarch64 GIC) ─────────────────────────────────────────────────

const NUM_LINES: usize = 512;
const SHARERS: usize = 4;
const STORM_LIMIT: u32 = 1000;

struct LineSlot {
    handler: [AtomicUsize; SHARERS],
    ctx: [AtomicUsize; SHARERS],
    unhandled_run: AtomicU32,
    count: AtomicU64,
}

static LINES: [LineSlot; NUM_LINES] = [const { LineSlot {
    handler: [const { AtomicUsize::new(0) }; SHARERS],
    ctx: [const { AtomicUsize::new(0) }; SHARERS],
    unhandled_run: AtomicU32::new(0),
    count: AtomicU64::new(0),
} }; NUM_LINES];

/// Called by the aarch64 GIC dispatcher for INTIDs it has no built-in handler
/// for. Returns false if nobody is registered (the caller then masks it).
pub fn dispatch_line(id: u32) -> bool {
    if id as usize >= NUM_LINES { return false; }
    let l = &LINES[id as usize];
    let mut any = false;
    let mut handled = false;
    for i in 0..SHARERS {
        let h = l.handler[i].load(Ordering::Acquire);
        if h == 0 { continue; }
        any = true;
        let f: IrqHandler = unsafe { core::mem::transmute::<usize, IrqHandler>(h) };
        if f(l.ctx[i].load(Ordering::Acquire)) == IrqReturn::Handled { handled = true; }
    }
    if !any { return false; }
    l.count.fetch_add(1, Ordering::Relaxed);
    if handled {
        l.unhandled_run.store(0, Ordering::Relaxed);
    } else if l.unhandled_run.fetch_add(1, Ordering::Relaxed) + 1 >= STORM_LIMIT {
        l.unhandled_run.store(0, Ordering::Relaxed);
        line_disable(id);
        crate::pci::serial_debug("[IRQ] line ");
        crate::pci::serial_debug_hex(id);
        crate::pci::serial_debug(" fired 1000 times with no handler claiming it; masked\n");
    }
    true
}

/// Deliveries on line `id` since boot.
pub fn line_count(id: u32) -> u64 {
    if id as usize >= NUM_LINES { 0 } else { LINES[id as usize].count.load(Ordering::Relaxed) }
}

#[cfg(target_arch = "aarch64")]
extern "C" {
    fn arch_irq_line_enable(id: u32, edge: bool) -> bool;
    fn arch_irq_line_disable(id: u32);
}

fn line_enable(id: u32) -> bool {
    #[cfg(target_arch = "aarch64")]
    { unsafe { arch_irq_line_enable(id, false) } }
    #[cfg(not(target_arch = "aarch64"))]
    { let _ = id; false }
}

fn line_disable(id: u32) {
    #[cfg(target_arch = "aarch64")]
    unsafe { arch_irq_line_disable(id) }
    #[cfg(not(target_arch = "aarch64"))]
    let _ = id;
}

// ── request / free ───────────────────────────────────────────────────────────

/// Attach `handler(ctx)` to `src`. Task context.
pub fn request_irq(src: IrqSource, handler: IrqHandler, ctx: usize) -> Result<IrqHandle, IrqError> {
    match src {
        IrqSource::Vector(v) => {
            if !cfg!(target_arch = "x86_64") { return Err(IrqError::Unsupported); }
            set_vector_handler(v, handler, ctx)?;
            Ok(IrqHandle { kind: HandleKind::Vector { vector: v } })
        }
        IrqSource::Line(id) => {
            if !cfg!(target_arch = "aarch64") || id < 32 || id as usize >= NUM_LINES {
                return Err(IrqError::Unsupported);
            }
            let l = &LINES[id as usize];
            let mut slot = SHARERS;
            for i in 0..SHARERS {
                if l.handler[i].load(Ordering::Acquire) == 0 {
                    l.ctx[i].store(ctx, Ordering::Release);
                    if l.handler[i].compare_exchange(0, handler as usize, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                        slot = i;
                        break;
                    }
                }
            }
            if slot == SHARERS { return Err(IrqError::NoSpace); }
            if !line_enable(id) {
                l.handler[slot].store(0, Ordering::Release);
                return Err(IrqError::Unsupported);
            }
            Ok(IrqHandle { kind: HandleKind::Line { id, slot } })
        }
    }
}

/// Detach a handler and release whatever `request_*` set up (MSI disabled,
/// MSI-X entry masked, vector freed, line masked when its last sharer goes).
/// Task context. After return the handler is no longer called on this CPU;
/// a delivery already running on another CPU may still be finishing.
pub fn free_irq(h: IrqHandle) {
    match h.kind {
        HandleKind::Vector { vector } => free_vector(vector),
        HandleKind::Line { id, slot } => {
            let l = &LINES[id as usize];
            l.handler[slot].store(0, Ordering::Release);
            if (0..SHARERS).all(|i| l.handler[i].load(Ordering::Acquire) == 0) { line_disable(id); }
        }
        HandleKind::Msi { vector, func, cap } => {
            let ctrl = func.read16(cap + 2);
            func.write16(cap + 2, ctrl & !1);
            free_vector(vector);
        }
        HandleKind::MsiX { vector, table, entry } => {
            unsafe { ((table + entry as usize * 16 + 12) as *mut u32).write_volatile(1); }
            free_vector(vector);
        }
    }
}

// ── MSI ──────────────────────────────────────────────────────────────────────

#[cfg(target_arch = "x86_64")]
extern "C" {
    /// The MSI address/data pair that delivers `vector` (arch-x86_64).
    fn arch_msi_message(vector: u32, addr: *mut u64, data: *mut u32) -> bool;
}

/// (address, data) of the message for `vector`, or None (aarch64: no ITS).
pub fn msi_message(vector: u8) -> Option<(u64, u32)> {
    #[cfg(target_arch = "x86_64")]
    {
        let (mut a, mut d) = (0u64, 0u32);
        if unsafe { arch_msi_message(vector as u32, &mut a, &mut d) } { Some((a, d)) } else { None }
    }
    #[cfg(not(target_arch = "x86_64"))]
    { let _ = vector; None }
}

/// Enable single-vector MSI on `func` with `handler(ctx)`. Allocates a vector,
/// installs the handler, programs address/data (64-bit capable or not),
/// masks per-vector if the function supports it while programming, disables
/// INTx and MSI-X, enables MSI, sets bus mastering. Task context.
pub fn request_msi(func: PciFunction, handler: IrqHandler, ctx: usize) -> Result<IrqHandle, IrqError> {
    if !cfg!(target_arch = "x86_64") { return Err(IrqError::Unsupported); }
    let cap = func.find_capability(crate::pci::cap::MSI).ok_or(IrqError::NotFound)?;
    let vector = alloc_vector().ok_or(IrqError::NoSpace)?;
    let (addr, data) = match msi_message(vector) { Some(m) => m, None => { free_vector(vector); return Err(IrqError::Unsupported); } };
    if let Err(e) = set_vector_handler(vector, handler, ctx) { free_vector(vector); return Err(e); }
    let ctrl = func.read16(cap + 2);
    let is64 = ctrl & (1 << 7) != 0;
    let maskable = ctrl & (1 << 8) != 0;
    // MSI off while programming, MSI-X off (they are exclusive), 1 vector.
    func.write16(cap + 2, ctrl & !1 & !(7 << 4));
    if let Some(mx) = func.find_capability(crate::pci::cap::MSIX) {
        let c = func.read16(mx + 2);
        if c & 0x8000 != 0 { func.write16(mx + 2, c & !0x8000); }
    }
    func.write32(cap + 4, addr as u32);
    let (data_off, mask_off) = if is64 {
        func.write32(cap + 8, (addr >> 32) as u32);
        (cap + 12, cap + 16)
    } else {
        (cap + 8, cap + 12)
    };
    func.write16(data_off, data as u16);
    if maskable { func.write32(mask_off, 0); }
    func.set_intx_disable(true);
    func.set_bus_master(true);
    func.write16(cap + 2, (ctrl & !(7 << 4)) | 1);
    Ok(IrqHandle { kind: HandleKind::Msi { vector, func, cap } })
}

/// Mask / unmask the MSI vector of a handle from `request_msi` (only if the
/// function has per-vector masking; returns false otherwise). Any context.
pub fn msi_set_masked(h: &IrqHandle, masked: bool) -> bool {
    if let HandleKind::Msi { func, cap, .. } = h.kind {
        let ctrl = func.read16(cap + 2);
        if ctrl & (1 << 8) == 0 { return false; }
        let mask_off = if ctrl & (1 << 7) != 0 { cap + 16 } else { cap + 12 };
        func.write32(mask_off, if masked { 1 } else { 0 });
        return true;
    }
    false
}

// ── MSI-X ────────────────────────────────────────────────────────────────────

/// An MSI-X table mapped and enabled on one function. Entries start masked;
/// [`MsixTable::request`] programs one and unmasks it.
pub struct MsixTable {
    func: PciFunction,
    cap: u16,
    /// VA of the vector table (16 bytes per entry) and the PBA.
    table: usize,
    pba: usize,
    entries: u16,
}

impl MsixTable {
    /// Map the table + PBA (uncached), mask every entry, enable MSI-X with
    /// the function mask set, then clear the function mask; disable MSI and
    /// INTx; set bus mastering. Task context.
    pub fn enable(func: PciFunction) -> Result<MsixTable, IrqError> {
        if !cfg!(target_arch = "x86_64") { return Err(IrqError::Unsupported); }
        let cap = func.find_capability(crate::pci::cap::MSIX).ok_or(IrqError::NotFound)?;
        let ctrl = func.read16(cap + 2);
        let entries = (ctrl & 0x7FF) + 1;
        let tbl = func.read32(cap + 4);
        let pba = func.read32(cap + 8);
        let map = |bir_off: u32, len: usize| -> Option<usize> {
            let bar = func.bar((bir_off & 7) as usize)?;
            if bar.io || bar.base == 0 { return None; }
            mm::kva::ioremap(bar.base + (bir_off & !7) as u64, len, mm::kva::CacheMode::Uncached)
        };
        let table = map(tbl, entries as usize * 16).ok_or(IrqError::NoMemory)?;
        let pba_va = match map(pba, ((entries as usize + 63) / 64) * 8) {
            Some(v) => v,
            None => { mm::kva::iounmap(table); return Err(IrqError::NoMemory); }
        };
        if let Some(m) = func.find_capability(crate::pci::cap::MSI) {
            let c = func.read16(m + 2);
            if c & 1 != 0 { func.write16(m + 2, c & !1); }
        }
        // Enable + function mask, mask every entry, then lift the function mask.
        func.write16(cap + 2, ctrl | 0xC000);
        for i in 0..entries as usize {
            unsafe { ((table + i * 16 + 12) as *mut u32).write_volatile(1); }
        }
        func.set_intx_disable(true);
        func.set_bus_master(true);
        func.write16(cap + 2, (ctrl | 0x8000) & !0x4000);
        Ok(MsixTable { func, cap, table, pba: pba_va, entries })
    }

    pub fn entries(&self) -> u16 { self.entries }
    pub fn function(&self) -> PciFunction { self.func }

    /// Program entry `entry` with a fresh vector, install `handler(ctx)`, and
    /// unmask it. Task context.
    pub fn request(&self, entry: u16, handler: IrqHandler, ctx: usize) -> Result<IrqHandle, IrqError> {
        if entry >= self.entries { return Err(IrqError::NotFound); }
        let vector = alloc_vector().ok_or(IrqError::NoSpace)?;
        let (addr, data) = match msi_message(vector) { Some(m) => m, None => { free_vector(vector); return Err(IrqError::Unsupported); } };
        if let Err(e) = set_vector_handler(vector, handler, ctx) { free_vector(vector); return Err(e); }
        let e = self.table + entry as usize * 16;
        unsafe {
            ((e + 12) as *mut u32).write_volatile(1);
            (e as *mut u32).write_volatile(addr as u32);
            ((e + 4) as *mut u32).write_volatile((addr >> 32) as u32);
            ((e + 8) as *mut u32).write_volatile(data);
            ((e + 12) as *mut u32).write_volatile(0);
        }
        Ok(IrqHandle { kind: HandleKind::MsiX { vector, table: self.table, entry } })
    }

    /// Per-vector mask (any context).
    pub fn set_masked(&self, entry: u16, masked: bool) {
        if entry >= self.entries { return; }
        unsafe { ((self.table + entry as usize * 16 + 12) as *mut u32).write_volatile(masked as u32); }
    }

    /// Pending bit of `entry` in the PBA (set while masked and the device
    /// wanted to signal).
    pub fn pending(&self, entry: u16) -> bool {
        if entry >= self.entries { return false; }
        let w = unsafe { ((self.pba + (entry as usize / 64) * 8) as *const u64).read_volatile() };
        w & (1u64 << (entry % 64)) != 0
    }

    /// Function-wide mask (any context).
    pub fn set_function_masked(&self, masked: bool) {
        let c = self.func.read16(self.cap + 2);
        self.func.write16(self.cap + 2, if masked { c | 0x4000 } else { c & !0x4000 });
    }

    /// Disable MSI-X and unmap the table. Free every `request`ed handle first.
    /// Task context.
    pub fn disable(self) {
        let c = self.func.read16(self.cap + 2);
        self.func.write16(self.cap + 2, c & !0xC000);
        mm::kva::iounmap(self.table);
        mm::kva::iounmap(self.pba);
    }
}
