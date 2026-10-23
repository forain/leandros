//! PCI bus discovery and device enumeration.

use alloc::vec::Vec;
use spin::Mutex;

/// PCI command register (offset 0x04) bit 10, Interrupt Disable: the function
/// must not assert its INTx pin. Set by every driver that polls its virtio
/// device (blk, net, snd, input), because a device whose ISR status is never
/// read holds INTx asserted for good, and on QEMU virt's four shared PCIe
/// lines that would storm the GPU's completion interrupt.
pub const PCI_CMD_INTX_DISABLE: u16 = 1 << 10;

#[derive(Debug, Clone, Copy)]
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

#[cfg(target_arch = "x86_64")]
unsafe fn pci_read_config(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    let address = ((bus as u32) << 16) | ((dev as u32) << 11) |
                  ((func as u32) << 8) | ((offset as u32) & 0xFC) | 0x8000_0000;
    let mut val: u32;
    core::arch::asm!(
        "mov dx, 0xCF8",
        "out dx, eax",
        "mov dx, 0xCFC",
        "in eax, dx",
        inout("eax") address => val,
        out("dx") _,
        options(nomem, nostack)
    );
    val
}

static mut PCI_ECAM_BASE: usize = 0;

/// Virtual address of the ECAM window (aarch64), 0 if none. x86_64 uses legacy
/// 0xCF8 access and finds its ECAM through ACPI MCFG instead (see nvidia_probe).
pub fn ecam_va() -> usize { unsafe { PCI_ECAM_BASE } }

pub fn init_pci(ecam_base: usize) {
    unsafe { PCI_ECAM_BASE = ecam_base; }
}

