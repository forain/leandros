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
    /// `ROLE_*` bits below.
    pub role: u8,
    /// Root-hub port (1-based) and xHCI speed ID.
    pub port: u8,
    pub speed: u8,
}

/// `UsbDevInfo::role` bits (also the `SYS_USBDEV_INFO` byte 9 ABI).
pub const ROLE_KEYBOARD: u8 = 0x01;
pub const ROLE_MOUSE: u8 = 0x02;
pub const ROLE_MASS_STORAGE: u8 = 0x04;
pub const ROLE_HUB: u8 = 0x08;
/// Synthetic root-hub entry (one per controller), combined with ROLE_HUB.
pub const ROLE_ROOT_HUB: u8 = 0x40;
pub const ROLE_HID_UNSUPPORTED: u8 = 0x80;

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
///
/// Lane fsio: with IRQs masked but preemption ON and nothing held that
/// forbids sleeping (`irqctx::can_sleep`: the caller holds only sleeping
/// locks — `F2FS_MOUNTS`, the USB disk lock — see `usb_storage::IoWindow`),
/// a wait that has spun for `SPIN_BUDGET_NS` parks (1/8 of the time waited) between
/// polls. A read command on a USB 2.0 stick finishes inside the spin budget's
/// order of magnitude; a small write on a cheap stick can take hundreds of
/// milliseconds, and spinning through that kept a CPU busy per request.
fn relax() {
    use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    static POLLS: AtomicU32 = AtomicU32::new(0);
    /// Per CPU: when the current run of polls started, and the last poll.
    static STREAK: [AtomicU64; sched::MAX_CPUS] = [const { AtomicU64::new(0) }; sched::MAX_CPUS];
    static LAST: [AtomicU64; sched::MAX_CPUS] = [const { AtomicU64::new(0) }; sched::MAX_CPUS];
    const SPIN_BUDGET_NS: u64 = 300_000;
    /// A gap this long since the previous poll on this CPU starts a new wait.
    const NEW_WAIT_GAP_NS: u64 = 50_000;
    if !sched::irqs_masked() {
        sched::yield_now("usb-poll");
    } else if sched::preempt_disabled() {
        if POLLS.fetch_add(1, Ordering::Relaxed) & 0xFF == 0 {
            sched::irq_window();
        }
    } else if sched::current_pid() != 0 && sched::irqctx::can_sleep() {
        let cpu = (unsafe { sched::cpu_id() }).min(sched::MAX_CPUS - 1);
        let now = sched::monotonic_ns();
        if now.saturating_sub(LAST[cpu].load(Ordering::Relaxed)) > NEW_WAIT_GAP_NS {
            STREAK[cpu].store(now, Ordering::Relaxed);
        }
        LAST[cpu].store(now, Ordering::Relaxed);
        if now.saturating_sub(STREAK[cpu].load(Ordering::Relaxed)) > SPIN_BUDGET_NS {
            // Park for 1/8 of the time waited so far (100 us .. 2 ms): short
            // commands keep a fine completion granularity, a write that runs
            // for hundreds of milliseconds costs a few hundred wake-ups.
            let waited = now.saturating_sub(STREAK[cpu].load(Ordering::Relaxed));
            let park = (waited / 8).clamp(100_000, 2_000_000);
            crate::usb_storage::sleep_until(now + park);
            // Possibly another CPU now: continue the streak there.
            let c2 = (unsafe { sched::cpu_id() }).min(sched::MAX_CPUS - 1);
            if c2 != cpu { STREAK[c2].store(STREAK[cpu].load(Ordering::Relaxed), Ordering::Relaxed); }
            LAST[c2].store(sched::monotonic_ns(), Ordering::Relaxed);
            return;
        }
    }
    core::hint::spin_loop();
}
/// USB log lines go to the console (UART + framebuffer), like `[BLK]`: on a
/// real machine the screen is the only place the `[USB] ...` device lines
/// can be read. IRQ-safe (the kernel's staged console print).
fn console_log(s: &str) {
    extern "C" { fn serial_print(s: *const u8, len: usize); }
    unsafe { serial_print(s.as_ptr(), s.len()) };
}

