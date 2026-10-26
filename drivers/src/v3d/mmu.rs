//! V3D 7.1 MMU: one flat table of 2^20 32-bit PTEs (4 MiB, physically contiguous) that maps the
//! GPU's 32-bit virtual space in 4 KiB pages, plus the MMU cache / TLB flush that must follow
//! every PTE change. Linux reference: `drivers/gpu/drm/v3d/v3d_mmu.c`.
//!
//! The pure part (PTE encoding, table fill/clear, the enable and flush register sequences) is
//! written against [`V3dRegs`] and a [`Clock`] so it runs on the host
//! (`drivers/v3d-tests`). The hardware glue (`hw`, feature `rpi5`) owns the real table, the
//! scratch page and the hooks CREATE_BO / BO free call.
//!
//! COHERENCY. V3D is not cache-coherent with the CPUs (no `dma-coherent` in the DT). The table is
//! written only through a Normal-NC alias; its cacheable HHDM alias is cleaned and invalidated
//! once, after zeroing, so no dirty line can later be evicted over a PTE. BO pages get the same
//! treatment when they are mapped, and BO user mmaps are already write-combine
//! (`blob_map_cache_type`).

use super::regs::V3dRegs;

// ── Hub registers ───────────────────────────────────────────────────────────
pub const MMUC_CONTROL: u32 = 0x1000;
pub const MMUC_CONTROL_ENABLE: u32 = 1 << 0;
pub const MMUC_CONTROL_FLUSH: u32 = 1 << 1;
pub const MMUC_CONTROL_FLUSHING: u32 = 1 << 2;

pub const MMU_CTL: u32 = 0x1200;
pub const MMU_CTL_ENABLE: u32 = 1 << 0;
pub const MMU_CTL_TLB_CLEAR: u32 = 1 << 2;
pub const MMU_CTL_TLB_CLEARING: u32 = 1 << 7;
pub const MMU_CTL_WRITE_VIOLATION_INT: u32 = 1 << 10;
pub const MMU_CTL_WRITE_VIOLATION_ABORT: u32 = 1 << 11;
pub const MMU_CTL_WRITE_VIOLATION: u32 = 1 << 12;
pub const MMU_CTL_PT_INVALID_ENABLE: u32 = 1 << 16;
pub const MMU_CTL_PT_INVALID_INT: u32 = 1 << 18;
pub const MMU_CTL_PT_INVALID_ABORT: u32 = 1 << 19;
pub const MMU_CTL_PT_INVALID: u32 = 1 << 20;
pub const MMU_CTL_CAP_EXCEEDED_INT: u32 = 1 << 25;
pub const MMU_CTL_CAP_EXCEEDED_ABORT: u32 = 1 << 26;
pub const MMU_CTL_CAP_EXCEEDED: u32 = 1 << 27;
/// What `v3d_mmu_set_page_table` writes.
pub const MMU_CTL_ON: u32 = MMU_CTL_ENABLE
    | MMU_CTL_PT_INVALID_ENABLE
    | MMU_CTL_PT_INVALID_ABORT
    | MMU_CTL_PT_INVALID_INT
    | MMU_CTL_WRITE_VIOLATION_ABORT
    | MMU_CTL_WRITE_VIOLATION_INT
    | MMU_CTL_CAP_EXCEEDED_ABORT
    | MMU_CTL_CAP_EXCEEDED_INT;

pub const MMU_PT_PA_BASE: u32 = 0x1204;
pub const MMU_VIO_ID: u32 = 0x122c;
pub const MMU_ILLEGAL_ADDR: u32 = 0x1230;
pub const MMU_ILLEGAL_ADDR_ENABLE: u32 = 1 << 31;
pub const MMU_VIO_ADDR: u32 = 0x1234;
pub const MMU_DEBUG_INFO: u32 = 0x1238;

// ── Page table ──────────────────────────────────────────────────────────────
pub const PAGE_SHIFT: u32 = 12;
pub const PAGE_SIZE: u64 = 1 << PAGE_SHIFT;
pub const BIG_PAGE_SIZE: u64 = 64 << 10;
pub const SUPER_PAGE_SIZE: u64 = 1 << 20;
pub const PT_ENTRIES: usize = 1 << 20;
pub const PT_BYTES: usize = PT_ENTRIES * 4;
/// Buddy order of the table: 2^10 pages of 4 KiB = 4 MiB.
pub const PT_ORDER: usize = 10;