#[cfg(target_arch = "aarch64")]
unsafe fn pci_read_config(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    let ecam_base = PCI_ECAM_BASE;
    if ecam_base == 0 { return 0xFFFF_FFFF; }
    
    let addr = ecam_base + 
               ((bus as usize) << 20) + 
               ((dev as usize) << 15) + 
               ((func as usize) << 12) + 
               (offset as usize & 0xFC);
               
    core::ptr::read_volatile(addr as *const u32)
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
unsafe fn pci_read_config(_bus: u8, _dev: u8, _func: u8, _offset: u8) -> u32 {
    0xFFFF_FFFF
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub fn serial_debug(msg: &str) {
    extern "C" { fn arch_serial_putc(c: u8); }
    for &b in msg.as_bytes() {
        unsafe { arch_serial_putc(b); }
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub fn serial_debug(_msg: &str) {}

fn hex_digit(v: u8) -> u8 {
    if v < 10 { b'0' + v } else { b'A' + v - 10 }
}

pub fn serial_debug_hex(v: u32) {
    let mut buf = [0u8; 10];
    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..8 {
        buf[9 - i] = hex_digit(((v >> (i * 4)) & 0xF) as u8);
    }
    serial_debug(unsafe { core::str::from_utf8_unchecked(&buf) });
}

pub fn serial_debug_dec(mut v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 { break; }
    }
    serial_debug(unsafe { core::str::from_utf8_unchecked(&buf[i..]) });
}

pub fn serial_debug_hex_64(v: u64) {
    serial_debug("0x");
    for i in (0..16).rev() {
        let digit = hex_digit(((v >> (i * 4)) & 0xF) as u8);
        serial_debug(unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(&digit, 1)) });
    }
}

/// Verbose per-frame/per-sample serial logging for the render and audio hot
/// paths (DRM, DRM-IF, DRM-SRV, KERN, FB, GPU, KMS, SND, PW). Off by default:
/// these call sites run on every frame or audio buffer, and the per-byte
/// serial port writes measurably slow rendering and sound. Flip to `true`
/// locally to re-enable while debugging those subsystems.
pub const RENDER_DEBUG: bool = false;

#[inline(always)]
pub fn rdebug(msg: &str) {
    if RENDER_DEBUG { serial_debug(msg); }
}

#[inline(always)]
pub fn rdebug_hex(v: u32) {
    if RENDER_DEBUG { serial_debug_hex(v); }
}

#[inline(always)]
pub fn rdebug_hex_64(v: u64) {
    if RENDER_DEBUG { serial_debug_hex_64(v); }
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub unsafe fn pci_read_config_8(bus: u8, dev: u8, func: u8, offset: u8) -> u8 {
    let val = pci_read_config(bus, dev, func, offset);
    ((val >> ((offset & 3) * 8)) & 0xFF) as u8
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub unsafe fn pci_read_config_16(bus: u8, dev: u8, func: u8, offset: u8) -> u16 {
    let val = pci_read_config(bus, dev, func, offset);
    ((val >> ((offset & 3) * 8)) & 0xFFFF) as u16
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub unsafe fn pci_read_config_32(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    pci_read_config(bus, dev, func, offset)
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub unsafe fn pci_read_config_32_any(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    let b0 = pci_read_config_8(bus, dev, func, offset) as u32;
    let b1 = pci_read_config_8(bus, dev, func, offset + 1) as u32;
    let b2 = pci_read_config_8(bus, dev, func, offset + 2) as u32;
    let b3 = pci_read_config_8(bus, dev, func, offset + 3) as u32;
    b0 | (b1 << 8) | (b2 << 16) | (b3 << 24)
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub unsafe fn pci_read_config_8(_bus: u8, _dev: u8, _func: u8, _offset: u8) -> u8 { 0 }
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub unsafe fn pci_read_config_16(_bus: u8, _dev: u8, _func: u8, _offset: u8) -> u16 { 0 }
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub unsafe fn pci_read_config_32(_bus: u8, _dev: u8, _func: u8, _offset: u8) -> u32 { 0 }
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub unsafe fn pci_read_config_32_any(_bus: u8, _dev: u8, _func: u8, _offset: u8) -> u32 { 0 }

#[cfg(target_arch = "x86_64")]
pub unsafe fn pci_write_config_16(bus: u8, dev: u8, func: u8, offset: u8, val: u16) {
    let address = ((bus as u32) << 16) | ((dev as u32) << 11) |
                  ((func as u32) << 8) | ((offset as u32) & 0xFC) | 0x8000_0000;
    
    let mut current = pci_read_config(bus, dev, func, offset & 0xFC);
    let shift = (offset & 3) * 8;
    current &= !(0xFFFF << shift);
    current |= (val as u32) << shift;

    core::arch::asm!(
        "mov dx, 0xCF8",
        "out dx, eax",
        "mov eax, edi",
        "mov dx, 0xCFC",
        "out dx, eax",
        // eax is overwritten with the data dword: declare it clobbered, or
        // LLVM reuses it as the address of the next config access (a BAR
        // sizing read-back then hit bus 255 and read all-ones).
        inout("eax") address => _,
        in("edi") current,
        out("dx") _,
        options(nomem, nostack)
    );
}

#[cfg(target_arch = "aarch64")]
pub unsafe fn pci_write_config_16(bus: u8, dev: u8, func: u8, offset: u8, val: u16) {
    let ecam_base = PCI_ECAM_BASE;
    if ecam_base == 0 { return; }
    
    let addr = ecam_base + 
               ((bus as usize) << 20) + 
               ((dev as usize) << 15) + 
               ((func as usize) << 12) + 
               (offset as usize & 0xFC);
               
    let mut current = core::ptr::read_volatile(addr as *const u32);
    let shift = (offset & 3) * 8;
    current &= !(0xFFFF << shift);
    current |= (val as u32) << shift;
    
    core::ptr::write_volatile(addr as *mut u32, current);
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub unsafe fn pci_write_config_16(_bus: u8, _dev: u8, _func: u8, _offset: u8, _val: u16) {}

#[cfg(target_arch = "x86_64")]
pub unsafe fn pci_write_config_32(bus: u8, dev: u8, func: u8, offset: u8, val: u32) {
    let address = ((bus as u32) << 16) | ((dev as u32) << 11) |
                  ((func as u32) << 8) | ((offset as u32) & 0xFC) | 0x8000_0000;
    core::arch::asm!(
        "mov dx, 0xCF8",
        "out dx, eax",
        "mov eax, edi",
        "mov dx, 0xCFC",
        "out dx, eax",
        // eax is overwritten with the data dword: declare it clobbered, or
        // LLVM reuses it as the address of the next config access (a BAR
        // sizing read-back then hit bus 255 and read all-ones).
        inout("eax") address => _,
        in("edi") val,
        out("dx") _,
        options(nomem, nostack)
    );
}

#[cfg(target_arch = "aarch64")]
pub unsafe fn pci_write_config_32(bus: u8, dev: u8, func: u8, offset: u8, val: u32) {
    let ecam_base = PCI_ECAM_BASE;
    if ecam_base == 0 { return; }
    let addr = ecam_base +
               ((bus as usize) << 20) +
               ((dev as usize) << 15) +
               ((func as usize) << 12) +
               (offset as usize & 0xFC);
    core::ptr::write_volatile(addr as *mut u32, val);
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub unsafe fn pci_write_config_32(_bus: u8, _dev: u8, _func: u8, _offset: u8, _val: u32) {}

impl PciDevice {
    pub unsafe fn find_capability(&self, cap_id: u8) -> Option<u8> {
        let status = pci_read_config_16(self.bus, self.dev, self.func, 0x06);
        if (status & (1 << 4)) == 0 { return None; }
        let mut cap_ptr = pci_read_config_8(self.bus, self.dev, self.func, 0x34);
        while cap_ptr != 0 {
            let id = pci_read_config_8(self.bus, self.dev, self.func, cap_ptr);
            if id == cap_id { return Some(cap_ptr); }
            cap_ptr = pci_read_config_8(self.bus, self.dev, self.func, cap_ptr + 1);
        }
        None
    }
}

static PCI_DEVICES: Mutex<Option<Vec<PciDevice>>> = Mutex::new(None);

pub fn scan() -> Vec<PciDevice> {
    {
        let lock = PCI_DEVICES.lock();
        if let Some(ref devices) = *lock {
            return devices.clone();
        }
    }

    let mut devices = Vec::new();
    serial_debug("[PCI] Scanning bus...\n");

    // On a bare `-kernel` boot there is no firmware to assign PCI BARs, so we
    // assign them ourselves from QEMU virt's 32-bit PCIe MMIO window. BARs that
    // already hold an address (UEFI/x86, where firmware programmed them) are
    // left untouched, so this is a no-op on the firmware-assisted paths.
    #[cfg(target_arch = "aarch64")]
    let mut mmio_next: usize = 0x1000_0000;
    #[cfg(target_arch = "aarch64")]
    const PCIE_MMIO_END: usize = 0x3eff_0000;

    let max_bus = if cfg!(target_arch = "aarch64") { 0 } else { 255 };

    for bus in 0..=max_bus {
        for dev in 0..32 {
            let val = unsafe { pci_read_config(bus as u8, dev as u8, 0, 0) };
            if val == 0xFFFF_FFFF { continue; }

            let header_type = unsafe { pci_read_config_8(bus as u8, dev as u8, 0, 0x0E) };
            let max_func = if header_type & 0x80 != 0 { 8 } else { 1 };

            for func in 0..max_func {
                let val = unsafe { pci_read_config(bus as u8, dev as u8, func as u8, 0) };
                if val == 0xFFFF_FFFF { continue; }
                let vendor_id = (val & 0xFFFF) as u16;
                let device_id = (val >> 16) as u16;
                serial_debug("[PCI] Found dev ");
                serial_debug_hex(vendor_id as u32);
                serial_debug(":");
                serial_debug_hex(device_id as u32);
                serial_debug("\n");

                let class_rev = unsafe { pci_read_config(bus as u8, dev as u8, func as u8, 0x08) };
                let class = (class_rev >> 24) as u8;
                let subclass = (class_rev >> 16) as u8;
                let prog_if = (class_rev >> 8) as u8;

                let mut bars = [0u32; 6];
                let mut i = 0usize;
                while i < 6 {
                    let off = 0x10 + (i as u8 * 4);
                    let orig = unsafe { pci_read_config(bus as u8, dev as u8, func as u8, off) };

                    // I/O-space BARs (bit 0 set): record and move on.
                    if orig & 1 == 1 {
                        bars[i] = orig;
                        i += 1;
                        continue;
                    }

                    let is_64 = (orig & 0x6) == 0x4; // memory BAR type bits == 10b
                    let already_assigned = (orig & 0xFFFF_FFF0) != 0
                        || (is_64 && unsafe { pci_read_config(bus as u8, dev as u8, func as u8, off + 4) } != 0);

                    // Direct boot: size and assign any unprogrammed memory BAR.
                    #[cfg(target_arch = "aarch64")]
                    if !already_assigned {
                        unsafe {
                            pci_write_config_32(bus as u8, dev as u8, func as u8, off, 0xFFFF_FFFF);
                            let sz_lo = pci_read_config(bus as u8, dev as u8, func as u8, off) & 0xFFFF_FFF0;
                            let sz_hi = if is_64 {
                                pci_write_config_32(bus as u8, dev as u8, func as u8, off + 4, 0xFFFF_FFFF);
                                pci_read_config(bus as u8, dev as u8, func as u8, off + 4)
                            } else { 0xFFFF_FFFF };
                            let mask = ((sz_hi as u64) << 32) | (sz_lo as u64);
                            let size = (!mask).wrapping_add(1) as usize;
                            if size != 0 && mmio_next + size <= PCIE_MMIO_END {
                                let base = leandros_lib::align_up(mmio_next, size);
                                pci_write_config_32(bus as u8, dev as u8, func as u8, off, base as u32);
                                if is_64 {
                                    pci_write_config_32(bus as u8, dev as u8, func as u8, off + 4, (base >> 32) as u32);
                                }
                                mmio_next = base + size;
                            }
                        }
                    }

                    bars[i] = unsafe { pci_read_config(bus as u8, dev as u8, func as u8, off) };
                    serial_debug("  BAR");
                    serial_debug_hex(i as u32);
                    serial_debug("=");
                    serial_debug_hex(bars[i]);
                    serial_debug("\n");

                    if is_64 && i + 1 < 6 {
                        bars[i + 1] = unsafe { pci_read_config(bus as u8, dev as u8, func as u8, off + 4) };
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                devices.push(PciDevice {
                    bus: bus as u8, dev: dev as u8, func: func as u8,
                    vendor_id, device_id, class, subclass, prog_if, bars,
                });
            }
        }
    }
    *PCI_DEVICES.lock() = Some(devices.clone());
    devices
}

pub fn find_device(vendor_id: u16, device_id: u16) -> Option<PciDevice> {
    scan().into_iter().find(|d| d.vendor_id == vendor_id && d.device_id == device_id)
}

pub fn find_all_devices(vendor_id: u16, device_id: u16) -> Vec<PciDevice> {
    scan().into_iter().filter(|d| d.vendor_id == vendor_id && d.device_id == device_id).collect()
}

pub fn find_by_class(class: u8, subclass: u8, prog_if: u8) -> Vec<PciDevice> {
    scan().into_iter()
        .filter(|d| d.class == class && d.subclass == subclass && d.prog_if == prog_if)
        .collect()
}

// ════════════════════════════════════════════════════════════════════════════
// Generic PCI function helpers (lane kfacil): full 4 KiB config space through
// ECAM with a legacy fallback, capability walkers, BAR sizing, command bits.
// ════════════════════════════════════════════════════════════════════════════

/// Serialises legacy 0xCF8/0xCFC accesses made through [`PciFunction`]: the
/// address/data pair is two port writes and must not interleave across CPUs.
/// IRQ-safe (an interrupt handler may read config space).
#[cfg(target_arch = "x86_64")]
static LEGACY_CFG: sched::ksync::KSpinLock<()> = sched::ksync::KSpinLock::new(());

/// Physical ECAM base from ACPI MCFG (x86_64), 0 = none.
#[cfg(target_arch = "x86_64")]
static ECAM_PHYS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Mapped ECAM pages (x86_64): (bdf, va), mapped on first use, never unmapped.
#[cfg(target_arch = "x86_64")]
static ECAM_PAGES: Mutex<Vec<(u16, usize)>> = Mutex::new(Vec::new());

/// Record the ACPI MCFG ECAM window (x86_64; called once from kernel init).
/// aarch64 already has its ECAM mapped (`init_pci`).
pub fn init_ecam_from_acpi(rsdp_phys: u64, hhdm: u64) {
    #[cfg(target_arch = "x86_64")]
    {
        let base = unsafe { boot::acpi::find_ecam_base(rsdp_phys, hhdm) };
        ECAM_PHYS.store(base, core::sync::atomic::Ordering::Release);
    }
    #[cfg(not(target_arch = "x86_64"))]
    { let _ = (rsdp_phys, hhdm); }
}

/// A PCI function address with config-space accessors over all 4 KiB.
///
/// Offsets >= 0x100 need ECAM (x86_64: ACPI MCFG, recorded by
/// `init_ecam_from_acpi`; aarch64: the DTB/Limine ECAM). Without it they read
/// as all-ones and writes are dropped. Each access is a single sized access
/// (no read-modify-write of neighbouring bytes), so RW1C status bits next to
/// the written field are never clobbered. Any context, IRQ handlers included;
/// on x86_64 the first ECAM access to a function maps its page, which needs
/// task context (in IRQ context an unmapped page reads as all-ones), so touch
/// extended space once from task context first (`has_extended_config`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciFunction {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
}

/// One decoded BAR.
#[derive(Debug, Clone, Copy)]
pub struct BarInfo {
    pub index: usize,
    pub io: bool,
    pub is64: bool,
    pub prefetchable: bool,
    /// Bus address (I/O port number for I/O BARs). 0 = unassigned.
    pub base: u64,
    /// Size in bytes (0 when decoded without sizing).
    pub size: u64,
}

/// Result of [`PciFunction::size_bars`].
#[derive(Debug, Clone)]
pub struct BarSizing {
    pub bars: Vec<BarInfo>,
    /// Expansion ROM BAR as found (restored).
    pub rom_orig: u32,
    /// Expansion ROM size (0 = none).
    pub rom_size: u64,
}

/// Capability ids.
pub mod cap {
    pub const PM: u8 = 0x01;
    pub const MSI: u8 = 0x05;
    pub const VENDOR: u8 = 0x09;
    pub const PCIE: u8 = 0x10;
    pub const MSIX: u8 = 0x11;
    /// Extended capability ids.
    pub const EXT_AER: u16 = 0x0001;
    pub const EXT_REBAR: u16 = 0x0015;
}

pub const CMD_IO: u16 = 1 << 0;
pub const CMD_MEMORY: u16 = 1 << 1;
pub const CMD_BUS_MASTER: u16 = 1 << 2;

impl PciDevice {
    /// The function address of a scanned device.
    pub fn function(&self) -> PciFunction { PciFunction { bus: self.bus, dev: self.dev, func: self.func } }
}

impl PciFunction {
    pub const fn new(bus: u8, dev: u8, func: u8) -> Self { Self { bus, dev, func } }

    #[inline]
    fn bdf(&self) -> u16 { ((self.bus as u16) << 8) | ((self.dev as u16) << 3) | self.func as u16 }

    /// Virtual address of this function's 4 KiB ECAM page, mapping it if
    /// needed (x86_64: task context for the first call). 0 = no ECAM.
    pub fn ecam_page(&self) -> usize {
        #[cfg(target_arch = "x86_64")]
        {
            let base = ECAM_PHYS.load(core::sync::atomic::Ordering::Acquire);
            if base == 0 { return 0; }
            let cached = self.ecam_page_cached();
            if cached != 0 || sched::irqctx::in_interrupt() { return cached; }
            let key = self.bdf();
            let phys = base + ((key as u64) << 12);
            let va = match mm::kva::ioremap(phys, 4096, mm::kva::CacheMode::Uncached) { Some(v) => v, None => return 0 };
            let mut g = ECAM_PAGES.lock();
            if let Some(&(_, old)) = g.iter().find(|(k, _)| *k == key) {
                drop(g);
                mm::kva::iounmap(va);
                return old;
            }
            g.push((key, va));
            va
        }
        #[cfg(target_arch = "aarch64")]
        {
            let base = unsafe { PCI_ECAM_BASE };
            if base == 0 { return 0; }
            base + ((self.bdf() as usize) << 12)
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        { 0 }
    }

    /// Already-mapped ECAM page, never mapping (safe in IRQ context).
    #[cfg(target_arch = "x86_64")]
    fn ecam_page_cached(&self) -> usize {
        let key = self.bdf();
        ECAM_PAGES.try_lock().and_then(|g| g.iter().find(|(k, _)| *k == key).map(|&(_, v)| v)).unwrap_or(0)
    }

    /// True if offsets >= 0x100 are reachable.
    pub fn has_extended_config(&self) -> bool { self.ecam_page() != 0 }

    /// Legacy 0xCF8/0xCFC access; `size` = 1/2/4, `write` = Some(value).
    #[cfg(target_arch = "x86_64")]
    fn legacy(&self, off: u16, size: u8, write: Option<u32>) -> u32 {
        let addr = 0x8000_0000u32 | ((self.bus as u32) << 16) | ((self.dev as u32) << 11)
            | ((self.func as u32) << 8) | (off as u32 & 0xFC);
        let port = 0xCFCu16 + (off & 3);
        let _g = LEGACY_CFG.lock();
        unsafe {
            core::arch::asm!("out dx, eax", in("dx") 0xCF8u16, in("eax") addr, options(nomem, nostack));
            match (size, write) {
                (1, None) => { let v: u8; core::arch::asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack)); v as u32 }
                (2, None) => { let v: u16; core::arch::asm!("in ax, dx", out("ax") v, in("dx") port, options(nomem, nostack)); v as u32 }
                (_, None) => { let v: u32; core::arch::asm!("in eax, dx", out("eax") v, in("dx") 0xCFCu16, options(nomem, nostack)); v }
                (1, Some(w)) => { core::arch::asm!("out dx, al", in("dx") port, in("al") w as u8, options(nomem, nostack)); 0 }
                (2, Some(w)) => { core::arch::asm!("out dx, ax", in("dx") port, in("ax") w as u16, options(nomem, nostack)); 0 }
                (_, Some(w)) => { core::arch::asm!("out dx, eax", in("dx") 0xCFCu16, in("eax") w, options(nomem, nostack)); 0 }
            }
        }
    }

    fn access(&self, off: u16, size: u8, write: Option<u32>) -> u32 {
        let ones = match size { 1 => 0xFF, 2 => 0xFFFF, _ => 0xFFFF_FFFF };
        if off >= 0x1000 || (off as u32) % size as u32 != 0 { return ones; }
        // x86_64: legacy for the first 256 bytes (always present; the path
        // every existing driver uses), ECAM above. aarch64: ECAM always.
        #[cfg(target_arch = "x86_64")]
        if off < 0x100 { return self.legacy(off, size, write); }
        let page = self.ecam_page();
        if page == 0 { return ones; }
        let a = page + off as usize;
        unsafe {
            match (size, write) {
                (1, None) => core::ptr::read_volatile(a as *const u8) as u32,
                (2, None) => core::ptr::read_volatile(a as *const u16) as u32,
                (_, None) => core::ptr::read_volatile(a as *const u32),
                (1, Some(w)) => { core::ptr::write_volatile(a as *mut u8, w as u8); 0 }
                (2, Some(w)) => { core::ptr::write_volatile(a as *mut u16, w as u16); 0 }
                (_, Some(w)) => { core::ptr::write_volatile(a as *mut u32, w); 0 }
            }
        }
    }

    pub fn read8(&self, off: u16) -> u8 { self.access(off, 1, None) as u8 }
    pub fn read16(&self, off: u16) -> u16 { self.access(off, 2, None) as u16 }
    pub fn read32(&self, off: u16) -> u32 { self.access(off, 4, None) }
    pub fn write8(&self, off: u16, v: u8) { self.access(off, 1, Some(v as u32)); }
    pub fn write16(&self, off: u16, v: u16) { self.access(off, 2, Some(v as u32)); }
    pub fn write32(&self, off: u16, v: u32) { self.access(off, 4, Some(v)); }

    pub fn vendor_id(&self) -> u16 { self.read16(0x00) }
    pub fn device_id(&self) -> u16 { self.read16(0x02) }
    pub fn command(&self) -> u16 { self.read16(0x04) }
    pub fn set_command(&self, v: u16) { self.write16(0x04, v) }

    /// Set memory-space decode.
    pub fn enable_memory(&self) { let c = self.command(); if c & CMD_MEMORY == 0 { self.set_command(c | CMD_MEMORY); } }
    /// Set I/O-space decode.
    pub fn enable_io(&self) { let c = self.command(); if c & CMD_IO == 0 { self.set_command(c | CMD_IO); } }
    /// Set (or clear) bus mastering (needed for DMA and for MSI writes).
    pub fn set_bus_master(&self, on: bool) {
        let c = self.command();
        let n = if on { c | CMD_BUS_MASTER } else { c & !CMD_BUS_MASTER };
        if n != c { self.set_command(n); }
    }
    /// Memory decode + bus master in one write.
    pub fn enable_mem_and_bus_master(&self) {
        let c = self.command();
        let n = c | CMD_MEMORY | CMD_BUS_MASTER;
        if n != c { self.set_command(n); }
    }
    /// Command bit 10: forbid (true) or allow (false) INTx assertion.
    pub fn set_intx_disable(&self, disable: bool) {
        let c = self.command();
        let n = if disable { c | PCI_CMD_INTX_DISABLE } else { c & !PCI_CMD_INTX_DISABLE };
        if n != c { self.set_command(n); }
    }

    /// Every standard capability as `(id, offset)`, in list order.
    pub fn capabilities(&self) -> Vec<(u8, u16)> {
        let mut out = Vec::new();
        if self.read16(0x06) & (1 << 4) == 0 { return out; }
        let mut p = (self.read8(0x34) & !3) as u16;
        let mut guard = 0;
        while p >= 0x40 && guard < 48 {
            guard += 1;
            out.push((self.read8(p), p));
            p = (self.read8(p + 1) & !3) as u16;
        }
        out
    }

    /// Offset of the first standard capability with `id`. Allocation-free
    /// (IRQ-safe).
    pub fn find_capability(&self, id: u8) -> Option<u16> {
        if self.read16(0x06) & (1 << 4) == 0 { return None; }
        let mut p = (self.read8(0x34) & !3) as u16;
        let mut guard = 0;
        while p >= 0x40 && guard < 48 {
            guard += 1;
            if self.read8(p) == id { return Some(p); }
            p = (self.read8(p + 1) & !3) as u16;
        }
        None
    }

    /// Every extended capability (offset >= 0x100) as `(id, version, offset)`.
    /// Empty without ECAM.
    pub fn ext_capabilities(&self) -> Vec<(u16, u8, u16)> {
        let mut out = Vec::new();
        if !self.has_extended_config() { return out; }
        let mut p: u16 = 0x100;
        let mut guard = 0;
        while p >= 0x100 && p < 0x1000 && guard < 128 {
            guard += 1;
            let h = self.read32(p);
            if h == 0 || h == 0xFFFF_FFFF { break; }
            out.push(((h & 0xFFFF) as u16, ((h >> 16) & 0xF) as u8, p));
            p = ((h >> 20) & 0xFFC) as u16;
        }
        out
    }

    /// Offset of the first extended capability with `id`.
    pub fn find_ext_capability(&self, id: u16) -> Option<u16> {
        if !self.has_extended_config() { return None; }
        let mut p: u16 = 0x100;
        let mut guard = 0;
        while p >= 0x100 && p < 0x1000 && guard < 128 {
            guard += 1;
            let h = self.read32(p);
            if h == 0 || h == 0xFFFF_FFFF { break; }
            if (h & 0xFFFF) as u16 == id { return Some(p); }
            p = ((h >> 20) & 0xFFC) as u16;
        }
        None
    }

    /// Decode BAR `i` from its current value (no sizing; `size` = 0).
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

    /// Size every BAR and the expansion ROM BAR the safe way: local IRQs
    /// masked, memory + I/O decode off for the duration, every register and
    /// then COMMAND restored bit-for-bit (Linux's procedure; this is the code
    /// `nvidia_probe` shipped with, factored out). Nothing is printed inside
    /// the window. Only the first 256 bytes are touched.
    pub fn size_bars(&self) -> BarSizing {
        let (bus, dev, func) = (self.bus, self.dev, self.func);
        let rd = |off: u8| unsafe { pci_read_config_32(bus, dev, func, off) };
        let wr = |off: u8, v: u32| unsafe { pci_write_config_32(bus, dev, func, off, v) };
        let mut orig = [0u32; 7];
        let mut sz = [0u32; 7];
        let mut kind64 = [false; 6];
        let irq = sched::ksync::irq_save();
        let cmd = unsafe { pci_read_config_16(bus, dev, func, 0x04) };
        unsafe { pci_write_config_16(bus, dev, func, 0x04, cmd & !0x3); }
        let mut i = 0;
        while i < 6 {
            let off = 0x10 + 4 * i as u8;
            let o = rd(off);
            wr(off, 0xFFFF_FFFF);
            let v = rd(off);
            wr(off, o);
            orig[i] = o;
            sz[i] = v;
            if o & 1 == 0 && (o & 6) == 4 && i + 1 < 6 {
                kind64[i] = true;
                let o2 = rd(off + 4);
                wr(off + 4, 0xFFFF_FFFF);
                let v2 = rd(off + 4);
                wr(off + 4, o2);
                orig[i + 1] = o2;
                sz[i + 1] = v2;
                i += 2;
            } else {
                i += 1;
            }
        }
        // Expansion ROM BAR: size with the enable bit clear.
        let rom_orig = rd(0x30);
        wr(0x30, 0xFFFF_F800);
        let rom_sz = rd(0x30) & 0xFFFF_F800;
        wr(0x30, rom_orig);
        unsafe { pci_write_config_16(bus, dev, func, 0x04, cmd); }
        sched::ksync::irq_restore(irq);

        let mut bars = Vec::new();
        let mut i = 0;
        while i < 6 {
            let (o, v) = (orig[i], sz[i]);
            if v == 0 || (v == 0xFFFF_FFFF && o == 0xFFFF_FFFF) { i += 1; continue; }
            if o & 1 == 1 {
                let size = (!(v & !3)).wrapping_add(1) & 0xFFFF;
                bars.push(BarInfo { index: i, io: true, is64: false, prefetchable: false, base: (o & !3) as u64, size: size as u64 });
                i += 1;
            } else if kind64[i] {
                let mask = ((sz[i + 1] as u64) << 32) | (v & !0xF) as u64;
                let size = (!mask).wrapping_add(1);
                let base = ((orig[i + 1] as u64) << 32) | (o & !0xF) as u64;
                bars.push(BarInfo { index: i, io: false, is64: true, prefetchable: o & 8 != 0, base, size });
                i += 2;
            } else {
                let size = (!(v & !0xF)).wrapping_add(1) as u64;
                bars.push(BarInfo { index: i, io: false, is64: false, prefetchable: o & 8 != 0, base: (o & !0xF) as u64, size });
                i += 1;
            }
        }
        let rom_size = if rom_sz == 0 { 0 } else { (!rom_sz).wrapping_add(1) as u64 };
        BarSizing { bars, rom_orig, rom_size }
    }

    /// Size and map memory BAR `i` with cache attribute `mode` (KVA window).
    /// Returns `(virtual address, size)`. Task context.
    pub fn map_bar(&self, i: usize, mode: mm::kva::CacheMode) -> Option<(usize, u64)> {
        let s = self.size_bars();
        let b = s.bars.iter().find(|b| b.index == i && !b.io && b.base != 0 && b.size != 0)?;
        let va = mm::kva::ioremap(b.base, b.size as usize, mode)?;
        Some((va, b.size))
    }

    /// The GIC INTID this function's INTx pin is wired to on QEMU `virt`
    /// (PCIe host bridge INTA..D = SPIs 3..6, standard slot swizzle; same
    /// mapping virtio-gpu uses). x86_64: None (INTx routing needs ACPI _PRT;
    /// use MSI there).
    pub fn intx_line(&self) -> Option<u32> {
        let pin = self.read8(0x3D);
        if pin == 0 || pin > 4 { return None; }
        #[cfg(all(target_arch = "aarch64", not(any(feature = "rpi5", feature = "raspi4b"))))]
        { return Some(32 + 3 + ((self.dev as u32 + (pin as u32 - 1)) % 4)); }
        #[allow(unreachable_code)]
        None
    }
}

/// Byte write to config space at `offset` < 0x100 (a single byte access; no
/// read-modify-write of the neighbouring bytes).
pub unsafe fn pci_write_config_8(bus: u8, dev: u8, func: u8, offset: u8, val: u8) {
    PciFunction::new(bus, dev, func).write8(offset as u16, val)
}

/// First scanned function with this vendor/device id.
pub fn find_function(vendor_id: u16, device_id: u16) -> Option<PciFunction> {
    find_device(vendor_id, device_id).map(|d| d.function())
}