pub fn init() {
    let ctrls = pci::find_by_class(0x0C, 0x03, 0x30);
    #[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
    let rp1 = crate::rp1_usb::any_pending();
    #[cfg(not(all(target_arch = "aarch64", feature = "rpi5")))]
    let rp1 = false;
    if ctrls.is_empty() && !rp1 {
        return;
    }
    usb::platform::install(usb::platform::Hooks {
        now_ns: sched::monotonic_ns,
        relax,
        log: console_log,
    });
    if cfg!(feature = "usb-debug") {
        set_debug(true);
    }
    crate::usb_hid::prepare();
    for pci in &ctrls {
        unsafe { probe_one(pci) };
    }
    // Pi 5: RP1's DWC3 controllers (their own DMA offset; nothing else on that SoC uses xHCI).
    #[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
    if rp1 && ctrls.is_empty() { crate::rp1_usb::probe_pending(); }
    crate::usb_hid::init();
}

/// Early quiesce of every xHCI controller (lane usbladder, L8.3).
///
/// Firmware (UEFI's USB stack, or a BIOS doing legacy keyboard emulation) can
/// leave an xHCI running at hand-off: its schedules, event ring and scratchpad
/// buffers live in memory the firmware owned, which this kernel's frame
/// allocator hands out again long before `usb_hcd::init` (task context, after
/// the block layer) halts the controller. A controller still walking those
/// structures DMAs into reused pages. So as soon as PCI config space is
/// reachable (right after `pci::init_pci` / `init_ecam_from_acpi`) every
/// controller that decodes memory and masters the bus is taken from the BIOS
/// (USBLEGSUP), stopped (USBCMD.R/S = 0, wait USBSTS.HCH), reset (HCRST), and
/// its PCI bus mastering is turned off; `probe_one` turns it back on.
///
/// Runs in early boot: no scheduler, maybe no calibrated clock. Every wait is
/// bounded by an iteration count as well as by `monotonic_ns`.
pub fn early_quiesce() {
    for pci in &pci::find_by_class(0x0C, 0x03, 0x30) {
        unsafe { early_quiesce_one(pci) };
    }
}

/// Wait until `cond` or `us` microseconds (by `monotonic_ns`, or ~`us * 64`
/// polls when the clock does not advance yet).
fn early_wait(us: u64, mut cond: impl FnMut() -> bool) -> bool {
    let t0 = sched::monotonic_ns();
    for _ in 0..us.saturating_mul(64) {
        if cond() { return true; }
        if sched::monotonic_ns().saturating_sub(t0) >= us * 1000 { break; }
        core::hint::spin_loop();
    }
    cond()
}

unsafe fn early_quiesce_one(pci: &PciDevice) {
    let cmd = pci_read_config_16(pci.bus, pci.dev, pci.func, 0x04);
    if cmd & 0x4 == 0 {
        // Not a bus master: it cannot DMA.
        console_log(&alloc::format!("[XHCI] {:02x}:{:02x}.{} early quiesce: idle (bus mastering already off)\n",
                                    pci.bus, pci.dev, pci.func));
        return;
    }
    let raw = pci.bars[0];
    let is64 = raw & 0x6 == 0x4;
    let phys = (raw & !0xF) as u64 | if is64 { (pci.bars[1] as u64) << 32 } else { 0 };
    let mut what = "bus mastering off";
    if raw & 1 == 0 && phys != 0 && cmd & 0x2 != 0 {
        pci_write_config_16(pci.bus, pci.dev, pci.func, 0x04, cmd & !0x3);
        let size = bar_size(pci, 0, is64);
        pci_write_config_16(pci.bus, pci.dev, pci.func, 0x04, cmd);
        let map = if (0x1000..=16 << 20).contains(&size) {
            mm::paging::map_kernel_device(
                phys as usize, size as usize,
                mm::paging::PageFlags::PRESENT | mm::paging::PageFlags::WRITABLE | mm::paging::PageFlags::MMIO)
        } else { None };
        if let Some(base) = map {
            what = early_halt_reset(base, size as usize);
        }
    }
    // Halted or not, nothing DMAs without bus mastering.
    pci_write_config_16(pci.bus, pci.dev, pci.func, 0x04, cmd & !0x4);
    console_log(&alloc::format!("[XHCI] {:02x}:{:02x}.{} early quiesce: {}\n", pci.bus, pci.dev, pci.func, what));
}

