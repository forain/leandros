//! `DmaBuf` — the one type every xHCI DMA target is allocated as.
//!
//! Rings, the DCBAA, scratchpad pages, device/input contexts, the ERST and
//! every transfer bounce buffer are a `DmaBuf`. Nothing else is ever handed
//! to the controller: no stack address, no heap (`alloc`) address, no
//! caller-supplied slice. The guarantees:
//!
//! * **Physically contiguous and naturally aligned.** A buddy block of
//!   2^order pages is aligned to its own size, so a buffer of ≤ 64 KiB never
//!   crosses a 64 KiB boundary (the xHCI rule for TRB data buffers), and a
//!   one-page buffer satisfies every context/ring alignment (≤ 4 KiB).
//! * **Addressable by the controller.** With `dma32` the block is guaranteed
//!   to end below 4 GiB (for AC64=0 controllers), via `buddy::alloc_below`.
//! * **Zeroed** at allocation.
//! * **Freed on Drop.** The owner is responsible for making sure the
//!   controller can no longer reach the buffer before dropping it (the xHCI
//!   driver halts the controller, or disables the slot, first).
//!
//! The CPU accesses the buffer through its HHDM alias with volatile
//! accesses; [`dma_wmb`]/[`dma_rmb`] order those against MMIO doorbells and
//! controller-written completion records.

use mm::buddy;

pub struct DmaBuf {
    phys: usize,
    virt: usize,
    order: usize,
    len: usize,
}

// The buffer is plain memory owned by this value; the raw address is an
// integer, so moving it between CPUs is fine.
unsafe impl Send for DmaBuf {}
unsafe impl Sync for DmaBuf {}

const FOUR_GIB: usize = 1 << 32;

impl DmaBuf {
    /// Allocate a zeroed, physically contiguous buffer of at least `len`
    /// bytes (rounded up to a power-of-two number of pages).
    #[track_caller]
    pub fn new(len: usize, dma32: bool) -> Option<Self> {
        let pages = len.max(1).div_ceil(buddy::PAGE_SIZE).next_power_of_two();
        let order = pages.trailing_zeros() as usize;
        let size = buddy::PAGE_SIZE << order;
        let dma32 = dma32 || crate::platform::force_dma32();

        let phys = if dma32 {
            buddy::alloc_below(order, FOUR_GIB)?
        } else {
            buddy::alloc(order)?
        };
        let virt = mm::phys_to_virt(phys);
        unsafe { core::ptr::write_bytes(virt as *mut u8, 0, size); }
        dma_wmb();
        Some(DmaBuf { phys, virt, order, len: size })
    }

    #[inline] pub fn phys(&self) -> u64 { self.phys as u64 }
    #[inline] pub fn len(&self) -> usize { self.len }
    #[inline] pub fn as_ptr(&self) -> *mut u8 { self.virt as *mut u8 }

    #[inline]
    pub fn read32(&self, off: usize) -> u32 {
        assert!(off + 4 <= self.len);
        unsafe { ((self.virt + off) as *const u32).read_volatile() }
    }
    #[inline]
    pub fn write32(&self, off: usize, v: u32) {
        assert!(off + 4 <= self.len);
        unsafe { ((self.virt + off) as *mut u32).write_volatile(v) }
    }
    #[inline]
    pub fn write64(&self, off: usize, v: u64) {
        self.write32(off, v as u32);
        self.write32(off + 4, (v >> 32) as u32);
    }
    #[inline]
    pub fn read64(&self, off: usize) -> u64 {
        self.read32(off) as u64 | ((self.read32(off + 4) as u64) << 32)
    }

    pub fn zero(&self, off: usize, n: usize) {
        assert!(off + n <= self.len);
        unsafe { core::ptr::write_bytes((self.virt + off) as *mut u8, 0, n); }
    }

    /// Copy `src` into the buffer at `off`.
    pub fn copy_in(&self, off: usize, src: &[u8]) {
        assert!(off + src.len() <= self.len);
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), (self.virt + off) as *mut u8, src.len());
        }
    }

    /// Copy `dst.len()` bytes out of the buffer at `off`.
    pub fn copy_out(&self, off: usize, dst: &mut [u8]) {
        assert!(off + dst.len() <= self.len);
        unsafe {
            core::ptr::copy_nonoverlapping((self.virt + off) as *const u8, dst.as_mut_ptr(), dst.len());
        }
    }

    /// Make CPU writes visible to the device. x86 PCIe is always
    /// DMA-coherent and so is QEMU `virt`, so this is an ordering barrier
    /// only. A non-coherent aarch64 platform (to be verified per board — see
    /// artifacts/notes/lane-xhci-2026-10-05.md) would clean the range to the
    /// point of coherency here, and invalidate it in `sync_for_cpu`.
    #[inline] pub fn sync_for_device(&self) { dma_wmb(); }
    /// Make device writes visible to the CPU (see `sync_for_device`).
    #[inline] pub fn sync_for_cpu(&self) { dma_rmb(); }
}

impl Drop for DmaBuf {
    fn drop(&mut self) {
        buddy::free(self.phys, self.order);
    }
}

/// Order normal-memory writes before a following MMIO write (doorbell) or a
/// following cycle-bit flip the controller polls.
#[inline]
pub fn dma_wmb() {
    #[cfg(target_arch = "aarch64")]
    unsafe { core::arch::asm!("dsb st", options(nostack, preserves_flags)); }
    #[cfg(not(target_arch = "aarch64"))]
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}

/// Order a read of a controller-written ownership bit before reads of the
/// rest of the record it guards.
#[inline]
pub fn dma_rmb() {
    #[cfg(target_arch = "aarch64")]
    unsafe { core::arch::asm!("dsb ld", options(nostack, preserves_flags)); }
    #[cfg(not(target_arch = "aarch64"))]
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}
