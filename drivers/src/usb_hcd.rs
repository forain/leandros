//! USB host-controller bring-up (xHCI over PCI) and the enumerated-device
//! registry backing `lsusb`.
//!
//! `init()` finds every PCI function of class 0C/03/30 (xHCI), maps BAR0,
//! and hands it to the `usb` crate's driver, which resets the controller,
//! enumerates the root ports and configures any bulk-only mass-storage
//! interface. With no xHCI on the bus (every default QEMU configuration)
//! it returns after one lookup in the cached PCI scan.
//!
//! The July 2026 attempt to do this corrupted kernel memory some time after
//! bring-up. Root cause (2026-10-05, artifacts/notes/lane-xhci-2026-10-05.md):
//! the old `usb::xhci::Xhci` was a 573,568-byte value returned by value onto
//! a 128 KiB kernel stack with no guard page. The driver was rewritten; see
//! the safety model at the top of `drivers/usb/src/xhci/mod.rs`.
//!
//! Verbose tracing and the boot-time BOT self-test are enabled by the
//! `usb-debug` cargo feature (kernel feature of the same name), or at run
//! time with [`set_debug`] before `init()`.
//!
//! Transports for the mass-storage class driver: [`mass_storage_devices`].

use crate::pci::{self, PciDevice, pci_read_config_16, pci_read_config_32, pci_write_config_16, pci_write_config_32};

pub use usb::{mass_storage_count, mass_storage_devices, BulkTransport, UsbErr, XhciBulk};

/// Metadata for `lsusb`.
#[derive(Debug, Clone, Copy)]
pub struct UsbDevInfo {
    pub bus: u8,
    pub address: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub class: u8,
}

/// Turn on verbose xHCI tracing and the BOT self-test. Call before `init()`.
pub fn set_debug(on: bool) {
    usb::platform::set_verbose(on);
    usb::platform::set_selftest(on);
}

/// Called between polls of the controller.
///
/// With IRQs on (the kernel init task enumerating at boot) it yields, so other
/// tasks run while we wait; before the scheduler runs that returns at once.
///
/// With IRQs masked we are in a syscall — typically f2fs/VFS block I/O that
/// holds `spin::Mutex`es (all IRQ-off spinlocks in this kernel). Yielding there
/// parks those locks, and the next task to want one spins at IF=0 forever: a
/// whole-guest hang, seen on the first USB-root boot. So never switch: spin,
/// and when the caller opened a preempt-disabled section
/// (`usb_storage::IoWindow`), let the tick in every 256 polls (`irq_window`;
/// tick hooks and deadlines keep running, no context switch), exactly like
/// `virtio_gpu`'s `SpinWindow`.
fn relax() {
    use core::sync::atomic::{AtomicU32, Ordering};
    static POLLS: AtomicU32 = AtomicU32::new(0);
    if !sched::irqs_masked() {
        sched::yield_now("usb-poll");
    } else if sched::preempt_disabled() && POLLS.fetch_add(1, Ordering::Relaxed) & 0xFF == 0 {
        sched::irq_window();
    }
    core::hint::spin_loop();
}

pub fn init() {
    let ctrls = pci::find_by_class(0x0C, 0x03, 0x30);
    if ctrls.is_empty() {
        return;
    }
    usb::platform::install(usb::platform::Hooks {
        now_ns: sched::monotonic_ns,
        relax,
        log: pci::serial_debug,
    });
    if cfg!(feature = "usb-debug") {
        set_debug(true);
    }
    for pci in &ctrls {
        unsafe { probe_one(pci) };
    }
}

/// Size BAR `idx` (and its upper half if 64-bit) with decoding disabled.
unsafe fn bar_size(pci: &PciDevice, idx: usize, is64: bool) -> u64 {
    let off = 0x10 + 4 * idx as u8;
    let lo = pci_read_config_32(pci.bus, pci.dev, pci.func, off);
    pci_write_config_32(pci.bus, pci.dev, pci.func, off, 0xFFFF_FFFF);
    let sz_lo = pci_read_config_32(pci.bus, pci.dev, pci.func, off) & 0xFFFF_FFF0;
    pci_write_config_32(pci.bus, pci.dev, pci.func, off, lo);
    let sz_hi = if is64 {
        let hi = pci_read_config_32(pci.bus, pci.dev, pci.func, off + 4);
        pci_write_config_32(pci.bus, pci.dev, pci.func, off + 4, 0xFFFF_FFFF);
        let s = pci_read_config_32(pci.bus, pci.dev, pci.func, off + 4);
        pci_write_config_32(pci.bus, pci.dev, pci.func, off + 4, hi);
        s
    } else {
        0xFFFF_FFFF
    };
    let mask = ((sz_hi as u64) << 32) | sz_lo as u64;
    (!mask).wrapping_add(1)
}