/// BIOS handoff, halt, HCRST on the controller mapped at `base`. Returns
/// what was done (for the log line).
unsafe fn early_halt_reset(base: usize, len: usize) -> &'static str {
    let r = |off: usize| core::ptr::read_volatile((base + off) as *const u32);
    let w = |off: usize, v: u32| core::ptr::write_volatile((base + off) as *mut u32, v);
    let cap = r(0);
    if cap == 0xFFFF_FFFF { return "no response (all-ones), bus mastering off"; }
    let op = (cap & 0xFF) as usize;
    if op < 0x20 || op + 8 > len { return "bad CAPLENGTH, bus mastering off"; }
    // USB Legacy Support: take ownership, mask the BIOS's SMIs.
    let mut off = ((r(0x10) >> 16) as usize) * 4;
    let mut guard = 0;
    while off != 0 && off + 8 <= len && guard < 64 {
        let dw0 = r(off);
        if dw0 & 0xFF == 1 {
            if dw0 & (1 << 16) != 0 {
                w(off, dw0 | (1 << 24));
                if !early_wait(1_000_000, || r(off) & (1 << 16) == 0) {
                    w(off, (r(off) & !(1 << 16)) | (1 << 24));
                }
            }
            // SMI enables off (bits 0, 4, 13..15), write-1-to-clear events (29..31).
            let c = r(off + 4);
            w(off + 4, (c & 0x000E_1FEE) | 0xE000_0000);
            break;
        }
        let next = ((dw0 >> 8) & 0xFF) as usize;
        if next == 0 { break; }
        off += next * 4;
        guard += 1;
    }
    const CMD_RUN: u32 = 1 << 0;
    const CMD_HCRST: u32 = 1 << 1;
    const CMD_INTE: u32 = 1 << 2;
    const CMD_HSEE: u32 = 1 << 3;
    const STS_HCH: u32 = 1 << 0;
    const STS_CNR: u32 = 1 << 11;
    let running = r(op + 4) & STS_HCH == 0;
    if running {
        w(op, r(op) & !(CMD_RUN | CMD_INTE | CMD_HSEE));
        if !early_wait(100_000, || r(op + 4) & STS_HCH != 0) {
            return "was RUNNING and did not halt; bus mastering off";
        }
    }
    if !early_wait(1_000_000, || r(op + 4) & STS_CNR == 0) {
        return if running { "was running, halted; not ready for reset" } else { "halted; not ready for reset" };
    }
    w(op, CMD_HCRST);
    // Some Intel controllers hang if USBCMD is read too soon after HCRST.
    let t0 = sched::monotonic_ns();
    for _ in 0..200_000u32 {
        if sched::monotonic_ns().saturating_sub(t0) >= 1_000_000 { break; }
        core::hint::spin_loop();
    }
    let done = early_wait(1_000_000, || r(op) & CMD_HCRST == 0 && r(op + 4) & STS_CNR == 0);
    match (running, done) {
        (true, true) => "was RUNNING (firmware left it on): halted + reset",
        (true, false) => "was RUNNING: halted, reset timed out",
        (false, true) => "halted + reset",
        (false, false) => "halted, reset timed out",
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

    CONTROLLERS.lock().push(Ctrl { bus: pci.bus, dev: pci.dev, func: pci.func, vendor: pci.vendor_id, base: virt });
    usb::usb_log!("[XHCI] {:02x}:{:02x}.{} {:04x}:{:04x} BAR0 {:#x} ({} KiB)\n",
                  pci.bus, pci.dev, pci.func, pci.vendor_id, pci.device_id, phys, size / 1024);
    match usb::probe_xhci(virt, size as usize) {
        Ok(n) => usb::usb_log!("[XHCI] {} device(s), {} mass-storage interface(s)\n", n, usb::mass_storage_count()),
        Err(e) => usb::usb_log!("[XHCI] controller bring-up failed: {}\n", e),
    }
}

/// `usb.tbxhci=0`: skip the controller-gone (all-ones USBSTS) guard in the hot-plug scan.
pub fn disable_controller_gone_guard() {
    usb::TBXHCI_GUARD_OFF.store(true, core::sync::atomic::Ordering::Relaxed);
}

pub fn device_count() -> usize {
    usb::controller_count() + usb::devices().len()
}

