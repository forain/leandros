//! Module-side kernel API shim (see Cargo.toml). Only `kapi_*` functions are imported.
#![no_std]

use core::sync::atomic::{AtomicU16, Ordering};
use kmod_abi::{sys, KPciDev, KmodId, KMAP_MMIO, KMAP_UC, KMAP_WC, KMAP_WRITABLE};

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

    /// Memory decode on, bus mastering too when asked. False when the function is gone.
    pub fn enable(bdf: u32, bus_master: bool) -> bool { unsafe { sys::kapi_pci_enable(bdf, bus_master) == 0 } }

    /// Config offset of capability `cap_id` (`cap::PM`, `cap::PCIE`, ...).
    pub fn find_capability(bdf: u32, cap_id: u8) -> Option<u16> {
        match unsafe { sys::kapi_pci_find_cap(bdf, cap_id) } { 0 => None, o => Some(o) }
    }

    pub mod cap {
        pub const PM: u8 = 0x01;
        pub const MSI: u8 = 0x05;
        pub const PCIE: u8 = 0x10;
        pub const MSIX: u8 = 0x11;
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
            pub const UNCACHED: PageFlags = PageFlags(KMAP_UC);
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

    pub mod kva {
        use super::*;

        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub enum CacheMode { Uncached }

        /// Uncached device mapping of `len` bytes at `phys` (the BAR-mapping mode of the Y build).
        pub fn ioremap(phys: u64, len: usize, mode: CacheMode) -> Option<usize> {
            let CacheMode::Uncached = mode;
            match unsafe { sys::kapi_map_device(phys, len, KMAP_UC | KMAP_WRITABLE | KMAP_MMIO) } { 0 => None, v => Some(v) }
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

pub mod irq {
    use super::*;

    #[repr(u32)]
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum IrqReturn { None = 0, Handled = 1 }

    /// Same shape as the kernel's `IrqHandler`; `IrqReturn` is `repr(u32)`, 0 or 1.
    pub type IrqHandler = extern "C" fn(usize) -> IrqReturn;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum IrqError { NoSpace, NotFound, Unsupported, Busy }

    fn err(e: i64) -> IrqError {
        match e { -19 => IrqError::NotFound, -95 => IrqError::Unsupported, -16 => IrqError::Busy, _ => IrqError::NoSpace }
    }

    /// An installed interrupt; permanent (modules never free it in v1).
    #[derive(Clone, Copy, Debug)]
    pub struct IrqHandle { vector: Option<u8>, line: Option<u32> }
    impl IrqHandle {
        pub fn vector(&self) -> Option<u8> { self.vector }
        pub fn line(&self) -> Option<u32> { self.line }
    }

    fn raw(h: IrqHandler) -> kmod_abi::KIrqHandler { unsafe { core::mem::transmute(h) } }

    /// Single-vector MSI on the function `bdf`.
    pub fn request_msi(bdf: u32, h: IrqHandler, ctx: usize) -> Result<IrqHandle, IrqError> {
        match unsafe { sys::kapi_irq_request_msi(owner(), bdf, raw(h), ctx) } {
            v if v >= 0 => Ok(IrqHandle { vector: Some(v as u8), line: None }),
            e => Err(err(e)),
        }
    }

    /// The function's INTx line.
    pub fn request_line(bdf: u32, h: IrqHandler, ctx: usize) -> Result<IrqHandle, IrqError> {
        match unsafe { sys::kapi_irq_request_line(owner(), bdf, raw(h), ctx) } {
            l if l >= 0 => Ok(IrqHandle { vector: None, line: Some(l as u32) }),
            e => Err(err(e)),
        }
    }
}

pub mod netdev {
    use super::*;
    use core::sync::atomic::{AtomicU64, AtomicUsize};
    pub use kmod_abi::KNetifOps;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Kind { Wired, Wifi }

    /// Register a netif; the ops table must outlive the module (modules are permanent).
    /// Returns the netif id, or the kernel's negative errno.
    pub fn register(kind: Kind, prefix: &str, ops: &'static KNetifOps, ctx: usize) -> Result<u8, i32> {
        let k = if kind == Kind::Wifi { kmod_abi::KNET_WIFI } else { kmod_abi::KNET_WIRED };
        match unsafe { sys::kapi_netdev_register(owner(), k, prefix.as_ptr(), prefix.len(), ops, ctx) } {
            id if id >= 0 => Ok(id as u8),
            e => Err(e),
        }
    }

    pub fn kick() { unsafe { sys::kapi_netdev_kick() } }
    pub fn kick_from_irq() { unsafe { sys::kapi_netdev_kick_from_irq() } }

    /// Same surface as the kernel's `LinkMon`, over a kernel wait-queue handle created on first use.
    pub struct LinkMon {
        h: AtomicUsize,
        pub irq_wakes: AtomicU64,
        pub timer_wakes: AtomicU64,
    }

    impl LinkMon {
        pub const fn new() -> Self {
            Self { h: AtomicUsize::new(0), irq_wakes: AtomicU64::new(0), timer_wakes: AtomicU64::new(0) }
        }

        fn handle(&self) -> usize {
            let h = self.h.load(Ordering::Acquire);
            if h != 0 { return h; }
            let n = unsafe { sys::kapi_wait_new() };
            match self.h.compare_exchange(0, n, Ordering::AcqRel, Ordering::Acquire) { Ok(_) => n, Err(cur) => cur }
        }

        /// Any context (ISR included).
        pub fn notify(&self) { unsafe { sys::kapi_wait_notify(self.handle()) } }

        /// Sleep up to `timeout_ms` or until an event arrives (the kernel tracks what this waiter has seen).
        /// `seen` is kept for signature parity with the Y `LinkMon`.
        pub fn wait(&self, _seen: &mut u64, timeout_ms: u64) -> bool {
            let hit = unsafe { sys::kapi_wait_timeout(self.handle(), timeout_ms * 1_000_000) };
            if hit { self.irq_wakes.fetch_add(1, Ordering::Relaxed); } else { self.timer_wakes.fetch_add(1, Ordering::Relaxed); }
            hit
        }
    }
}

pub mod pm {
    use super::*;
    pub use kmod_abi::KPmFn;

    /// Register suspend / resume / shutdown hooks (see `kmod_abi::KPmFn`). Err = kernel errno.
    pub fn register(suspend: Option<KPmFn>, resume: Option<KPmFn>, shutdown: Option<KPmFn>, ctx: usize) -> Result<(), i32> {
        match unsafe { sys::kapi_pm_register(owner(), suspend, resume, shutdown, ctx) } { 0 => Ok(()), e => Err(e) }
    }
}

pub mod v4l2 {
    use super::*;
    pub use kmod_abi::{KCamBuf, KCamDone, KCamInfo, KCameraOps, KFormat, KFrame, KCAM_GONE, KCAM_NEW_FRAME};

    /// Register a camera with the kernel's V4L2 layer. `ops` must be `'static` (modules are permanent).
    /// Err = kernel errno.
    pub fn register(ops: &'static KCameraOps, ctx: usize) -> Result<(), i32> {
        match unsafe { sys::kapi_v4l2_register(owner(), ops, ctx) } { 0 => Ok(()), e => Err(e) }
    }
}

pub mod tick {
    use super::*;
    pub use kmod_abi::KTickFn;

    /// Run `f(ctx)` on every USB timer tick (interrupt context, must not block).
    pub fn register(f: KTickFn, ctx: usize) -> Result<(), i32> {
        match unsafe { sys::kapi_tick_register(owner(), f, ctx) } { 0 => Ok(()), e => Err(e) }
    }
}

pub mod usb {
    use super::*;
    pub use kmod_abi::{KUsbIf, KUsbProbeFn, KmodDevId, BUS_USB};

    /// Register a USB class driver. The kernel replays interfaces that were enumerated earlier and
    /// left unclaimed. `probe` returns 0 to claim the interface. Err = kernel errno.
    pub fn class_register(ids: &'static [KmodDevId], probe: KUsbProbeFn, ctx: usize) -> Result<(), i32> {
        match unsafe { sys::kapi_usb_class_register(owner(), ids.as_ptr(), ids.len(), probe, ctx) } {
            0 => Ok(()),
            e => Err(e),
        }
    }
}
