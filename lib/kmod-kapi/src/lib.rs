//! Module-side kernel API shim (see Cargo.toml). Only `kapi_*` functions are imported.
#![no_std]

use core::sync::atomic::{AtomicU16, Ordering};
use kmod_abi::{sys, KPciDev, KmodId, KMAP_MMIO, KMAP_WC, KMAP_WRITABLE};

static OWNER: AtomicU16 = AtomicU16::new(0);

/// Called once by the module runtime from `kmod_init` with `ctx.owner`.
pub fn set_owner(id: KmodId) { OWNER.store(id.0, Ordering::SeqCst); }
pub fn owner() -> KmodId { KmodId(OWNER.load(Ordering::SeqCst)) }

pub mod pci {
    use super::*;

    pub const PCI_CMD_INTX_DISABLE: u16 = 1 << 10;

    #[derive(Clone, Copy, Debug)]
    pub struct PciDevice {
        pub bus: u8,
        pub dev: u8,
        pub func: u8,
        pub vendor_id: u16,
        pub device_id: u16,
        pub class: u8,
        pub subclass: u8,
        pub prog_if: u8,
        pub bars: [u32; 6],
    }

    fn bdf(bus: u8, dev: u8, func: u8) -> u32 { (bus as u32) << 16 | (dev as u32) << 8 | func as u32 }

    pub fn find_device(vendor_id: u16, device_id: u16) -> Option<PciDevice> {
        let mut k = KPciDev::default();
        if unsafe { sys::kapi_pci_find(vendor_id, device_id, 0, &mut k) } != 0 { return None; }
        Some(PciDevice {
            bus: (k.bdf >> 16) as u8,
            dev: (k.bdf >> 8) as u8,
            func: k.bdf as u8,
            vendor_id: k.vendor,
            device_id: k.device,
            class: (k.class >> 16) as u8,
            subclass: (k.class >> 8) as u8,
            prog_if: k.class as u8,
            bars: k.bars,
        })
    }

    pub unsafe fn pci_read_config_8(bus: u8, dev: u8, func: u8, off: u8) -> u8 {
        sys::kapi_pci_cfg_read(bdf(bus, dev, func), off as u16, 1) as u8
    }
    pub unsafe fn pci_read_config_16(bus: u8, dev: u8, func: u8, off: u8) -> u16 {
        sys::kapi_pci_cfg_read(bdf(bus, dev, func), off as u16, 2) as u16
    }
    pub unsafe fn pci_read_config_32(bus: u8, dev: u8, func: u8, off: u8) -> u32 {
        sys::kapi_pci_cfg_read(bdf(bus, dev, func), off as u16, 4)
    }
    pub unsafe fn pci_write_config_16(bus: u8, dev: u8, func: u8, off: u8, val: u16) {
        sys::kapi_pci_cfg_write(bdf(bus, dev, func), off as u16, 2, val as u32)
    }

    /// Kernel log line (the trailing newline is stripped; `kapi_log` adds its own).
    pub fn serial_debug(msg: &str) {
        let m = msg.strip_suffix('\n').unwrap_or(msg);
        unsafe { sys::kapi_log(6, m.as_ptr(), m.len()) }
    }
}

pub mod mm {
    use super::*;

    pub fn phys_to_virt(phys: usize) -> usize { unsafe { sys::kapi_phys_to_virt(phys as u64) } }

    pub mod paging {
        use super::*;

        /// Local flag set; translated to KMAP_* (a module can never ask for exec or user).
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub struct PageFlags(u32);
        impl PageFlags {
            pub const PRESENT: PageFlags = PageFlags(0);
            pub const WRITABLE: PageFlags = PageFlags(KMAP_WRITABLE);
            pub const MMIO: PageFlags = PageFlags(KMAP_MMIO);
            pub const WRITECOMBINE: PageFlags = PageFlags(KMAP_WC);
            pub const fn bits(self) -> u32 { self.0 }
        }
        impl core::ops::BitOr for PageFlags {
            type Output = PageFlags;
            fn bitor(self, o: PageFlags) -> PageFlags { PageFlags(self.0 | o.0) }
        }

        /// None when the kernel could not map it. Callers keep their own fallback.
        pub unsafe fn map_kernel_device(phys: usize, size: usize, flags: PageFlags) -> Option<usize> {
            match sys::kapi_map_device(phys as u64, size, flags.0) { 0 => None, v => Some(v) }
        }
    }

    pub mod buddy {
        use super::*;

        pub fn alloc(order: usize) -> Option<usize> {
            match unsafe { sys::kapi_page_alloc(order as u32, 0) } { 0 => None, p => Some(p as usize) }
        }
        pub fn free(addr: usize, order: usize) { unsafe { sys::kapi_page_free(addr as u64, order as u32) } }
    }
}

pub mod sched {
    use super::*;

    pub fn monotonic_ns() -> u64 { unsafe { sys::kapi_monotonic_ns() } }

    pub mod random {
        use super::*;
        use core::sync::atomic::AtomicUsize;

        pub type SourceFn = fn(&mut [u8]) -> usize;
        const N: usize = 4;
        static FNS: [AtomicUsize; N] = [const { AtomicUsize::new(0) }; N];
        static USED: AtomicUsize = AtomicUsize::new(0);

        extern "C" fn tramp<const I: usize>(p: *mut u8, len: usize) -> usize {
            let f = FNS[I].load(Ordering::Acquire);
            if f == 0 || p.is_null() { return 0; }
            let f: SourceFn = unsafe { core::mem::transmute(f) };
            f(unsafe { core::slice::from_raw_parts_mut(p, len) })
        }

        /// Same contract as `sched::random::register_source`. The function is kept in a
        /// module static; the kernel gets an `extern "C"` trampoline (modules are permanent).
        pub fn register_source(name: &'static str, f: SourceFn) {
            let i = USED.fetch_add(1, Ordering::SeqCst);
            if i >= N { USED.store(N, Ordering::SeqCst); return; }
            FNS[i].store(f as usize, Ordering::Release);
            let t: extern "C" fn(*mut u8, usize) -> usize = match i {
                0 => tramp::<0>, 1 => tramp::<1>, 2 => tramp::<2>, _ => tramp::<3>,
            };
            unsafe { sys::kapi_hwrng_register(owner(), name.as_ptr(), name.len(), t); }
        }
    }
}
