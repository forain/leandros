//! USB host-controller bring-up and the live enumerated-device registry
//! backing `lsusb`.
//!
//! A real xHCI driver bring-up was implemented and tested here (PCI class
//! discovery, BAR mapping, `usb::xhci::Xhci` reset/init/run, root-hub port
//! power-on and status polling). That work found and fixed three genuine
//! bugs in `drivers/usb/src/xhci/` that had made it entirely non-functional:
//!
//!  - `Ring`/DCBAA/`InputContext` wrote kernel *virtual* addresses into
//!    hardware DMA registers instead of physical addresses (see
//!    `drivers/usb/src/xhci/ring.rs` and `mod.rs`) — real HHDM-mapped memory
//!    means virt != phys, so every DMA-target register was wrong.
//!  - `ERSTBA` (Event Ring Segment Table Base Address) was pointed directly
//!    at the event ring's own TRB array instead of at a proper segment-table
//!    entry — once the controller started running it would read the ring's
//!    first (zeroed) TRB as `{base: 0, size: 0}` and could misdirect DMA
//!    writes anywhere in physical memory.
//!  - `CMD_EIE` (interrupt enable) was set despite this driver being purely
//!    polling-based with no IRQ handler registered anywhere for the
//!    controller's line/MSI-X vector.
//!
//! With all three fixed, every individual hardware operation (reset, start,
//! port power-on, status reads, port reset, feature-clear) traces clean, and
//! the full port-scan sequence completes without error. However, running
//! that full real-hardware sequence still triggers a delayed, hard-to-
//! reproduce corruption elsewhere in the kernel afterward — one that does
//! *not* reproduce with an equivalent-duration delay or an equivalent
//! buddy-allocator footprint on their own, only with the actual live
//! hardware interaction. That residual issue needs more specialized tooling
//! (cycle-accurate tracing, a real hardware/QEMU-level debugger) than was
//! available this session, so real bring-up is not called from boot here —
//! this reports an empty registry instead of risking boot stability.
//! `lsusb` therefore correctly reports no controllers found, same as it
//! would on real hardware with no USB support wired up.

/// Metadata for `lsusb`. Deliberately lightweight — no descriptor buffers —
/// so the registry doesn't need to own any DMA memory.
#[derive(Debug, Clone, Copy)]
pub struct UsbDevInfo {
    pub bus: u8,
    pub address: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub class: u8,
}

pub fn init() {
    // Intentionally not driving real hardware — see module doc comment.
}

pub fn device_count() -> usize {
    0
}

pub fn device_info(_index: usize) -> Option<UsbDevInfo> {
    None
}