/// Intel 7-/8-/9-series PCHs power up with their USB ports routed to the
/// EHCI controllers; until the OS flips them (Linux
/// `usb_enable_intel_xhci_ports`), devices plugged into those ports are
/// invisible to the xHCI. Enable SuperSpeed on every port the BIOS allows
/// (USB3_PSSEN ← USB3PRM) and hand every switchable USB2 port to the xHCI
/// (XUSB2PR ← USB2PRM).
unsafe fn intel_route_ports_to_xhci(pci: &PciDevice) {
    const SWITCHABLE: [u16; 5] = [0x1E31, 0x8C31, 0x9C31, 0x9CB1, 0x8CB1];
    if pci.vendor_id != 0x8086 || !SWITCHABLE.contains(&pci.device_id) {
        return;
    }
    let usb3_mask = pci_read_config_32(pci.bus, pci.dev, pci.func, 0xDC);
    pci_write_config_32(pci.bus, pci.dev, pci.func, 0xD8, usb3_mask);
    let usb2_mask = pci_read_config_32(pci.bus, pci.dev, pci.func, 0xD4);
    pci_write_config_32(pci.bus, pci.dev, pci.func, 0xD0, usb2_mask);
    usb::usb_log!("[XHCI] Intel PCH: routed ports to xHCI (USB3 {:#x}, USB2 {:#x})\n", usb3_mask, usb2_mask);
}

unsafe fn probe_one(pci: &PciDevice) {
    let raw = pci.bars[0];
    if raw & 1 != 0 {
        pci::serial_debug("[XHCI] BAR0 is I/O space, skipping controller\n");
        return;
    }
    let is64 = (raw >> 1) & 3 == 2;
    let phys = (raw & !0xF) as u64 | if is64 { (pci.bars[1] as u64) << 32 } else { 0 };
    if phys == 0 {
        pci::serial_debug("[XHCI] BAR0 unassigned, skipping controller\n");
        return;
    }

    let cmd = pci_read_config_16(pci.bus, pci.dev, pci.func, 0x04);
    // Memory/I-O decoding off while BAR0 is sized (bus mastering untouched:
    // a controller firmware left running is stopped by the driver's own
    // halt + reset). Then memory space + bus master on, INTx off — the
    // driver polls and registers no interrupt handler.
    pci_write_config_16(pci.bus, pci.dev, pci.func, 0x04, cmd & !0x3);
    let size = bar_size(pci, 0, is64);
    pci_write_config_16(pci.bus, pci.dev, pci.func, 0x04,
                        (cmd & !0x1) | 0x0006 | pci::PCI_CMD_INTX_DISABLE);
    if size < 0x1000 || size > 16 << 20 {
        usb::usb_log!("[XHCI] implausible BAR0 size {:#x} (BAR0 {:#x}), skipping controller\n", size, phys);
        return;
    }

    intel_route_ports_to_xhci(pci);

    let Some(virt) = mm::paging::map_kernel_device(
        phys as usize, size as usize,
        mm::paging::PageFlags::PRESENT | mm::paging::PageFlags::WRITABLE | mm::paging::PageFlags::MMIO,
    ) else {
        pci::serial_debug("[XHCI] could not map BAR0, skipping controller\n");
        return;
    };

    usb::usb_log!("[XHCI] {:02x}:{:02x}.{} {:04x}:{:04x} BAR0 {:#x} ({} KiB)\n",
                  pci.bus, pci.dev, pci.func, pci.vendor_id, pci.device_id, phys, size / 1024);
    match usb::probe_xhci(virt, size as usize) {
        Ok(n) => usb::usb_log!("[XHCI] {} device(s), {} mass-storage interface(s)\n", n, usb::mass_storage_count()),
        Err(e) => usb::usb_log!("[XHCI] controller bring-up failed: {}\n", e),
    }
}

pub fn device_count() -> usize {
    usb::devices().len()
}

pub fn device_info(index: usize) -> Option<UsbDevInfo> {
    let d = usb::devices().into_iter().nth(index)?;
    Some(UsbDevInfo {
        // `lsusb` prints bus + 1 (0-based, like the PCI-style syscalls).
        bus: d.bus.saturating_sub(1),
        address: d.address,
        vendor_id: d.vendor,
        product_id: d.product,
        class: d.class,
    })
}
