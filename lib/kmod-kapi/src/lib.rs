//! Module-side kernel API shim (see Cargo.toml). Only `kapi_*` functions are imported.
#![no_std]

extern crate alloc;

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

    impl PciDevice {
        pub fn function(&self) -> PciFunction { PciFunction { bus: self.bus, dev: self.dev, func: self.func } }
    }

    /// One decoded BAR (same shape as the kernel's `pci::BarInfo`).
    #[derive(Debug, Clone, Copy)]
    pub struct BarInfo {
        pub index: usize,
        pub io: bool,
        pub is64: bool,
        pub prefetchable: bool,
        pub base: u64,
        pub size: u64,
    }

    /// bdf of the function whose `intx_line` was asked last; `irq::request_irq(IrqSource::Line)`
    /// turns the line back into the kernel's per-function `kapi_irq_request_line`.
    static INTX_BDF: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

    /// A PCI function addressed by bdf: the module-side twin of the kernel's `pci::PciFunction`
    /// over `kapi_pci_cfg_*`, `kapi_pci_enable`, `kapi_pci_find_cap` and `kapi_map_device`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PciFunction { pub bus: u8, pub dev: u8, pub func: u8 }

    impl From<PciFunction> for u32 {
        fn from(f: PciFunction) -> u32 { f.bdf() }
    }

    impl PciFunction {
        pub const fn new(bus: u8, dev: u8, func: u8) -> Self { Self { bus, dev, func } }
        pub fn bdf(&self) -> u32 { bdf(self.bus, self.dev, self.func) }
        fn rd(&self, off: u16, w: u8) -> u32 { unsafe { sys::kapi_pci_cfg_read(self.bdf(), off, w) } }
        fn wr(&self, off: u16, w: u8, v: u32) { unsafe { sys::kapi_pci_cfg_write(self.bdf(), off, w, v) } }
        pub fn read8(&self, off: u16) -> u8 { self.rd(off, 1) as u8 }
        pub fn read16(&self, off: u16) -> u16 { self.rd(off, 2) as u16 }
        pub fn read32(&self, off: u16) -> u32 { self.rd(off, 4) }
        pub fn write8(&self, off: u16, v: u8) { self.wr(off, 1, v as u32) }
        pub fn write16(&self, off: u16, v: u16) { self.wr(off, 2, v as u32) }
        pub fn write32(&self, off: u16, v: u32) { self.wr(off, 4, v) }
        pub fn find_capability(&self, id: u8) -> Option<u16> { find_capability(self.bdf(), id) }
        pub fn enable_mem_and_bus_master(&self) { let _ = enable(self.bdf(), true); }

        pub fn bar(&self, i: usize) -> Option<BarInfo> {
            if i >= 6 { return None; }
            let off = 0x10 + 4 * i as u16;
            let v = self.read32(off);
            if v & 1 == 1 {
                return Some(BarInfo { index: i, io: true, is64: false, prefetchable: false, base: (v & !3) as u64, size: 0 });
            }
            let is64 = (v & 6) == 4;
            let hi = if is64 && i < 5 { self.read32(off + 4) as u64 } else { 0 };
            Some(BarInfo { index: i, io: false, is64, prefetchable: v & 8 != 0, base: (hi << 32) | (v & !0xF) as u64, size: 0 })
        }

        /// Size memory BAR `i` (decode off, all-ones probe, restore; no local-IRQ mask, so call it
        /// from init before anything else touches the device).
        fn bar_size(&self, i: usize) -> u64 {
            let off = 0x10 + 4 * i as u16;
            let cmd = self.read16(0x04);
            self.write16(0x04, cmd & !0x3);
            let o = self.read32(off);
            self.write32(off, 0xFFFF_FFFF);
            let v = self.read32(off);
            self.write32(off, o);
            let size = if (o & 6) == 4 && i < 5 {
                let o2 = self.read32(off + 4);
                self.write32(off + 4, 0xFFFF_FFFF);
                let v2 = self.read32(off + 4);
                self.write32(off + 4, o2);
                (!(((v2 as u64) << 32) | (v & !0xF) as u64)).wrapping_add(1)
            } else {
                (!(v & !0xF)).wrapping_add(1) as u64
            };
            self.write16(0x04, cmd);
            size
        }

        /// Size and map memory BAR `i`; `(virtual address, size)`.
        pub fn map_bar(&self, i: usize, mode: mm::kva::CacheMode) -> Option<(usize, u64)> {
            let b = self.bar(i)?;
            if b.io || b.base == 0 { return None; }
            let size = self.bar_size(i);
            if size == 0 { return None; }
            let va = mm::kva::ioremap(b.base, size as usize, mode)?;
            Some((va, size))
        }

        /// INTx routing: only the aarch64 `virt` swizzle is known; elsewhere use MSI.
        pub fn intx_line(&self) -> Option<u32> {
            let pin = self.read8(0x3D);
            if pin == 0 || pin > 4 { return None; }
            INTX_BDF.store(self.bdf(), Ordering::SeqCst);
            #[cfg(target_arch = "aarch64")]
            { return Some(32 + 3 + ((self.dev as u32 + (pin as u32 - 1)) % 4)); }
            #[allow(unreachable_code)]
            None
        }
    }

    pub(super) fn intx_bdf() -> u32 { INTX_BDF.load(Ordering::SeqCst) }

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
        /// Physical pages below `limit` (exclusive).
        pub fn alloc_below(order: usize, limit: usize) -> Option<usize> {
            match unsafe { sys::kapi_page_alloc(order as u32, limit as u64) } { 0 => None, p => Some(p as usize) }
        }
        pub fn free(addr: usize, order: usize) { unsafe { sys::kapi_page_free(addr as u64, order as u32) } }
    }
}

