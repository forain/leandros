//! Block-driver support (kmod W2-3): the block-class registration wrappers and `env`, the `kenv`
//! of the storage modules (virtio_blk, nvme): the kmod-kapi pci/mm/sched plus the handful of
//! helpers the built-in kernel build reaches directly.

use kmod_abi::blk::sys_blk;
use kmod_abi::{KBlkDesc, KBlkOps};

pub fn register(desc: &KBlkDesc, ops: &'static KBlkOps, ctx: usize) -> Result<i64, i64> {
    match unsafe { sys_blk::kapi_blk_register(crate::owner(), desc, ops, ctx) } {
        i if i >= 0 => Ok(i),
        e => Err(e),
    }
}
pub fn remove(idx: i64) { unsafe { sys_blk::kapi_blk_remove(idx); } }

impl crate::pci::PciFunction {
    pub fn set_command(&self, v: u16) { self.write16(0x04, v); }
    pub fn command(&self) -> u16 { self.read16(0x04) }
    /// Module-side twin of the kernel's `size_bars` (BARs only; no local-IRQ mask, so call it
    /// from init before anything else touches the device).
    pub fn size_bars(&self) -> BarSizing {
        let mut bars = alloc::vec::Vec::new();
        let mut i = 0;
        while i < 6 {
            if let Some(mut b) = self.bar(i) {
                if !b.io { b.size = self.bar_size(i); }
                if b.base != 0 || b.size != 0 { bars.push(b); }
                i += if b.is64 { 2 } else { 1 };
            } else { i += 1; }
        }
        BarSizing { bars, rom_orig: 0, rom_size: 0 }
    }
}

pub struct BarSizing {
    pub bars: alloc::vec::Vec<crate::pci::BarInfo>,
    pub rom_orig: u32,
    pub rom_size: u64,
}

pub mod env {
    pub use crate::mm;

    pub fn serial_raw(s: &str) { crate::pci::serial_debug(s) }
    pub fn sleep_until(ns: u64) { crate::sys_common::sleep_until(ns) }

    fn word(key: &str) -> Option<&'static str> {
        crate::cmdline().split_ascii_whitespace().find_map(|t| t.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
    }
    pub fn cmdline_known() -> bool { true }
    pub fn cmdline_word(key: &str) -> Option<&'static str> { word(key) }
    pub fn cmdline_opt(key: &str) -> Option<u64> { word(key).and_then(|w| w.parse().ok()) }

    pub mod pci {
        pub use crate::pci::*;
        use kmod_abi::KPciDev;
        use alloc::vec::Vec;

        pub const CMD_MEMORY: u16 = 1 << 1;
        pub const CMD_BUS_MASTER: u16 = 1 << 2;

        fn conv(k: &KPciDev) -> PciDevice {
            PciDevice {
                bus: (k.bdf >> 16) as u8, dev: (k.bdf >> 8) as u8, func: k.bdf as u8,
                vendor_id: k.vendor, device_id: k.device,
                class: (k.class >> 16) as u8, subclass: (k.class >> 8) as u8, prog_if: k.class as u8, bars: k.bars,
            }
        }
        /// Every function of `vendor:device`, in PCI scan order.
        pub fn find_all_devices(vendor: u16, device: u16) -> Vec<PciDevice> {
            let mut out = Vec::new();
            let mut i = 0u32;
            loop {
                let mut k = KPciDev::default();
                if unsafe { crate::sys::kapi_pci_find(vendor, device, i, &mut k) } != 0 { break; }
                out.push(conv(&k));
                i += 1;
            }
            out
        }
        pub fn find_by_class(class: u8, sub: u8, prog_if: u8) -> Vec<PciDevice> {
            let want = (class as u32) << 16 | (sub as u32) << 8 | prog_if as u32;
            let mut out = Vec::new();
            let mut i = 0u32;
            while let Some(k) = crate::sys_common::pci_scan(i) {
                if k.class == want { out.push(conv(&k)); }
                i += 1;
            }
            out
        }
        pub fn serial_debug_dec(v: u64) { serial_debug(&alloc::format!("{}", v)); }
    }

    pub mod sched {
        pub use crate::sched::*;
        pub fn irqs_masked() -> bool { crate::sys_common::irqs_masked() }
        pub fn current_pid() -> u32 { crate::sys_common::current_pid() }
        pub fn preempt_disable() { crate::sys_common::preempt_disable() }
        pub fn preempt_disabled() -> bool { unsafe { kmod_abi::blk::sys_blk::kapi_preempt_disabled() } }
        pub fn preempt_enable_no_resched() { unsafe { kmod_abi::blk::sys_blk::kapi_preempt_enable_no_resched() } }
        /// Open one interrupt window so the tick still runs (the kernel's `sched::irq_window`
        /// minus the timer-liveness check).
        #[inline(always)]
        pub fn irq_window() {
            unsafe {
                #[cfg(target_arch = "x86_64")]
                core::arch::asm!("sti; pause; cli");
                #[cfg(target_arch = "aarch64")]
                core::arch::asm!("msr daifclr, #2; nop; msr daifset, #2");
            }
        }
        pub mod irqctx {
            pub fn can_sleep() -> bool { crate::sys_common::can_sleep() }
        }
        pub mod ksync {
            use core::ops::{Deref, DerefMut};
            use core::cell::UnsafeCell;
            /// Sleeping mutex over `kapi_mutex_*`.
            pub struct KMutex<T> { h: usize, v: UnsafeCell<T> }
            unsafe impl<T: Send> Send for KMutex<T> {}
            unsafe impl<T: Send> Sync for KMutex<T> {}
            pub struct KMutexGuard<'a, T> { m: &'a KMutex<T> }
            impl<T> KMutex<T> {
                pub fn new(v: T) -> Self { KMutex { h: crate::sys_common::mutex_new("blk"), v: UnsafeCell::new(v) } }
                pub fn try_lock(&self) -> Option<KMutexGuard<'_, T>> {
                    if crate::sys_common::mutex_trylock(self.h) { Some(KMutexGuard { m: self }) } else { None }
                }
                pub fn is_locked(&self) -> bool {
                    match self.try_lock() { Some(_) => false, None => true }
                }
                pub fn lock(&self) -> KMutexGuard<'_, T> { crate::sys_common::mutex_lock(self.h); KMutexGuard { m: self } }
            }
            impl<T> Deref for KMutexGuard<'_, T> { type Target = T; fn deref(&self) -> &T { unsafe { &*self.m.v.get() } } }
            impl<T> DerefMut for KMutexGuard<'_, T> { fn deref_mut(&mut self) -> &mut T { unsafe { &mut *self.m.v.get() } } }
            impl<T> Drop for KMutexGuard<'_, T> { fn drop(&mut self) { crate::sys_common::mutex_unlock(self.m.h); } }
        }
    }
}