pub fn device_info(index: usize) -> Option<UsbDevInfo> {
    let nctl = usb::controller_count();
    if index < nctl {
        let (_, usb3) = usb::controller_ports(index)?;
        return Some(UsbDevInfo {
            bus: index as u8,
            address: 1,
            vendor_id: 0x1d6b,
            product_id: if usb3 { 0x0003 } else { 0x0002 },
            class: 0x09,
            role: ROLE_HUB | ROLE_ROOT_HUB,
            port: 0,
            speed: if usb3 { 4 } else { 3 },
        });
    }
    let d = usb::devices().into_iter().nth(index - nctl)?;
    Some(UsbDevInfo {
        // `lsusb` prints bus + 1 (0-based, like the PCI-style syscalls).
        bus: d.bus.saturating_sub(1),
        address: d.address,
        vendor_id: d.vendor,
        product_id: d.product,
        class: d.class,
        role: (d.hid & (usb::hid::ROLE_KEYBOARD | usb::hid::ROLE_MOUSE))
            | if d.mass_storage { ROLE_MASS_STORAGE } else { 0 }
            | if d.class == 0x09 { ROLE_HUB } else { 0 }
            | if d.hid & usb::hid::ROLE_HID_UNSUPPORTED != 0 { ROLE_HID_UNSUPPORTED } else { 0 },
        port: d.port,
        speed: d.speed,
    })
}

// ── Reboot / power-off quiesce ───────────────────────────────────────────────

/// Every xHCI the driver mapped, for [`quiesce_all`].
struct Ctrl { bus: u8, dev: u8, func: u8, vendor: u16, base: usize }
static CONTROLLERS: spin::Mutex<alloc::vec::Vec<Ctrl>> = spin::Mutex::new(alloc::vec::Vec::new());

/// Registers a non-PCI-class xHCI (the Pi 5's RP1 DWC3s; `bus:dev.func` is the
/// PCI function whose bus mastering carries its DMA) so `quiesce_all` halts
/// and resets it before kexec like any other controller.
pub fn register_platform_controller(bus: u8, dev: u8, func: u8, vendor: u16, base: usize) {
    CONTROLLERS.lock().push(Ctrl { bus, dev, func, vendor, base });
}

// xHCI register offsets/bits used below (xHCI 1.2 §5; same values as
// drivers/usb/src/xhci/regs.rs).
const CAP_HCSPARAMS1: usize = 0x04;
const CAP_HCCPARAMS1: usize = 0x10;
const HCCPARAMS1_PPC: u32 = 1 << 3;
const OP_USBCMD: usize = 0x00;
const OP_USBSTS: usize = 0x04;
const OP_PORTSC: usize = 0x400;
const CMD_RUN: u32 = 1 << 0;
const CMD_HCRST: u32 = 1 << 1;
const CMD_INTE: u32 = 1 << 2;
const CMD_HSEE: u32 = 1 << 3;
const STS_HCH: u32 = 1 << 0;
const STS_CNR: u32 = 1 << 11;
const PORTSC_PP: u32 = 1 << 9;

#[inline(always)]
fn mmio_r32(a: usize) -> u32 {
    #[cfg(target_arch = "aarch64")]
    unsafe { let v: u32; core::arch::asm!("ldr {v:w}, [{a}]", a = in(reg) a, v = out(reg) v, options(nostack, readonly)); v }
    #[cfg(not(target_arch = "aarch64"))]
    unsafe { core::ptr::read_volatile(a as *const u32) }
}
#[inline(always)]
fn mmio_w32(a: usize, v: u32) {
    #[cfg(target_arch = "aarch64")]
    unsafe { core::arch::asm!("str {v:w}, [{a}]", a = in(reg) a, v = in(reg) v, options(nostack)); }
    #[cfg(not(target_arch = "aarch64"))]
    unsafe { core::ptr::write_volatile(a as *mut u32, v) }
}
/// Spin until `cond` (Some(ns waited)) or `timeout_ns` (None).
fn spin_until(timeout_ns: u64, mut cond: impl FnMut() -> bool) -> Option<u64> {
    let t0 = sched::monotonic_ns();
    loop {
        if cond() { return Some(sched::monotonic_ns().saturating_sub(t0)); }
        if sched::monotonic_ns().saturating_sub(t0) > timeout_ns { return if cond() { Some(timeout_ns) } else { None }; }
        core::hint::spin_loop();
    }
}