pub mod sched {
    use super::*;

    pub fn monotonic_ns() -> u64 { unsafe { sys::kapi_monotonic_ns() } }

    pub mod kthread {
        use super::*;
        use alloc::boxed::Box;

        pub fn udelay(us: u64) { unsafe { sys::kapi_udelay(us) } }
        pub fn msleep(ms: u64) { unsafe { sys::kapi_msleep(ms) } }

        extern "C" fn tramp(arg: *mut u8) {
            let f: Box<Box<dyn FnOnce() + Send>> = unsafe { Box::from_raw(arg as *mut Box<dyn FnOnce() + Send>) };
            (*f)();
        }

        /// A kernel thread running `f`; `Some(())` when it started.
        pub fn spawn<F: FnOnce() + Send + 'static>(name: &str, f: F) -> Option<()> {
            let arg = Box::into_raw(Box::new(Box::new(f) as Box<dyn FnOnce() + Send>)) as *mut u8;
            let r = unsafe { sys::kapi_kthread_spawn(name.as_ptr(), name.len(), tramp, arg, owner()) };
            if r < 0 {
                drop(unsafe { Box::from_raw(arg as *mut Box<dyn FnOnce() + Send>) });
                None
            } else { Some(()) }
        }
    }

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

    /// Where an interrupt comes from (the Y build's `irq::IrqSource`, line variant only).
    #[derive(Clone, Copy, Debug)]
    pub enum IrqSource { Line(u32) }

    /// The INTx line of the function last passed to `PciFunction::intx_line`.
    pub fn request_irq(src: IrqSource, h: IrqHandler, ctx: usize) -> Result<IrqHandle, IrqError> {
        let IrqSource::Line(_) = src;
        request_line(super::pci::intx_bdf(), h, ctx)
    }

    /// Single-vector MSI on the function `bdf`.
    pub fn request_msi(bdf: impl Into<u32>, h: IrqHandler, ctx: usize) -> Result<IrqHandle, IrqError> {
        match unsafe { sys::kapi_irq_request_msi(owner(), bdf.into(), raw(h), ctx) } {
            v if v >= 0 => Ok(IrqHandle { vector: Some(v as u8), line: None }),
            e => Err(err(e)),
        }
    }

    /// The function's INTx line.
    pub fn request_line(bdf: impl Into<u32>, h: IrqHandler, ctx: usize) -> Result<IrqHandle, IrqError> {
        match unsafe { sys::kapi_irq_request_line(owner(), bdf.into(), raw(h), ctx) } {
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

        pub fn line(&self) -> alloc::string::String {
            alloc::format!("linkmon: irq_wakes {} timer_wakes {}",
                           self.irq_wakes.load(Ordering::Relaxed), self.timer_wakes.load(Ordering::Relaxed))
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

    pub use kmod_abi::{pm_phase, KPmOps};
    /// Register a full hook table in a phase (`pm_phase::*`). Err = kernel errno.
    pub fn register2(ops: &KPmOps, ctx: usize) -> Result<(), i32> {
        match unsafe { sys::kapi_pm_register2(owner(), ops, ctx) } { 0 => Ok(()), e => Err(e) }
    }
    /// Register suspend / resume / shutdown hooks (see `kmod_abi::KPmFn`). Err = kernel errno.
    pub fn register(suspend: Option<KPmFn>, resume: Option<KPmFn>, shutdown: Option<KPmFn>, ctx: usize) -> Result<(), i32> {
        match unsafe { sys::kapi_pm_register(owner(), suspend, resume, shutdown, ctx) } { 0 => Ok(()), e => Err(e) }
    }
}

/// Scheduler and address helpers (kmodW2 common kapi).
pub mod sys_common {
    use super::*;
    /// Physical address of a direct-map address, or None for a module-image address (not DMA-able).
    pub fn virt_to_phys(va: usize) -> Option<u64> {
        match unsafe { sys::kapi_virt_to_phys(va) } { u64::MAX => None, p => Some(p) }
    }
    pub fn can_sleep() -> bool { unsafe { sys::kapi_can_sleep() } }
    pub fn sleep_until(ns: u64) { unsafe { sys::kapi_sleep_until(ns) } }
    pub fn yield_now() { unsafe { sys::kapi_yield() } }
    pub fn irqs_masked() -> bool { unsafe { sys::kapi_irqs_masked() } }
    pub fn current_pid() -> u32 { unsafe { sys::kapi_current_pid() } }
}

/// The kernel command line.
pub fn cmdline() -> &'static str {
    let mut n = 0usize;
    let p = unsafe { sys::kapi_cmdline(&mut n) };
    if p.is_null() { return ""; }
    core::str::from_utf8(unsafe { core::slice::from_raw_parts(p, n) }).unwrap_or("")
}

/// Raw log line for drivers that print their own prefix (newline stripped; `kapi_log` adds one).
pub fn serial_raw(s: &str) { pci::serial_debug(s) }

pub mod v4l2 {
    use super::*;
    pub use kmod_abi::{KCamBuf, KCamDone, KCamInfo, KCameraOps, KFormat, KFrame, KCAM_GONE, KCAM_NEW_FRAME};

    /// Register a camera with the kernel's V4L2 layer. `ops` must be `'static` (modules are permanent).
    /// Err = kernel errno.
    pub fn register(ops: &'static KCameraOps, ctx: usize) -> Result<(), i32> {
        match unsafe { sys::kapi_v4l2_register(owner(), ops, ctx) } { 0 => Ok(()), e => Err(e) }
    }

    /// Remove the camera registered with `ctx` (the device went away).
    pub fn unregister(ctx: usize) { unsafe { sys::kapi_v4l2_unregister(ctx) } }
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
    pub use kmod_abi::{KUsbDetachFn, KUsbIf, KUsbProbeFn, KUsbSetup, KmodDevId, BUS_USB};

    /// Register a USB class driver. The kernel replays interfaces that were enumerated earlier and
    /// left unclaimed. `probe` returns 0 to claim the interface; `detach` runs when a claimed
    /// device leaves the bus. Err = kernel errno.
    pub fn class_register(ids: &'static [KmodDevId], probe: KUsbProbeFn, detach: Option<KUsbDetachFn>,
                          ctx: usize) -> Result<(), i32> {
        match unsafe { sys::kapi_usb_class_register(owner(), ids.as_ptr(), ids.len(), probe, detach, ctx) } {
            0 => Ok(()),
            e => Err(e),
        }
    }

    fn rc(r: i32) -> Result<usize, i32> { if r < 0 { Err(r) } else { Ok(r as usize) } }

    /// Control transfer on a claimed function; `buf` must hold `setup.length` bytes. Ok = bytes
    /// transferred (IN) or 0. Err = -errno. Task context.
    pub fn ctrl(handle: u64, setup: &KUsbSetup, buf: &mut [u8], timeout_ms: u32) -> Result<usize, i32> {
        rc(unsafe { sys::kapi_usb_ctrl(handle, setup, buf.as_mut_ptr(), buf.len(), timeout_ms) })
    }

    /// SET_INTERFACE. Task context.
    pub fn set_alt(handle: u64, iface: u8, alt: u8) -> Result<(), i32> {
        rc(unsafe { sys::kapi_usb_set_alt(handle, iface, alt) }).map(|_| ())
    }

    /// The function's full configuration descriptor.
    pub fn config_descriptor(handle: u64) -> Result<alloc::vec::Vec<u8>, i32> {
        let n = rc(unsafe { sys::kapi_usb_cfg(handle, core::ptr::null_mut(), 0) })?;
        let mut v = alloc::vec![0u8; n];
        rc(unsafe { sys::kapi_usb_cfg(handle, v.as_mut_ptr(), n) })?;
        Ok(v)
    }

    /// What `isoch_submit` found.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Poll { Ok, Busy, Gone }

    /// Open an isochronous endpoint (see `kapi_usb_isoch_open`). Ok = pipe handle. Task context.
    pub fn isoch_open(handle: u64, iface: u8, alt: u8, ep_addr: u8, fifo_bytes: usize, ntds: usize,
                      framed: bool) -> Result<u64, i32> {
        let r = unsafe { sys::kapi_usb_isoch_open(handle, iface, alt, ep_addr, fifo_bytes, ntds, framed) };
        if r < 0 { Err(r as i32) } else { Ok(r as u64) }
    }

    /// Retire completed transfers, keep the ring full. Interrupt-safe, never blocks.
    pub fn isoch_submit(pipe: u64) -> Poll {
        match unsafe { sys::kapi_usb_isoch_submit(pipe) } { 0 => Poll::Ok, 2 => Poll::Gone, _ => Poll::Busy }
    }

    /// Oldest framed packet: Some(length; may exceed `buf.len()`), None when none is queued.
    pub fn isoch_reap(pipe: u64, buf: &mut [u8]) -> Option<usize> {
        let r = unsafe { sys::kapi_usb_isoch_reap(pipe, buf.as_mut_ptr(), buf.len()) };
        if r < 0 { None } else { Some(r as usize) }
    }

    /// Stop and free the pipe. Task context.
    pub fn isoch_close(pipe: u64) { unsafe { sys::kapi_usb_isoch_close(pipe) } }
}