pub const PTE_SUPERPAGE: u32 = 1 << 31;
pub const PTE_BIGPAGE: u32 = 1 << 30;
pub const PTE_WRITEABLE: u32 = 1 << 29;
pub const PTE_VALID: u32 = 1 << 28;
/// 24-bit page frame number: physical addresses below 64 GiB.
pub const PTE_PFN_MASK: u32 = 0x00FF_FFFF;

/// Every flush poll is bounded (design note: 100 ms).
pub const FLUSH_TIMEOUT_US: u64 = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    /// 4 KiB.
    Small,
    /// 64 KiB: 16 consecutive PTEs, VA and PA 64 KiB aligned.
    Big,
    /// 1 MiB: 256 consecutive PTEs, VA and PA 1 MiB aligned.
    Super,
}

impl PageKind {
    pub const fn bytes(self) -> u64 {
        match self {
            PageKind::Small => PAGE_SIZE,
            PageKind::Big => BIG_PAGE_SIZE,
            PageKind::Super => SUPER_PAGE_SIZE,
        }
    }
    const fn flag(self) -> u32 {
        match self {
            PageKind::Small => 0,
            PageKind::Big => PTE_BIGPAGE,
            PageKind::Super => PTE_SUPERPAGE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushStage {
    /// MMUC_CONTROL.FLUSHING never cleared.
    Mmuc,
    /// MMU_CTL.TLB_CLEARING never cleared.
    Tlb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmuError {
    /// Address not 4 KiB aligned.
    Misaligned,
    /// Physical address beyond the 24-bit PFN (or the MMU's PA width).
    PaTooHigh,
    /// VA range runs off the end of the table.
    OutOfRange,
    FlushTimeout(FlushStage),
    NoMemory,
    /// The MMU was never brought up (probe failed or not run).
    NotReady,
}

impl MmuError {
    /// Short greppable tag for the `[v3d]` boot line.
    pub fn tag(self) -> &'static str {
        match self {
            MmuError::Misaligned => "misaligned",
            MmuError::PaTooHigh => "pa-too-high",
            MmuError::OutOfRange => "out-of-range",
            MmuError::FlushTimeout(FlushStage::Mmuc) => "flush-mmuc-timeout",
            MmuError::FlushTimeout(FlushStage::Tlb) => "flush-tlb-timeout",
            MmuError::NoMemory => "nomem",
            MmuError::NotReady => "not-ready",
        }
    }
}

/// Monotonic microseconds, for bounded polls. The host tests use a fake one.
pub trait Clock {
    fn now_us(&self) -> u64;
}

/// One PTE for the first 4 KiB of a `kind` page at `pa`. The other 15/255 PTEs of a big/super
/// page are this value plus their index (the PFN is the low field).
pub fn pte(pa: u64, kind: PageKind) -> Result<u32, MmuError> {
    if pa & (PAGE_SIZE - 1) != 0 {
        return Err(MmuError::Misaligned);
    }
    let pfn = pa >> PAGE_SHIFT;
    if pfn > PTE_PFN_MASK as u64 {
        return Err(MmuError::PaTooHigh);
    }
    Ok(pfn as u32 | PTE_VALID | PTE_WRITEABLE | kind.flag())
}

/// Largest page kind usable at (`va`, `pa`) with `remaining` bytes left: both addresses aligned
/// to it and the whole page inside the range. Super pages only when `allow_super`.
pub fn choose_kind(va: u64, pa: u64, remaining: u64, allow_super: bool) -> PageKind {
    let fits = |k: PageKind| {
        let b = k.bytes();
        va & (b - 1) == 0 && pa & (b - 1) == 0 && remaining >= b
    };
    if allow_super && fits(PageKind::Super) {
        PageKind::Super
    } else if fits(PageKind::Big) {
        PageKind::Big
    } else {
        PageKind::Small
    }
}

fn range_index(pt_len: usize, va: u64, len: u64) -> Result<(usize, usize), MmuError> {
    if va & (PAGE_SIZE - 1) != 0 {
        return Err(MmuError::Misaligned);
    }
    let pages = len.checked_add(PAGE_SIZE - 1).ok_or(MmuError::OutOfRange)? >> PAGE_SHIFT;
    let first = va >> PAGE_SHIFT;
    let end = first.checked_add(pages).ok_or(MmuError::OutOfRange)?;
    if end > pt_len as u64 {
        return Err(MmuError::OutOfRange);
    }
    Ok((first as usize, pages as usize))
}

/// Write PTEs mapping GPU `va..va+len` to physical `pa..` (contiguous). Validates everything
/// before writing anything. Returns the number of PTEs written.
pub fn fill_ptes(pt: &mut [u32], va: u64, pa: u64, len: u64, allow_super: bool) -> Result<usize, MmuError> {
    let (first, pages) = range_index(pt.len(), va, len)?;
    if pa & (PAGE_SIZE - 1) != 0 {
        return Err(MmuError::Misaligned);
    }
    if pages == 0 {
        return Ok(0);
    }
    pte(pa + ((pages as u64 - 1) << PAGE_SHIFT), PageKind::Small)?;
    let mut i = 0usize;
    while i < pages {
        let off = (i as u64) << PAGE_SHIFT;
        let remaining = ((pages - i) as u64) << PAGE_SHIFT;
        let kind = choose_kind(va + off, pa + off, remaining, allow_super);
        let base = pte(pa + off, kind)?;
        let n = (kind.bytes() >> PAGE_SHIFT) as usize;
        for j in 0..n {
            pt[first + i + j] = base + j as u32;
        }
        i += n;
    }
    Ok(pages)
}

/// Invalidate the PTEs of `va..va+len`. Returns the number cleared.
pub fn clear_ptes(pt: &mut [u32], va: u64, len: u64) -> Result<usize, MmuError> {
    let (first, pages) = range_index(pt.len(), va, len)?;
    for e in &mut pt[first..first + pages] {
        *e = 0;
    }
    Ok(pages)
}

fn poll_clear(read: &dyn Fn() -> u32, mask: u32, clock: &dyn Clock, timeout_us: u64) -> bool {
    let start = clock.now_us();
    loop {
        if read() & mask == 0 {
            return true;
        }
        if clock.now_us().wrapping_sub(start) >= timeout_us {
            return read() & mask == 0;
        }
        core::hint::spin_loop();
    }
}

/// `v3d_mmu_flush_all`: flush the MMU cache, then clear the TLB, each poll bounded.
pub fn flush_all(regs: &dyn V3dRegs, clock: &dyn Clock) -> Result<(), MmuError> {
    barrier();
    regs.hub_w(MMUC_CONTROL, MMUC_CONTROL_FLUSH | MMUC_CONTROL_ENABLE);
    if !poll_clear(&|| regs.hub_r(MMUC_CONTROL), MMUC_CONTROL_FLUSHING, clock, FLUSH_TIMEOUT_US) {
        return Err(MmuError::FlushTimeout(FlushStage::Mmuc));
    }
    regs.hub_w(MMU_CTL, regs.hub_r(MMU_CTL) | MMU_CTL_TLB_CLEAR);
    if !poll_clear(&|| regs.hub_r(MMU_CTL), MMU_CTL_TLB_CLEARING, clock, FLUSH_TIMEOUT_US) {
        return Err(MmuError::FlushTimeout(FlushStage::Tlb));
    }
    Ok(())
}

/// `v3d_mmu_set_page_table`: point the MMU at the table, arm the fault reporting, send illegal
/// accesses to the scratch page, enable the MMU cache, flush.
pub fn enable(regs: &dyn V3dRegs, pt_pa: u64, scratch_pa: u64, clock: &dyn Clock) -> Result<(), MmuError> {
    if pt_pa & (PAGE_SIZE - 1) != 0 || scratch_pa & (PAGE_SIZE - 1) != 0 {
        return Err(MmuError::Misaligned);
    }
    if (pt_pa >> PAGE_SHIFT) > u32::MAX as u64 || (scratch_pa >> PAGE_SHIFT) > (MMU_ILLEGAL_ADDR_ENABLE - 1) as u64 {
        return Err(MmuError::PaTooHigh);
    }
    regs.hub_w(MMU_PT_PA_BASE, (pt_pa >> PAGE_SHIFT) as u32);
    regs.hub_w(MMU_CTL, MMU_CTL_ON);
    regs.hub_w(MMU_ILLEGAL_ADDR, (scratch_pa >> PAGE_SHIFT) as u32 | MMU_ILLEGAL_ADDR_ENABLE);
    regs.hub_w(MMUC_CONTROL, MMUC_CONTROL_ENABLE);
    flush_all(regs, clock)
}

/// PA width the MMU reports (`MMU_DEBUG_INFO[11:8]` + 30).
pub fn pa_width(regs: &dyn V3dRegs) -> u32 {
    30 + ((regs.hub_r(MMU_DEBUG_INFO) >> 8) & 0xF)
}

/// A page table plus the policy that fills it. Every change is followed by a flush.
pub struct Mmu<'a> {
    pub pt: &'a mut [u32],
    pub allow_super: bool,
    /// Exclusive physical limit (1 << pa_width).
    pub pa_limit: u64,
}

impl<'a> Mmu<'a> {
    pub fn map(&mut self, regs: &dyn V3dRegs, clock: &dyn Clock, va: u64, pa: u64, len: u64) -> Result<usize, MmuError> {
        if pa.checked_add(len).map_or(true, |end| end > self.pa_limit) {
            return Err(MmuError::PaTooHigh);
        }
        let n = fill_ptes(self.pt, va, pa, len, self.allow_super)?;
        if let Err(e) = flush_all(regs, clock) {
            // The caller frees the BO on error, so its PTEs must not survive.
            let _ = clear_ptes(self.pt, va, len);
            let _ = flush_all(regs, clock);
            return Err(e);
        }
        Ok(n)
    }

    pub fn unmap(&mut self, regs: &dyn V3dRegs, clock: &dyn Clock, va: u64, len: u64) -> Result<usize, MmuError> {
        let n = clear_ptes(self.pt, va, len)?;
        flush_all(regs, clock)?;
        Ok(n)
    }
}

/// Orders the CPU's PTE stores (Normal-NC) before the MMIO write that starts a flush.
#[inline]
pub fn barrier() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub use hw::*;

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
mod hw {
    use super::*;
    use core::fmt::Write;
    use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use spin::Mutex;

    /// CNTVCT-based clock.
    pub struct CntClock;
    impl Clock for CntClock {
        fn now_us(&self) -> u64 {
            let (v, f): (u64, u64);
            unsafe {
                core::arch::asm!("isb; mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack));
                core::arch::asm!("mrs {}, cntfrq_el0", out(reg) f, options(nomem, nostack));
            }
            if f == 0 { return v; }
            ((v as u128 * 1_000_000) / f as u128) as u64
        }
    }

    /// `dc civac` over a kernel VA range (64-byte lines), then `dsb sy`.
    pub fn dcache_clean_inval(va: usize, len: usize) {
        let mut p = va & !63;
        let end = va + len;
        while p < end {
            unsafe { core::arch::asm!("dc civac, {}", in(reg) p, options(nostack, preserves_flags)); }
            p += 64;
        }
        unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)); }
    }