/// Stop every xHCI before the machine resets or powers off — Linux
/// `usb_hcd_pci_shutdown` → `xhci_shutdown`: Run/Stop = 0 and wait for HCH
/// (32 ms; the spec allows 16), then, with `hcrst`, a host-controller reset
/// (Linux does that at shutdown only on its XHCI_SPURIOUS_WAKEUP /
/// RESET_TO_DEFAULT machines; here it is the default, so the firmware finds
/// a controller in its power-on state), waiting up to 250 ms for HCRST and
/// CNR to clear (Linux's XHCI_RESET_SHORT_USEC) after the 1 ms Intel
/// post-HCRST delay (XHCI_INTEL_HOST: an early register read can hang the
/// machine). With `vbus_off` and HCCPARAMS1.PPC, every root port's power is
/// switched off last, so the devices see a power cycle across a warm reset.
/// Bus mastering is cleared (Linux: `pci_disable_device`). IRQ-safe, no
/// lock but the registry's (`try_lock`), every wait bounded: a dead
/// controller costs at most ~300 ms. One report line per controller.
pub fn quiesce_all(hcrst: bool, vbus_off: bool) -> alloc::vec::Vec<alloc::string::String> {
    use core::fmt::Write as _;
    let mut out = alloc::vec::Vec::new();
    let ctrls = match CONTROLLERS.try_lock() {
        Some(c) => c,
        None => {
            out.push(alloc::string::String::from("xHCI registry busy; controllers left running"));
            return out;
        }
    };
    for c in ctrls.iter() {
        let mut s = alloc::string::String::new();
        let _ = write!(s, "xHCI {:02x}:{:02x}.{}:", c.bus, c.dev, c.func);
        let caplen = (mmio_r32(c.base) & 0xFF) as usize;
        let op = c.base + caplen;
        if caplen == 0 || mmio_r32(op + OP_USBSTS) == !0 {
            s.push_str(" registers read all-ones; skipped");
            out.push(s);
            continue;
        }
        let hcc1 = mmio_r32(c.base + CAP_HCCPARAMS1);
        let ports = ((mmio_r32(c.base + CAP_HCSPARAMS1) >> 24) as usize).min(255);
        let cmd = mmio_r32(op + OP_USBCMD);
        mmio_w32(op + OP_USBCMD, cmd & !(CMD_RUN | CMD_INTE | CMD_HSEE));
        let halted = spin_until(32_000_000, || mmio_r32(op + OP_USBSTS) & STS_HCH != 0);
        match halted {
            Some(ns) => { let _ = write!(s, " halted in {} us (was {})", ns / 1000, if cmd & CMD_RUN != 0 { "running" } else { "stopped" }); }
            None => { let _ = write!(s, " did NOT halt in 32 ms (USBSTS {:#x})", mmio_r32(op + OP_USBSTS)); }
        }
        if hcrst && halted.is_some() {
            mmio_w32(op + OP_USBCMD, CMD_HCRST);
            if c.vendor == 0x8086 {
                let t = sched::monotonic_ns();
                while sched::monotonic_ns().saturating_sub(t) < 1_000_000 { core::hint::spin_loop(); }
            }
            match spin_until(250_000_000, || mmio_r32(op + OP_USBCMD) & CMD_HCRST == 0
                                              && mmio_r32(op + OP_USBSTS) & STS_CNR == 0) {
                Some(ns) => { let _ = write!(s, ", HCRST done in {} us", ns / 1000); }
                None => { let _ = write!(s, ", HCRST did NOT complete in 250 ms (USBCMD {:#x} USBSTS {:#x})",
                                         mmio_r32(op + OP_USBCMD), mmio_r32(op + OP_USBSTS)); }
            }
        } else if !hcrst {
            s.push_str(", no HCRST (reboot.xhci_hcrst=0)");
        }
        let ppc = hcc1 & HCCPARAMS1_PPC != 0;
        if vbus_off {
            if ppc {
                // xhci_port_state_to_neutral: keep only the RO and RWS bits,
                // so no RW1C change bit (or PED, which disables) is written 1.
                const RO_RWS: u32 = (1 << 0) | (1 << 3) | (0xF << 10) | (1 << 30)
                                  | (0xF << 5) | (1 << 9) | (0x3 << 14) | (0x7 << 25);
                let mut n = 0;
                for i in 0..ports {
                    let a = op + OP_PORTSC + 0x10 * i;
                    let v = mmio_r32(a);
                    if v & PORTSC_PP != 0 {
                        mmio_w32(a, v & RO_RWS & !PORTSC_PP);
                        n += 1;
                    }
                }
                let off = spin_until(20_000_000, || (0..ports)
                    .all(|i| mmio_r32(op + OP_PORTSC + 0x10 * i) & PORTSC_PP == 0)).is_some();
                let _ = write!(s, ", port power off on {} of {} port(s){}", n, ports, if off { "" } else { " (some PP still set)" });
            } else {
                s.push_str(", no port power control (HCCPARAMS1.PPC=0): VBUS stays on");
            }
        }
        pci::PciFunction::new(c.bus, c.dev, c.func).set_bus_master(false);
        let _ = write!(s, ", bus master off; PPC={}", ppc as u8);
        out.push(s);
    }
    out
}

