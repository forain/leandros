//! Raspberry Pi 5 USB: the two DWC3 controllers inside RP1 (usb@200000 and
//! usb@300000 in RP1's BAR1), put into xHCI host mode and handed to the
//! common xHCI stack (`drivers/usb`).
//!
//! Two steps, because RP1 is probed (kernel init, `pcie.rpi5=1`) before the
//! USB layer comes up:
//! - [`attach`] (from `rp1::probe`): VBUS on (RP1 GPIO 42 = USB_VBUS_EN), then
//!   per controller GSNPSID check, core + PHY soft reset, GCTL host, the DTB
//!   `snps,*` quirks (`dwc3_hw::RP1_QUIRKS`); a controller that passes is
//!   queued.
//! - [`probe_pending`] (from `usb_hcd::init`): sets the xHCI DMA offset to
//!   RP1's inbound window (device address = CPU physical + 0x10_0000_0000)
//!   and runs `usb::probe_xhci` on each queued controller.
//!
//! The xHCI stack polls. With `rp1.irq=1` each controller's interrupter 0 is
//! also wired to its RP1 source over MSI-X (`rp1::request_irq`); the handler
//! only acknowledges and counts, so a missing interrupt costs nothing — the
//! `[rp1usb] ... irqs N` line shows whether the path works.
//!
//! Cache maintenance (non-coherent BCM2712 PCIe): `usb::dma::DmaBuf` maps
//! rings/contexts/DCBAA Non-cacheable and bulk bounce buffers are synced by
//! the stack (see `drivers/usb/src/dma.rs`).

use crate::brcmstb_hw as hw;
use crate::dwc3_hw as d;
use crate::irq::IrqReturn;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

const N: usize = d::RP1_USB.len();
/// DWC3 base VA of each controller that passed the glue setup (0 = none).
static PENDING: [AtomicUsize; N] = [const { AtomicUsize::new(0) }; N];
static IRQS: [AtomicU32; N] = [const { AtomicU32::new(0) }; N];

fn log(s: &str) { crate::pci::serial_debug(s) }
fn hex(v: u64) { crate::pci::serial_debug_hex_64(v) }

fn r(a: usize) -> u32 { unsafe { leandros_lib::mmio::read(a as *const u32) } }
fn w(a: usize, v: u32) { unsafe { leandros_lib::mmio::write(a as *mut u32, v) } }

/// Called once from `rp1::probe` with RP1's mapped BAR1. Inert on failure.
pub fn attach(bar1: usize, bar1_size: u64) {
    if bar1_size < (d::RP1_USB[N - 1].1 + d::RP1_USB_SIZE) as u64 {
        log("[rp1usb] FAIL bar1 too small\n");
        return;
    }
    vbus_on(bar1);
    for (i, &(name, off, _)) in d::RP1_USB.iter().enumerate() {
        if let Some(base) = glue(name, bar1 + off) { PENDING[i].store(base, Ordering::Release); }
    }
}

/// RP1 GPIO 42 (USB_VBUS_EN) as a plain output, high: port power on.
fn vbus_on(_bar1: usize) {
    if crate::rp1_gpio::output(d::RP1_USB_VBUS_GPIO, true) { log("[rp1usb] vbus on (gpio42)\n"); }
    else { log("[rp1usb] FAIL vbus gpio42\n"); }
}

