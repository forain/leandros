//! Module-side kernel API shim (see Cargo.toml). Only `kapi_*` functions are imported.
#![no_std]

extern crate alloc;

use core::sync::atomic::{AtomicU16, Ordering};
use kmod_abi::{sys, KPciDev, KmodId, KMAP_MMIO, KMAP_UC, KMAP_WC, KMAP_WRITABLE};

static OWNER: AtomicU16 = AtomicU16::new(0);

/// Called once by the module runtime from `kmod_init` with `ctx.owner`.
pub fn set_owner(id: KmodId) { OWNER.store(id.0, Ordering::SeqCst); }
pub fn owner() -> KmodId { KmodId(OWNER.load(Ordering::SeqCst)) }

pub mod blk;
pub mod ksync;
pub mod plat;
pub mod dma40;

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
        pub fn set_intx_disable(&self, disable: bool) {
            let c = self.read16(0x04);
            let n = if disable { c | PCI_CMD_INTX_DISABLE } else { c & !PCI_CMD_INTX_DISABLE };
            if n != c { self.write16(0x04, n); }
        }

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
        pub fn bar_size(&self, i: usize) -> u64 {
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

    /// Every function with this vendor/device id (kapi_pci_find walks them by index).
    /// Every PCI function (the kernel's `pci::scan`), via `kapi_pci_scan`.
    pub fn scan() -> alloc::vec::Vec<PciDevice> {
        let mut v = alloc::vec::Vec::new();
        let mut i = 0u32;
        while let Some(k) = super::sys_common::pci_scan(i) {
            v.push(PciDevice {
                bus: (k.bdf >> 16) as u8, dev: (k.bdf >> 8) as u8, func: k.bdf as u8,
                vendor_id: k.vendor, device_id: k.device,
                class: (k.class >> 16) as u8, subclass: (k.class >> 8) as u8, prog_if: k.class as u8,
                bars: k.bars,
            });
            i += 1;
        }
        v
    }

    pub fn find_all_devices(vendor_id: u16, device_id: u16) -> alloc::vec::Vec<PciDevice> {
        let mut v = alloc::vec::Vec::new();
        for index in 0..64u32 {
            let mut k = KPciDev::default();
            if unsafe { sys::kapi_pci_find(vendor_id, device_id, index, &mut k) } != 0 { break; }
            v.push(PciDevice {
                bus: (k.bdf >> 16) as u8, dev: (k.bdf >> 8) as u8, func: k.bdf as u8,
                vendor_id: k.vendor, device_id: k.device,
                class: (k.class >> 16) as u8, subclass: (k.class >> 8) as u8, prog_if: k.class as u8,
                bars: k.bars,
            });
        }
        v
    }

    pub fn serial_debug_hex(v: u32) {
        let mut b = [0u8; 10];
        b[0] = b'0'; b[1] = b'x';
        for i in 0..8 { b[2 + i] = b"0123456789abcdef"[((v >> (28 - 4 * i)) & 0xf) as usize]; }
        serial_debug(core::str::from_utf8(&b).unwrap_or(""));
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
    /// Config read that also works for offsets the built-in's ECAM path treats specially; one path here.
    pub unsafe fn pci_read_config_32_any(bus: u8, dev: u8, func: u8, off: u8) -> u32 { pci_read_config_32(bus, dev, func, off) }
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
    /// Verbose-boot trace in the kernel; a module build drops it.
    pub fn rdebug(_msg: &str) {}
    pub fn rdebug_hex(_v: u32) {}
    pub fn serial_debug(msg: &str) {
        let m = msg.strip_suffix('\n').unwrap_or(msg);
        unsafe { sys::kapi_log(6, m.as_ptr(), m.len()) }
    }
}

pub mod mm {
    use super::*;

    pub fn phys_to_virt(phys: usize) -> usize { unsafe { sys::kapi_phys_to_virt(phys as u64) } }
    /// Physical address of a direct-map address (0 for a module-image address: not DMA-able).
    pub fn virt_to_phys(va: usize) -> usize { super::sys_common::virt_to_phys(va).unwrap_or(0) as usize }

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
        pub enum CacheMode { Uncached, WriteCombining, WriteBack }

        /// Device mapping of `len` bytes at `phys`. Uncached is the BAR mode of the Y build; the
        /// other modes go through `kapi_ioremap` (0 uncached, 1 write-combining, 2 write-back).
        pub fn ioremap(phys: u64, len: usize, mode: CacheMode) -> Option<usize> {
            match mode {
                CacheMode::Uncached => match unsafe { sys::kapi_map_device(phys, len, KMAP_UC | KMAP_WRITABLE | KMAP_MMIO) } { 0 => None, v => Some(v) },
                CacheMode::WriteCombining => match unsafe { sys::kapi_ioremap(phys, len, 1) } { 0 => None, v => Some(v) },
                CacheMode::WriteBack => match unsafe { sys::kapi_ioremap(phys, len, 2) } { 0 => None, v => Some(v) },
            }
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
        pub fn total_pages() -> usize { info()[0] as usize }
        pub fn free_pages() -> usize { info()[1] as usize }
        /// `[total pages, free pages, page size]`.
        pub fn info() -> [u64; 3] { let mut o = [0u64; 3]; unsafe { sys::kapi_mem_info(o.as_mut_ptr()) }; o }
    }

    pub mod pageref {
        use super::*;
        /// One more reference on each order-0 frame.
        pub fn inc_many(frames: &[usize]) { unsafe { sys::kapi_page_ref_many(frames.as_ptr(), frames.len()) } }
        /// Free a block that mmap pins may still hold; true when some page was still held.
        pub fn free_block_pinned(phys: usize, order: usize) -> bool { unsafe { sys::kapi_page_free_pinned(phys as u64, order as u32) } }
    }
}

pub mod sched {
    use super::*;

    pub fn monotonic_ns() -> u64 { unsafe { sys::kapi_monotonic_ns() } }
    pub fn monotonic_us() -> u64 { monotonic_ns() / 1000 }

    pub mod kthread {
        use super::*;
        use alloc::boxed::Box;

        pub fn udelay(us: u64) { unsafe { sys::kapi_udelay(us) } }
        pub fn msleep(ms: u64) { unsafe { sys::kapi_msleep(ms) } }
        /// True once the owning module generation is being unloaded: thread bodies must return.
        pub fn should_stop() -> bool { unsafe { sys::kapi_kthread_should_stop() } }

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

        /// Fill `buf` from the kernel CSPRNG.
        pub fn fill(buf: &mut [u8]) { unsafe { sys::kapi_random_fill(buf.as_mut_ptr(), buf.len()) } }

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

    /// MSI-X of one function (x86_64): the kernel enables the table on the first request.
    #[derive(Clone, Copy, Debug)]
    pub struct MsixTable { bdf: u32 }
    impl MsixTable {
        pub fn enable(func: super::pci::PciFunction) -> Result<MsixTable, IrqError> {
            if func.find_capability(super::pci::cap::MSIX).is_none() { return Err(IrqError::NotFound); }
            Ok(MsixTable { bdf: func.bdf() })
        }
        pub fn request(&self, entry: u16, h: IrqHandler, ctx: usize) -> Result<IrqHandle, IrqError> {
            match super::sys_common::irq_request_msix(self.bdf, entry, raw(h), ctx) {
                Ok(v) => Ok(IrqHandle { vector: Some(v), line: None }),
                Err(e) => Err(err(e)),
            }
        }
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

pub mod input {
    use super::*;
    use kmod_abi::input::sys_input;
    pub use kmod_abi::input::{INPUT_KEYBOARD, INPUT_POINTER};
    /// Claim the evdev node for `kind`; the node index, or -errno.
    pub fn register(kind: u32, name: &str, bus: u16, vid: u16, pid: u16) -> i32 {
        unsafe { sys_input::kapi_input_register(owner(), kind, name.as_ptr(), name.len(), bus, vid, pid) }
    }
    pub fn claim(node: u32) -> bool { register(node, "virtio-input", 0x06, 0x1af4, 0x1052) >= 0 }
    pub fn push_event(node: u32, ty: u16, code: u16, value: i32) {
        unsafe { sys_input::kapi_input_event(node as i32, ty, code, value) }
    }
    pub fn set_device_caps(node: u32, rel: &[u8], key: &[u8]) {
        unsafe { sys_input::kapi_input_set_caps(node as i32, rel.as_ptr(), rel.len(), key.as_ptr(), key.len()); }
    }
    pub use kmod_abi::input::{INPUT_DEV_KEYBOARD, INPUT_DEV_MOUSE, REL_SRC_USB};
    /// Unregister a per-device node.
    pub fn unregister(node: u32) { unsafe { sys_input::kapi_input_unregister(node as i32); } }
    /// Key edge / SYN to the kernel's console keyboard.
    pub fn push_console(ty: u16, code: u16, value: i32) { unsafe { sys_input::kapi_input_event_console(ty, code, value) } }
    /// Keyboard LEDs to show (HID output bit order).
    pub fn leds() -> u8 { unsafe { sys_input::kapi_input_leds() as u8 } }
    /// Console typematic (delay, period) in ms.
    pub fn repeat() -> (u32, u32) {
        let (mut d, mut r) = (0u32, 0u32);
        unsafe { sys_input::kapi_input_repeat(&mut d, &mut r) };
        (d, r)
    }
    pub fn rel_report(src: u32, dx: i32, dy: i32, wheel: i32, hwheel: i32, buttons: u8) {
        unsafe { sys_input::kapi_rel_pointer_report(src, dx, dy, wheel, hwheel, buttons) }
    }
    pub fn rel_advertise(buttons: u8, hwheel: bool, hires: bool) {
        unsafe { sys_input::kapi_rel_pointer_advertise(buttons, hwheel, hires) }
    }
    pub fn register_stats_hook(_f: fn(&mut dyn core::fmt::Write)) {}
    pub fn vt_handoff_stats() -> (u64, u64) { (0, 0) }
}

pub mod snd {
    use super::*;
    pub use kmod_abi::{KSndOps, KSND_CAPTURE};
    /// Register the sound backend (one per machine); the ops table must outlive the module.
    /// Returns 0 or the kernel's negative errno (-16 a backend is already attached).
    pub fn register(ops: &'static KSndOps, ctx: usize) -> Result<(), i32> {
        match unsafe { sys::kapi_snd_register(owner(), ops, ctx) } { 0 => Ok(()), e => Err(e) }
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

/// DRM class (W2-7a): the uapi table (`drivers::drm_class::KDrmOps`), the fake-offset window, fences and syncobjs.
pub mod drm {
    use super::{owner, sys};
    /// Register the table (valid for the life of the module). 0, -22 bad table, -17 backend already installed.
    pub fn register(ops: *const u8, ctx: usize) -> i32 { unsafe { sys::kapi_drm_register(owner(), ops, ctx) } }
    /// KMS table kinds for `kms_register` (`drivers::kms_class::KIND_*`).
    pub const KMS_BACKEND: u32 = 0; pub const KMS_FLIP: u32 = 1; pub const KMS_CURSOR: u32 = 2;
    pub const KMS_OUTPUTS: u32 = 3; pub const KMS_PIPE2: u32 = 4; pub const KMS_CURSOR2: u32 = 5;
    /// Register a KMS table of `kind` (valid for the life of the module). 0, -22 bad, -17 already registered.
    pub fn kms_register(kind: u32, ops: *const u8, ctx: usize) -> i32 { unsafe { sys::kapi_kms_register(owner(), kind, ops, ctx) } }
    pub fn kms_set_internal_panel(v: bool) { unsafe { sys::kapi_kms_set_internal_panel(v) } }
    pub fn hotplug(minor: u8) { unsafe { sys::kapi_drm_hotplug(minor) } }
    pub fn ext_present_stats() -> [u64; 3] { let mut o = [0u64; 3]; unsafe { sys::kapi_ext_present_stats(o.as_mut_ptr()) }; o }
    pub fn tmpfs_census() -> Option<[u64; 9]> { let mut o = [0u64; 9]; if unsafe { sys::kapi_tmpfs_census(o.as_mut_ptr()) } { Some(o) } else { None } }
    pub fn offset_alloc(real: u64, size: u64) -> Option<u64> { match unsafe { sys::kapi_drm_offset_alloc(real, size) } { 0 => None, o => Some(o) } }
    pub fn offset_release(real: u64) { unsafe { sys::kapi_drm_offset_release(real) } }
    pub fn fence_wait(ids: &[u64], deadline_ns: u64) -> i32 { unsafe { sys::kapi_fence_wait(ids.as_ptr(), ids.len(), deadline_ns) } }
    pub fn syncobj_install(handle: u32, open_id: u32, point: u64, fence: u64) -> i32 { unsafe { sys::kapi_syncobj_install(handle, open_id, point, fence) } }
    pub fn syncobj_exists(handle: u32, open_id: u32) -> bool { unsafe { sys::kapi_syncobj_exists(handle, open_id) } }
    pub fn syncobj_in_fences(handle: u32, open_id: u32, point: u64, out: &mut [u64]) -> i64 {
        unsafe { sys::kapi_syncobj_in_fences(handle, open_id, point, out.as_mut_ptr(), out.len()) }
    }
    pub fn fence_irq_hint() { unsafe { sys::kapi_fence_irq_hint() } }
    pub fn kms_event_complete(cookie: u32, seq: u32, ts_ns: u64) { unsafe { sys::kapi_kms_event_complete(cookie, seq, ts_ns) } }
}

/// Boot framebuffer and backlight (K2b).
pub mod gfx {
    use super::*;
    pub use kmod_abi::{KBlGetFn, KBlSetFn, KBootFb};
    pub fn boot_fb() -> Option<KBootFb> {
        let mut f = KBootFb::default();
        if unsafe { sys::kapi_boot_fb(&mut f) } { Some(f) } else { None }
    }
    /// Register the panel backlight provider. 0, -22 no callbacks. Withdrawn if the module's init fails.
    pub fn backlight_register(get: Option<KBlGetFn>, set: Option<KBlSetFn>, ctx: usize) -> i32 {
        unsafe { sys::kapi_backlight_register(owner(), get, set, ctx) }
    }
}

pub mod wifi {
    use super::*;
    /// Hand a radio table (`drivers::wifi_radio::KWifiRadioOps`) and the optional legacy credential
    /// (`KLegacy`) to the kernel's Wi-Fi class. Both stay valid for the life of the module.
    pub fn register(ops: *const u8, ctx: usize, legacy: *const u8) -> i32 {
        unsafe { sys::kapi_wifi_register(owner(), ops, ctx, legacy) }
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
    use kmod_abi::KIrqHandler;
    /// Physical address of a direct-map address, or None for a module-image address (not DMA-able).
    pub fn virt_to_phys(va: usize) -> Option<u64> {
        match unsafe { sys::kapi_virt_to_phys(va) } { u64::MAX => None, p => Some(p) }
    }
    pub fn can_sleep() -> bool { unsafe { sys::kapi_can_sleep() } }
    pub fn sleep_until(ns: u64) { unsafe { sys::kapi_sleep_until(ns) } }
    pub fn yield_now() { unsafe { sys::kapi_yield() } }
    pub fn irqs_masked() -> bool { unsafe { sys::kapi_irqs_masked() } }
    pub fn current_pid() -> u32 { unsafe { sys::kapi_current_pid() } }
    /// Map device memory: cache 0 uncached, 1 write-combining, 2 write-back. None on failure.
    pub fn ioremap(phys: u64, len: usize, cache: u32) -> Option<usize> {
        match unsafe { sys::kapi_ioremap(phys, len, cache) } { 0 => None, v => Some(v) }
    }
    pub fn iounmap(va: usize, len: usize) { unsafe { sys::kapi_iounmap(va, len) } }
    /// dir: 0 to device, 1 to CPU, 2 both.
    pub fn dma_sync(va: usize, len: usize, dir: u32) { unsafe { sys::kapi_dma_sync(va, len, dir) } }
    pub fn dma_coherent() -> bool { unsafe { sys::kapi_dma_coherent() } }
    /// Sleeping mutex handle (permanent).
    pub fn mutex_new(name: &str) -> usize { unsafe { sys::kapi_mutex_new(name.as_ptr(), name.len()) } }
    pub fn mutex_lock(h: usize) { unsafe { sys::kapi_mutex_lock(h) } }
    pub fn mutex_unlock(h: usize) { unsafe { sys::kapi_mutex_unlock(h) } }
    pub fn mutex_trylock(h: usize) -> bool { unsafe { sys::kapi_mutex_trylock(h) } }
    pub fn sem_new(n: u32) -> usize { unsafe { sys::kapi_sem_new(n) } }
    pub fn sem_down_timeout(h: usize, ns: u64) -> bool { unsafe { sys::kapi_sem_down_timeout(h, ns) } }
    pub fn sem_up(h: usize) { unsafe { sys::kapi_sem_up(h) } }
    pub fn hrtimer_new(cb: extern "C" fn(usize), ctx: usize, irq_mode: bool) -> usize {
        unsafe { sys::kapi_hrtimer_new(Some(cb), ctx, irq_mode) }
    }
    pub fn hrtimer_arm(h: usize, ns: u64) { unsafe { sys::kapi_hrtimer_arm(h, ns) } }
    pub fn hrtimer_cancel(h: usize) { unsafe { sys::kapi_hrtimer_cancel(h) } }
    pub fn preempt_disable() { unsafe { sys::kapi_preempt_disable() } }
    pub fn preempt_enable() { unsafe { sys::kapi_preempt_enable() } }
    pub fn random_fill(buf: &mut [u8]) { unsafe { sys::kapi_random_fill(buf.as_mut_ptr(), buf.len()) } }
    /// Ioctl-thread only. Err(-14) bad user range.
    pub fn copy_from_user(dst: &mut [u8], uaddr: usize) -> Result<(), i32> {
        match unsafe { sys::kapi_copy_from_user(dst.as_mut_ptr(), uaddr, dst.len()) } { 0 => Ok(()), e => Err(e) }
    }
    pub fn copy_to_user(uaddr: usize, src: &[u8]) -> Result<(), i32> {
        match unsafe { sys::kapi_copy_to_user(uaddr, src.as_ptr(), src.len()) } { 0 => Ok(()), e => Err(e) }
    }
    /// The index-th PCI function of any kind; None past the end.
    pub fn pci_scan(index: u32) -> Option<KPciDev> {
        let mut d = core::mem::MaybeUninit::<KPciDev>::uninit();
        if unsafe { sys::kapi_pci_scan(index, d.as_mut_ptr()) } == 0 { Some(unsafe { d.assume_init() }) } else { None }
    }
    pub fn irq_request_msix(bdf: u32, entry: u16, handler: KIrqHandler, ctx: usize) -> Result<u8, i64> {
        match unsafe { sys::kapi_irq_request_msix(owner(), bdf, entry, handler, ctx) } { v if v < 0 => Err(v), v => Ok(v as u8) }
    }
    pub fn fw_root_ready() -> bool { unsafe { sys::kapi_fw_root_ready() } }
    /// Load firmware: (handle, data, len). The data stays valid until `fw_release(handle)`.
    pub fn fw_request(path: &str, max: usize) -> Option<(usize, *const u8, usize)> {
        let (mut n, mut p) = (0usize, core::ptr::null::<u8>());
        match unsafe { sys::kapi_fw_request(path.as_ptr(), path.len(), max, &mut n, &mut p) } { 0 => None, h => Some((h, p, n)) }
    }
    pub fn fw_release(h: usize) { unsafe { sys::kapi_fw_release(h) } }
    /// Command-line secret; the caller must have registered a netdev. Err is the negative errno-style code.
    pub fn secret_peek(key: &str, buf: &mut [u8]) -> Result<usize, isize> {
        match unsafe { sys::kapi_secret_peek(owner(), key.as_ptr(), key.len(), buf.as_mut_ptr(), buf.len()) } { n if n < 0 => Err(n), n => Ok(n as usize) }
    }
    /// (EBUSY refusals, pending backend events).
    pub fn ext_event_stats() -> (u64, u64) {
        let mut o = [0u64; 2];
        unsafe { sys::kapi_ext_event_stats(o.as_mut_ptr()) };
        (o[0], o[1])
    }
    /// Bytes of the PRIME/GEM counters line written to `buf` (0 when all zero).
    pub fn proc_prime(buf: &mut [u8]) -> Result<usize, isize> {
        match unsafe { sys::kapi_proc_prime(buf.as_mut_ptr(), buf.len()) } { n if n < 0 => Err(n), n => Ok(n as usize) }
    }
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

    /// Open the interrupt-IN endpoint `ep` of a boot HID interface. Ok = pipe handle. Task context.
    pub fn intr_open(handle: u64, ep: u8, max: usize) -> Result<u64, i32> {
        let r = unsafe { sys::kapi_usb_intr_open(handle, ep, max) };
        if r < 0 { Err(r as i32) } else { Ok(r as u64) }
    }
    /// Next report: Ok(len) (may exceed `buf.len()`), Err(-11) none, Err(-16) controller busy, Err(-19) gone.
    /// Never blocks (tick context).
    pub fn intr_poll(pipe: u64, buf: &mut [u8]) -> Result<usize, i32> {
        rc(unsafe { sys::kapi_usb_intr_poll(pipe, buf.as_mut_ptr(), buf.len()) } as i32)
    }
    /// HID output report without waiting. Ok queued; Err(-16) in flight, Err(-95) unusable.
    pub fn intr_out(pipe: u64, data: &[u8]) -> Result<(), i32> {
        rc(unsafe { sys::kapi_usb_intr_out(pipe, data.as_ptr(), data.len()) }).map(|_| ())
    }
    pub fn intr_close(pipe: u64) { unsafe { sys::kapi_usb_intr_close(pipe) } }

    /// Bulk-only transfer ops of `bulk`.
    pub const BULK_OUT: u32 = 0;
    pub const BULK_IN: u32 = 1;
    pub const BULK_CLEAR_IN: u32 = 2;
    pub const BULK_CLEAR_OUT: u32 = 3;
    pub const BULK_CTL_OUT: u32 = 4;
    pub const BULK_CTL_IN: u32 = 5;
    /// Configure the bulk-only interface `iface`. Ok = pipe handle. Task context.
    pub fn bulk_open(handle: u64, iface: u8) -> Result<u64, i32> {
        let r = unsafe { sys::kapi_usb_bulk_open(handle, iface) };
        if r < 0 { Err(r as i32) } else { Ok(r as u64) }
    }
    /// One transfer (see the kernel's `kapi_usb_bulk`): Ok(bytes) or Err(-errno). Sleeps.
    pub fn bulk(pipe: u64, op: u32, arg: u64, buf: *mut u8, len: usize, timeout_ns: u64) -> Result<usize, i32> {
        let r = unsafe { sys::kapi_usb_bulk(pipe, op, arg, buf, len, timeout_ns) };
        if r < 0 { Err(r as i32) } else { Ok(r as usize) }
    }
    pub fn bulk_close(pipe: u64) { unsafe { sys::kapi_usb_bulk_close(pipe) } }

    /// `bt_poll` flags.
    pub const BT_BULK: u32 = 1;
    pub const BT_RESET_EV: u32 = 2;
    pub const BT_RESET_ACL: u32 = 4;
    /// Configure the HCI interface of a class-claimed function. Ok = pipe handle. Task context.
    pub fn bt_open(handle: u64) -> Result<u64, i32> {
        let r = unsafe { sys::kapi_usb_bt_open(handle) };
        if r < 0 { Err(r as i32) } else { Ok(r as u64) }
    }
    /// Next received chunk: Ok((len, flags)) (len may exceed `buf`), Err(-11) none, Err(-16) busy, Err(-19) gone.
    pub fn bt_poll(pipe: u64, buf: &mut [u8]) -> Result<(usize, u32), i32> {
        let mut f = 0u32;
        let r = unsafe { sys::kapi_usb_bt_poll(pipe, buf.as_mut_ptr(), buf.len(), &mut f) };
        if r < 0 { if f != 0 && r == -11 { Ok((0, f)) } else { Err(r as i32) } } else { Ok((r as usize, f)) }
    }
    /// kind 0 = HCI command (EP0), 1 = ACL (bulk OUT); no H4 type byte. Sleeps.
    pub fn bt_send(pipe: u64, kind: u32, data: &[u8]) -> Result<(), i32> {
        match unsafe { sys::kapi_usb_bt_send(pipe, kind, data.as_ptr(), data.len()) } { 0 => Ok(()), e => Err(e) }
    }
    pub fn bt_close(pipe: u64) { unsafe { sys::kapi_usb_bt_close(pipe) } }
}

/// Bluetooth HCI transport registration (/dev/hci0).
pub mod hci {
    use super::*;
    pub fn register() -> Result<u64, i32> {
        let r = unsafe { sys::kapi_hci_register(owner()) };
        if r < 0 { Err(r as i32) } else { Ok(r as u64) }
    }
    /// Queue one H4 packet (type byte first) for btd.
    pub fn rx(h: u64, pkt: &[u8]) -> bool { unsafe { sys::kapi_hci_rx(h, pkt.as_ptr(), pkt.len()) == 0 } }
    /// Next H4 packet btd wrote, or None.
    pub fn tx_pop(h: u64, buf: &mut [u8]) -> Option<usize> {
        let r = unsafe { sys::kapi_hci_tx_pop(h, buf.as_mut_ptr(), buf.len()) };
        if r > 0 { Some(r as usize) } else { None }
    }
    pub fn notify(h: u64) { unsafe { sys::kapi_hci_ctl(h, 0); } }
    pub fn set_dead(h: u64) { unsafe { sys::kapi_hci_ctl(h, 1); } }
    pub fn is_open(h: u64) -> bool { unsafe { sys::kapi_hci_ctl(h, 2) > 0 } }
    pub fn rx_drops(h: u64) -> u64 { unsafe { sys::kapi_hci_ctl(h, 3).max(0) as u64 } }
    pub fn unregister(h: u64) { unsafe { sys::kapi_hci_unregister(h) } }
}