// ── Early USB BIOS handoff (x86_64) ──────────────────────────────────────────

/// A fixed-size line buffer: the early handoff runs before the heap is
/// trusted for anything, so it formats on the stack.
struct LineBuf { b: [u8; 200], n: usize }
impl core::fmt::Write for LineBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &c in s.as_bytes() { if self.n < self.b.len() { self.b[self.n] = c; self.n += 1; } }
        Ok(())
    }
}
fn early_line(args: core::fmt::Arguments) {
    use core::fmt::Write as _;
    let mut l = LineBuf { b: [0; 200], n: 0 };
    let _ = l.write_fmt(args);
    console_log(unsafe { core::str::from_utf8_unchecked(&l.b[..l.n]) });
}

/// Take every xHCI and EHCI away from the firmware's USB legacy support
/// before anything touches the legacy keyboard ports — Linux's
/// `quirk_usb_early_handoff` (drivers/usb/host/pci-quirks.c), which runs as
/// a PCI fixup long before i8042 or any USB driver:
/// - xHCI: USBLEGSUP extended capability: set OS-owned, wait up to 1 s for
///   BIOS-owned to clear (then clear it ourselves, "BIOS bug"), then
///   USBLEGCTLSTS: keep only the RsvdP bits, disable every SMI enable, ack
///   the SMI status bits (`quirk_usb_handoff_xhci`);
/// - EHCI: USBLEGSUP in config space at HCCPARAMS.EECP: OS-owned, wait up to
///   1 s, then USBLEGCTLSTS = 0 (`ehci_bios_handoff`).
/// Does not halt or reset anything (the xHCI driver does that at probe) and
/// never changes bus mastering. Legacy config-space access (0xCF8/0xCFC),
/// every bus; nothing is allocated. With Fast Boot OFF an HP EliteBook's
/// firmware enumerates USB fully and keeps legacy keyboard/storage
/// emulation in SMM until this point; with Fast Boot ON it skips USB, so the
/// handoff is a no-op there (BIOS-owned clear).
#[cfg(target_arch = "x86_64")]
pub fn early_handoff_all() {
    let mut seen = 0u32;
    for bus in 0..=255u8 {
        for dev in 0..32u8 {
            let id0 = unsafe { pci_read_config_32(bus, dev, 0, 0) };
            if id0 & 0xFFFF == 0xFFFF { continue; }
            let hdr = (unsafe { pci_read_config_32(bus, dev, 0, 0x0C) } >> 16) as u8;
            let nf = if hdr & 0x80 != 0 { 8 } else { 1 };
            for func in 0..nf {
                let id = unsafe { pci_read_config_32(bus, dev, func, 0) };
                if id & 0xFFFF == 0xFFFF { continue; }
                let cc = unsafe { pci_read_config_32(bus, dev, func, 0x08) } >> 8;
                match cc {
                    0x0C0330 => { seen += 1; unsafe { early_xhci_handoff(bus, dev, func) } }
                    0x0C0320 => { seen += 1; unsafe { early_ehci_handoff(bus, dev, func) } }
                    _ => {}
                }
            }
        }
    }
    if seen == 0 { console_log("[USB] early BIOS handoff: no xHCI/EHCI on the bus\n"); }
}