/// DWC3 global setup for host mode. Returns the base on success.
fn glue(name: &str, base: usize) -> Option<usize> {
    let id = r(base + d::GSNPSID);
    log("[rp1usb] ");
    log(name);
    log(" snpsid=");
    hex(id as u64);
    if d::ip_of(id).is_none() {
        log(" FAIL not dwc3\n");
        return None;
    }
    let q = &d::RP1_QUIRKS;
    // Core + PHY soft reset (the xHCI HCRST in probe resets the host side again).
    let (g, u2, u3) = d::reset_assert(r(base + d::GCTL), r(base + d::GUSB2PHYCFG0), r(base + d::GUSB3PIPECTL0));
    w(base + d::GCTL, g);
    w(base + d::GUSB3PIPECTL0, u3);
    w(base + d::GUSB2PHYCFG0, u2);
    sched::kthread::msleep(100);
    let (u2, u3) = d::phy_release(r(base + d::GUSB2PHYCFG0), r(base + d::GUSB3PIPECTL0));
    w(base + d::GUSB3PIPECTL0, d::gusb3pipectl(u3, q));
    w(base + d::GUSB2PHYCFG0, u2);
    sched::kthread::msleep(100);
    w(base + d::GCTL, d::gctl_host(r(base + d::GCTL)));
    w(base + d::GSBUSCFG1, d::gsbuscfg1(r(base + d::GSBUSCFG1), q));
    w(base + d::GTXTHRCFG, d::gtxthrcfg(r(base + d::GTXTHRCFG), q));
    w(base + d::GUCTL1, d::guctl1(r(base + d::GUCTL1), q));
    sched::kthread::msleep(10);
    let sts = r(base + d::GSTS);
    log(" gctl=");
    hex(r(base + d::GCTL) as u64);
    log(" gsts=");
    hex(sts as u64);
    log(" gfladj=");
    hex(r(base + d::GFLADJ) as u64);
    if sts & d::GSTS_CURMOD_MASK != d::GSTS_CURMOD_HOST {
        log(" FAIL not in host mode\n");
        return None;
    }
    log(" host\n");
    Some(base)
}

/// Called from `usb_hcd::init`: true if any RP1 controller is queued.
pub fn any_pending() -> bool { PENDING.iter().any(|p| p.load(Ordering::Acquire) != 0) }

/// Brings up every queued controller on the common xHCI stack.
pub fn probe_pending() {
    if !any_pending() { return; }
    usb::platform::set_dma_offset(crate::rp1::dma_addr(0));
    let irq = crate::rp1::GEM_IRQ.load(Ordering::Relaxed);
    for (i, &(name, _, src)) in d::RP1_USB.iter().enumerate() {
        let base = PENDING[i].swap(0, Ordering::AcqRel);
        if base == 0 { continue; }
        crate::usb_hcd::register_platform_controller(1, 0, 0, hw::RP1_VENDOR, base);
        let res = unsafe { usb::probe_xhci(base, d::XHCI_LEN) };
        log("[rp1usb] ");
        log(name);
        match res {
            Ok(n) => {
                log(" xhci up, devices=");
                hex(n as u64);
                let wired = irq && wire_irq(i, base, src);
                log(if wired { " irq msix\n" } else { " irq none (polled)\n" });
            }
            Err(e) => { log(" FAIL xhci: "); log(e); log("\n"); }
        }
    }
}

/// xHCI register helpers on the raw base (cap → op/runtime offsets).
fn op_base(base: usize) -> usize { base + (r(base) & 0xff) as usize }
fn ir0(base: usize) -> usize { base + (r(base + 0x18) & !0x1f) as usize + 0x20 }
const USBCMD_INTE: u32 = 1 << 2;
const USBSTS_EINT: u32 = 1 << 3;
const IMAN_IP: u32 = 1 << 0;
const IMAN_IE: u32 = 1 << 1;

extern "C" fn usb_isr(ctx: usize) -> IrqReturn {
    let i = ctx & 0xf;
    let base = ctx & !0xf;
    if IRQS[i].fetch_add(1, Ordering::Relaxed) == 0 {
        log(if i == 0 { "[rp1usb] usb0 first irq\n" } else { "[rp1usb] usb1 first irq\n" });
    }
    // Acknowledge only; the polled event-ring consumer does the work.
    w(op_base(base) + 4, USBSTS_EINT);
    let iman = ir0(base);
    w(iman, (r(iman) & IMAN_IE) | IMAN_IP);
    IrqReturn::Handled
}

fn wire_irq(i: usize, base: usize, src: u32) -> bool {
    if base & 0xf != 0 { return false; }
    if crate::rp1::request_irq(src, false, usb_isr, base | i).map(core::mem::forget).is_err() { return false; }
    let iman = ir0(base);
    w(iman, IMAN_IE | IMAN_IP);
    let op = op_base(base);
    w(op, r(op) | USBCMD_INTE);
    true
}

/// Interrupt counts per controller (for the boot summary / kiostat).
pub fn irq_counts() -> [u32; N] { core::array::from_fn(|i| IRQS[i].load(Ordering::Relaxed)) }