    /// Fixed-size line buffer for the one-line `[v3d]` reports.
    pub struct LineBuf { buf: [u8; 256], len: usize }
    impl LineBuf {
        pub const fn new() -> Self { LineBuf { buf: [0; 256], len: 0 } }
        pub fn emit(&self) {
            crate::pci::serial_debug(core::str::from_utf8(&self.buf[..self.len]).unwrap_or("[v3d] ?\n"));
        }
    }
    impl Write for LineBuf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let n = s.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
            self.len += n;
            Ok(())
        }
    }

    struct HwMmu {
        regs: &'static (dyn V3dRegs + Sync),
        pt_va: usize,
        pa_limit: u64,
    }

    static HW: Mutex<Option<HwMmu>> = Mutex::new(None);
    /// Page table and scratch page, kept for the post-reset re-enable (phase C), lock-free because
    /// a reset runs from the queue's interrupt/tick context.
    static PT_PA: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    static SCRATCH_PA: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

    /// After a GPU reset: program the same page table again and flush. False if never enabled or
    /// the flush timed out.
    pub fn reenable(regs: &dyn V3dRegs) -> bool {
        let pt = PT_PA.load(Ordering::Acquire);
        if pt == 0 { return false; }
        enable(regs, pt, SCRATCH_PA.load(Ordering::Acquire), &CntClock).is_ok()
    }
    static ACTIVE: AtomicBool = AtomicBool::new(false);
    /// Map/unmap calls whose flush failed (the GPU may hold a stale TLB entry).
    pub static FLUSH_FAILURES: AtomicU32 = AtomicU32::new(0);

    /// True once the MMU is enabled on probed hardware. CREATE_BO / free consult it.
    #[inline]
    pub fn active() -> bool { ACTIVE.load(Ordering::Acquire) }

    fn with_mmu<R>(f: impl FnOnce(&mut Mmu, &dyn V3dRegs) -> R) -> Option<R> {
        let g = HW.lock();
        let h = g.as_ref()?;
        let pt = unsafe { core::slice::from_raw_parts_mut(h.pt_va as *mut u32, PT_ENTRIES) };
        let mut m = Mmu { pt, allow_super: false, pa_limit: h.pa_limit };
        Some(f(&mut m, h.regs))
    }

    /// Map a freshly zeroed, physically contiguous BO at GPU `va`. Its zeroing went through the
    /// cacheable HHDM alias, so that alias is cleaned and invalidated first: the GPU reads
    /// memory, and no dirty line may be evicted later over GPU or write-combined user writes.
    pub fn map_bo(va: u64, pa: u64, len: u64) -> Result<(), MmuError> {
        if !active() { return Err(MmuError::NotReady); }
        dcache_clean_inval(mm::phys_to_virt(pa as usize), len as usize);
        let r = with_mmu(|m, regs| m.map(regs, &CntClock, va, pa, len)).ok_or(MmuError::NotReady)?;
        if let Err(MmuError::FlushTimeout(_)) = r { FLUSH_FAILURES.fetch_add(1, Ordering::Relaxed); }
        r.map(|_| ())
    }

    /// Remove a BO's PTEs. Must run before its VA is reused or its pages are freed.
    pub fn unmap_bo(va: u64, len: u64) {
        if !active() { return; }
        let r = with_mmu(|m, regs| m.unmap(regs, &CntClock, va, len));
        if let Some(Err(e)) = r {
            FLUSH_FAILURES.fetch_add(1, Ordering::Relaxed);
            let mut l = LineBuf::new();
            let _ = write!(l, "[v3d] mmu unmap va={:#x} len={:#x} fail {}\n", va, len, e.tag());
            l.emit();
        }
    }

    fn zeroed_block(order: usize) -> Result<usize, MmuError> {
        let pa = mm::buddy::alloc(order).ok_or(MmuError::NoMemory)?;
        let bytes = 4096usize << order;
        let va = mm::phys_to_virt(pa);
        unsafe { core::ptr::write_bytes(va as *mut u8, 0, bytes); }
        dcache_clean_inval(va, bytes);
        Ok(pa)
    }

    fn init_mmu(regs: &'static (dyn V3dRegs + Sync)) -> Result<u64, MmuError> {
        let pa_w = pa_width(regs).min(36);
        let pa_limit = 1u64 << pa_w;
        let pt_pa = zeroed_block(PT_ORDER)?;
        let scratch_pa = match zeroed_block(0) {
            Ok(p) => p,
            Err(e) => { mm::buddy::free(pt_pa, PT_ORDER); return Err(e); }
        };
        if (pt_pa + PT_BYTES) as u64 > pa_limit || scratch_pa as u64 + PAGE_SIZE > pa_limit {
            mm::buddy::free(pt_pa, PT_ORDER);
            mm::buddy::free(scratch_pa, 0);
            return Err(MmuError::PaTooHigh);
        }
        let pt_va = match mm::kva::ioremap(pt_pa as u64, PT_BYTES, mm::kva::CacheMode::WriteCombining) {
            Some(v) => v,
            None => {
                mm::buddy::free(pt_pa, PT_ORDER);
                mm::buddy::free(scratch_pa, 0);
                return Err(MmuError::NoMemory);
            }
        };
        // From here the hardware may hold the table's address: on failure the 4 MiB and the
        // scratch page are deliberately leaked rather than handed back to the allocator.
        enable(regs, pt_pa as u64, scratch_pa as u64, &CntClock)?;
        PT_PA.store(pt_pa as u64, Ordering::Release);
        SCRATCH_PA.store(scratch_pa as u64, Ordering::Release);
        *HW.lock() = Some(HwMmu { regs, pt_va, pa_limit });
        ACTIVE.store(true, Ordering::Release);
        Ok(pt_pa as u64)
    }

    /// Phase B bring-up. Call ONLY after the power/ident probe succeeded; on a failed probe the
    /// caller skips this and the MMU and IRQ lines are never touched. Prints one line:
    /// `[v3d] mmu pt=0x... flush ok; irq hub=282 ok core=281 ok mode=irq`.
    /// Returns true when the MMU is up (the IRQ result is reported, not required).
    pub fn after_probe(regs: &'static (dyn V3dRegs + Sync), ver: u32, va_width: u32) -> bool {
        let mut l = LineBuf::new();
        let ok = match init_mmu(regs) {
            Ok(pt) => { let _ = write!(l, "[v3d] mmu pt={:#x} flush ok; ", pt); true }
            Err(e) => { let _ = write!(l, "[v3d] mmu fail {}; ", e.tag()); false }
        };
        super::super::irq::bring_up(regs, ver, va_width, &mut l);
        let _ = l.write_str("\n");
        l.emit();
        ok
    }
}