#[cfg(target_arch = "x86_64")]
unsafe fn early_bar0(bus: u8, dev: u8, func: u8) -> Option<u64> {
    let lo = pci_read_config_32(bus, dev, func, 0x10);
    if lo & 1 != 0 { return None; }
    let hi = if (lo >> 1) & 3 == 2 { pci_read_config_32(bus, dev, func, 0x14) as u64 } else { 0 };
    let phys = (hi << 32) | (lo & !0xF) as u64;
    let cmd = pci_read_config_16(bus, dev, func, 0x04);
    if phys == 0 || cmd & 0x2 == 0 { return None; } // unassigned, or memory decode off
    Some(phys)
}

#[cfg(target_arch = "x86_64")]
unsafe fn early_xhci_handoff(bus: u8, dev: u8, func: u8) {
    let Some(phys) = early_bar0(bus, dev, func) else {
        early_line(format_args!("[XHCI] {:02x}:{:02x}.{}: early handoff skipped (BAR0 unassigned or decode off)\n", bus, dev, func));
        return;
    };
    const LEN: usize = 0x10000;
    let Some(base) = mm::paging::map_kernel_device(phys as usize, LEN,
        mm::paging::PageFlags::PRESENT | mm::paging::PageFlags::WRITABLE | mm::paging::PageFlags::MMIO) else { return };
    let hcc1 = mmio_r32(base + CAP_HCCPARAMS1);
    if hcc1 == !0 { return; }
    let mut off = ((hcc1 >> 16) as usize) * 4;
    let mut guard = 0;
    while off != 0 && off + 8 <= LEN && guard < 64 {
        let dw0 = mmio_r32(base + off);
        if dw0 & 0xFF == 1 {
            // USBLEGSUP
            let before = dw0;
            let mut how = "not BIOS-owned";
            if dw0 & (1 << 16) != 0 {
                mmio_w32(base + off, dw0 | (1 << 24));
                if spin_until(1_000_000_000, || mmio_r32(base + off) & (1 << 16) == 0).is_some() {
                    how = "BIOS released it";
                } else {
                    let v = mmio_r32(base + off);
                    mmio_w32(base + off, (v & !(1 << 16)) | (1 << 24));
                    how = "BIOS did NOT release it in 1 s; taken";
                }
            }
            let ctl = mmio_r32(base + off + 4);
            const KEEP: u32 = (0x7 << 1) | (0xff << 5) | (0x7 << 17);
            const SMI_EVENTS: u32 = 0x7 << 29;
            mmio_w32(base + off + 4, (ctl & KEEP) | SMI_EVENTS);
            early_line(format_args!("[XHCI] {:02x}:{:02x}.{}: early BIOS handoff: USBLEGSUP {:#010x} -> {:#010x} ({}), USBLEGCTLSTS {:#010x} -> {:#010x}\n",
                bus, dev, func, before, mmio_r32(base + off), how, ctl, mmio_r32(base + off + 4)));
            return;
        }
        let next = ((dw0 >> 8) & 0xFF) as usize;
        if next == 0 { break; }
        off += next * 4;
        guard += 1;
    }
}

#[cfg(target_arch = "x86_64")]
unsafe fn early_ehci_handoff(bus: u8, dev: u8, func: u8) {
    let Some(phys) = early_bar0(bus, dev, func) else { return };
    let Some(base) = mm::paging::map_kernel_device(phys as usize, 0x1000,
        mm::paging::PageFlags::PRESENT | mm::paging::PageFlags::WRITABLE | mm::paging::PageFlags::MMIO) else { return };
    let hccparams = mmio_r32(base + 0x08);
    let eecp = ((hccparams >> 8) & 0xFF) as u8;
    if eecp < 0x40 { return; }
    let leg = pci_read_config_32(bus, dev, func, eecp);
    if leg & 0xFF != 1 { return; }
    let mut how = "not BIOS-owned";
    if leg & (1 << 16) != 0 {
        pci_write_config_32(bus, dev, func, eecp, leg | (1 << 24));
        if spin_until(1_000_000_000, || pci_read_config_32(bus, dev, func, eecp) & (1 << 16) == 0).is_some() {
            how = "BIOS released it";
        } else {
            let v = pci_read_config_32(bus, dev, func, eecp);
            pci_write_config_32(bus, dev, func, eecp, (v & !(1 << 16)) | (1 << 24));
            how = "BIOS did NOT release it in 1 s; taken";
        }
    }
    pci_write_config_32(bus, dev, func, eecp + 4, 0);
    early_line(format_args!("[EHCI] {:02x}:{:02x}.{}: early BIOS handoff: USBLEGSUP {:#010x} ({}), SMIs off\n", bus, dev, func, leg, how));
}

/// (operational base, USBCMD) per controller, saved across a system-sleep cycle.
static PM_SAVE: spin::Mutex<alloc::vec::Vec<(usize, u32)>> = spin::Mutex::new(alloc::vec::Vec::new());

fn pm_wait_hch(op: usize, halted: bool) -> Option<u64> {
    let t = sched::monotonic_ns();
    while sched::monotonic_ns() - t < 32_000_000 {
        if (mmio_r32(op + OP_USBSTS) & 1 != 0) == halted { return Some(sched::monotonic_ns() - t); }
        core::hint::spin_loop();
    }
    None
}

/// System-sleep suspend hook (kernel pm). Default: controller interrupts off
/// (USBCMD.INTE), the schedule keeps running. With `halt`: USBCMD.R/S = 0 and
/// wait for HCHalted (xHCI 4.15: a halted controller keeps its rings and
/// device contexts and continues from them when R/S is set again).
pub fn pm_suspend(halt: bool) -> Result<alloc::string::String, alloc::string::String> {
    let ctrls = CONTROLLERS.lock();
    let mut save = PM_SAVE.lock();
    save.clear();
    for c in ctrls.iter() {
        let caplen = (mmio_r32(c.base) & 0xFF) as usize;
        let op = c.base + caplen;
        if caplen == 0 || mmio_r32(op + OP_USBSTS) == !0 { continue; }
        let cmd = mmio_r32(op + OP_USBCMD);
        save.push((op, cmd));
        if halt {
            mmio_w32(op + OP_USBCMD, cmd & !(CMD_RUN | CMD_INTE));
            if cmd & CMD_RUN != 0 && pm_wait_hch(op, true).is_none() {
                for &(o, v) in save.iter() { mmio_w32(o + OP_USBCMD, v); }
                save.clear();
                return Err(alloc::format!("xHCI {:02x}:{:02x}.{} did not halt in 32 ms", c.bus, c.dev, c.func));
            }
        } else {
            mmio_w32(op + OP_USBCMD, cmd & !CMD_INTE);
        }
    }
    Ok(alloc::format!("{} controller(s) {}", save.len(), if halt { "halted" } else { "interrupts masked" }))
}

/// System-sleep resume hook: USBCMD back (R/S, INTE), wait for the run state.
pub fn pm_resume() -> Result<alloc::string::String, alloc::string::String> {
    let save = core::mem::take(&mut *PM_SAVE.lock());
    let mut bad = 0;
    for &(op, cmd) in save.iter() {
        mmio_w32(op + OP_USBCMD, cmd);
        if cmd & CMD_RUN != 0 && pm_wait_hch(op, false).is_none() { bad += 1; }
    }
    if bad != 0 { return Err(alloc::format!("{} of {} controller(s) did not restart", bad, save.len())); }
    Ok(alloc::format!("{} controller(s) running", save.len()))
}

/// `pm.s3_xhci=auto|restore|reinit` (lane s3xhci).
pub fn pm_set_s3_mode(v: Option<&str>) {
    let m = match v { Some("restore") => 1, Some("reinit") => 2, _ => 0 };
    usb::PM_XHCI_MODE.store(m, core::sync::atomic::Ordering::Relaxed);
}

/// S3 suspend: ports to U3, controllers halted and their state saved.
pub fn pm_suspend_s3() -> Result<alloc::string::String, alloc::string::String> { usb::pm_suspend_all() }

/// S3 resume: restore or reinit, re-address the persisted devices, then check
/// every USB disk is the same one (TEST UNIT READY, READ CAPACITY).
pub fn pm_resume_s3() -> Result<alloc::string::String, alloc::string::String> {
    let mut s = usb::pm_resume_all()?;
    s.push_str(&crate::usb_storage::pm_resume_check()?);
    Ok(s)
}
