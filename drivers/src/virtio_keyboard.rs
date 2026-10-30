//! VirtIO input/keyboard driver (PCI transport, polling mode).

#[allow(unused_imports)]
use leandros_lib::mmio::{MmioRd, MmioWr};
use spin::Mutex;
// Kernel APIs come through `kenv` so the same source builds as a module (kmod-kapi shim).
use crate::kenv::pci::{PciDevice, pci_read_config_8, pci_read_config_16, pci_read_config_32, pci_write_config_16};
use crate::kenv::{irq, mm};

const VIRTIO_PCI_VENDOR: u16 = 0x1af4;
const VIRTIO_PCI_DEVICE_INPUT: u16 = 0x1052; // modern virtio-input

const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

// virtio-input device-config selects (virtio 1.1 §5.8.4).
const VIRTIO_INPUT_CFG_EV_BITS: u8 = 0x11;
const EV_KEY: u8 = 0x01;
const EV_REL: u8 = 0x02;
const EV_ABS: u8 = 0x03;

// evdev node indices (must match servers/evdev): keyboard=event0, tablet=event1.
const EVDEV_KEYBOARD: u32 = 0;
const EVDEV_TABLET: u32 = 1;

const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
const VIRTIO_STATUS_DRIVER: u8 = 2;
const VIRTIO_STATUS_FEATURES_OK: u8 = 8;
const VIRTIO_STATUS_DRIVER_OK: u8 = 4;

const VIRTQ_DESC_F_WRITE: u16 = 2;

#[repr(C, packed)]
struct VirtioPciCommonCfg {
    device_feature_select: u32,
    device_feature: u32,
    driver_feature_select: u32,
    driver_feature: u32,
    config_msix_vector: u16,
    num_queues: u16,
    device_status: u8,
    config_generation: u8,
    queue_select: u16,
    queue_size: u16,
    queue_msix_vector: u16,
    queue_enable: u16,
    queue_notify_off: u16,
    queue_desc: u64,
    queue_driver: u64,
    queue_device: u64,
}

#[repr(C, packed)]
struct VirtqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C, packed)]
struct VirtqAvail {
    flags: u16,
    idx: u16,
    ring: [u16; 0],
}

#[repr(C, packed)]
struct VirtqUsedElem {
    id: u32,
    len: u32,
}

#[repr(C, packed)]
struct VirtqUsed {
    flags: u16,
    idx: u16,
    ring: [VirtqUsedElem; 0],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct VirtioInputEvent {
    type_: u16,
    code: u16,
    value: i32,
}

struct VirtioQueue {
    size: u16,
    notify_off: u16,
    last_used_idx: u16,
    _desc: *mut VirtqDesc,
    avail: *mut VirtqAvail,
    used: *mut VirtqUsed,
}

unsafe impl Send for VirtioQueue {}
unsafe impl Sync for VirtioQueue {}

pub struct VirtioKeyboardDevice {
    _pci_dev: PciDevice,
    common_cfg: *mut VirtioPciCommonCfg,
    notify_cfg: *mut u32,
    device_cfg: *mut u8,
    notify_off_multiplier: u32,
    /// evdev node this instance feeds (0 = keyboard, 1 = tablet), decided by
    /// probing the device's EV_ABS capability.
    evdev_index: u32,

    queue: Option<VirtioQueue>,
    event_buffer_phys: usize,
}

unsafe impl Send for VirtioKeyboardDevice {}
unsafe impl Sync for VirtioKeyboardDevice {}

// QEMU exposes virtio-keyboard-pci and virtio-tablet-pci as two separate
// virtio-input PCI functions; we bind ALL of them and route each to its evdev
// node. (Static-init empty Vec — no allocation until init() runs.)
pub static VIRTIO_INPUTS: Mutex<alloc::vec::Vec<VirtioKeyboardDevice>> = Mutex::new(alloc::vec::Vec::new());

// ── eventq census ───────────────────────────────────────────────────────────
//
// Master gate, same shape as `syscall::EV_STATS`: a `const`, so every counter
// and the whole sampler compile out when it is off. IT MUST BE `false` IN A
// COMMITTED TREE — c5abb8d shipped a diagnostic switched on and had to be
// reverted.
//
// WHAT EACH NUMBER SETTLES. `polls` is the number of times the 100 Hz tick
// reached the drain at all; against wall time it says whether the drain really
// runs at 100 Hz. `skips` is the try_lock contention that was previously
// INVISIBLE — a poll that never happened used to be indistinguishable from a
// poll that found nothing. `minfree` is the low-water mark of
// `avail.idx - used.idx`, i.e. how many buffers the device still owned at the
// moment we looked: a minimum that reaches 0 IS ring exhaustion, and a minimum
// that stays near the queue size refutes the capacity story outright.
pub const VQ_STATS: bool = false;

static VQ_POLLS:   core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static VQ_SKIPS:   core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static VQ_DRAINED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static VQ_MAXB:    core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static VQ_MINFREE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(u32::MAX);
static VQ_STARVE:  core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static VQ_AIDX:    core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static VQ_UIDX:    core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static VQ_NOTIFY:  core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

pub struct VqCensus {
    pub polls: u32,
    pub skips: u32,
    pub drained: u32,
    pub maxb: u32,
    pub minfree: u32,
    pub starve: u32,
    pub aidx: u32,
    pub uidx: u32,
    pub notify: u32,
}

/// Snapshot of the eventq counters. Relaxed loads only — safe from IRQ context,
/// takes no lock. `minfree` reads `0xffffffff` when no poll has looked yet;
/// that is "never sampled", not "no buffers".
pub fn vq_census() -> VqCensus {
    use core::sync::atomic::Ordering::Relaxed;
    VqCensus {
        polls:   VQ_POLLS.load(Relaxed),
        skips:   VQ_SKIPS.load(Relaxed),
        drained: VQ_DRAINED.load(Relaxed),
        maxb:    VQ_MAXB.load(Relaxed),
        minfree: VQ_MINFREE.load(Relaxed),
        starve:  VQ_STARVE.load(Relaxed),
        aidx:    VQ_AIDX.load(Relaxed),
        uidx:    VQ_UIDX.load(Relaxed),
        notify:  VQ_NOTIFY.load(Relaxed),
    }
}

impl VirtioKeyboardDevice {
    pub fn new_from(dev: PciDevice) -> Option<Self> {
        crate::kenv::pci::serial_debug("[INPUT] Found VirtIO Input device\n");

        // Enable PCI Memory Space (bit 1) and Bus Master (bit 2); INTx
        // Disable (bit 10) too — this driver polls and never reads its ISR
        // status, so an asserted INTx would stay asserted forever and storm
        // the GPU's completion interrupt on a shared virt PCIe line
        // (`virtio_gpu::enable_intx`).
        unsafe {
            let cmd = pci_read_config_16(dev.bus, dev.dev, dev.func, 0x04);
            pci_write_config_16(dev.bus, dev.dev, dev.func, 0x04, (cmd | 0x0006) | crate::kenv::pci::PCI_CMD_INTX_DISABLE);
        }

        let mut common_cfg = core::ptr::null_mut();
        let mut notify_cfg = core::ptr::null_mut();
        let mut device_cfg: *mut u8 = core::ptr::null_mut();
        let mut notify_off_multiplier = 0;

        unsafe {
            let mut cap_ptr = pci_read_config_8(dev.bus, dev.dev, dev.func, 0x34);
            while cap_ptr != 0 {
                let cap_id = pci_read_config_8(dev.bus, dev.dev, dev.func, cap_ptr);
                if cap_id == 0x09 {
                    let cfg_type = pci_read_config_8(dev.bus, dev.dev, dev.func, cap_ptr + 3);
                    let bar_idx = pci_read_config_8(dev.bus, dev.dev, dev.func, cap_ptr + 4);
                    let offset = pci_read_config_32(dev.bus, dev.dev, dev.func, cap_ptr + 8);
                    let length = pci_read_config_32(dev.bus, dev.dev, dev.func, cap_ptr + 12);
                    
                    if (bar_idx as usize) < 6 {
                        let raw_bar = dev.bars[bar_idx as usize];
                        if raw_bar & 1 == 0 {
                            let bar64: u64 = if (raw_bar >> 1) & 3 == 2 && (bar_idx as usize) + 1 < 6 {
                                let lo = (raw_bar & !0xF) as u64;
                                let hi = dev.bars[bar_idx as usize + 1] as u64;
                                lo | (hi << 32)
                            } else {
                                (raw_bar & !0xF) as u64
                            };

                            if bar64 != 0 {
                                let virt = mm::paging::map_kernel_device(
                                    bar64 as usize + offset as usize,
                                    length as usize,
                                    mm::paging::PageFlags::PRESENT | mm::paging::PageFlags::WRITABLE | mm::paging::PageFlags::MMIO,
                                ).unwrap_or_else(|| mm::phys_to_virt(bar64 as usize + offset as usize));
                                    
                                match cfg_type {
                                    VIRTIO_PCI_CAP_COMMON_CFG => {
                                        common_cfg = virt as *mut VirtioPciCommonCfg;
                                    },
                                    VIRTIO_PCI_CAP_NOTIFY_CFG => {
                                        notify_off_multiplier = pci_read_config_32(dev.bus, dev.dev, dev.func, cap_ptr + 16);
                                        notify_cfg = virt as *mut u32;
                                    },
                                    VIRTIO_PCI_CAP_DEVICE_CFG => {
                                        device_cfg = virt as *mut u8;
                                    },
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                cap_ptr = pci_read_config_8(dev.bus, dev.dev, dev.func, cap_ptr + 1);
            }
        }

        if common_cfg.is_null() || notify_cfg.is_null() {
            crate::kenv::pci::serial_debug("[KBD] Missing required VirtIO capabilities\n");
            return None;
        }

        // Classify by EV_ABS capability BEFORE resetting the device in
        // init_device: a virtio-tablet reports a non-zero EV_ABS bitmap size,
        // a keyboard does not. This is order-independent (vs relying on PCI
        // enumeration order).
        let is_tablet = !device_cfg.is_null() && unsafe { device_supports_ev_abs(device_cfg) };
        if !crate::kenv::input::claim(if is_tablet { EVDEV_TABLET } else { EVDEV_KEYBOARD }) { return None; }
        let evdev_index = if is_tablet {
            crate::kenv::pci::serial_debug("[INPUT] -> tablet (event1)\n");
            EVDEV_TABLET
        } else {
            crate::kenv::pci::serial_debug("[INPUT] -> keyboard (event0)\n");
            EVDEV_KEYBOARD
        };

        // Hand the tablet's own wheel/button bitmaps to evdev, so EVIOCGBIT
        // advertises exactly what this device will send (REL_WHEEL etc.).
        // Without the bits, libevdev inside libinput discards the events.
        if evdev_index == EVDEV_TABLET {
            let mut rel = [0u8; 4];
            let mut key = [0u8; 48];
            unsafe {
                read_ev_bits(device_cfg, EV_REL, &mut rel);
                read_ev_bits(device_cfg, EV_KEY, &mut key);
            }
            crate::kenv::input::set_device_caps(EVDEV_TABLET, &rel, &key);
            crate::kenv::pci::serial_debug("[INPUT] tablet EV_REL bits=");
            crate::kenv::pci::serial_debug_hex(u32::from_le_bytes(rel));
            crate::kenv::pci::serial_debug(" BTN 0x110 byte=");
            crate::kenv::pci::serial_debug_hex(key[0x110 >> 3] as u32);
            crate::kenv::pci::serial_debug("\n");
        }

        let mut kbd = Self {
            _pci_dev: dev,
            common_cfg,
            notify_cfg,
            device_cfg,
            notify_off_multiplier,
            evdev_index,
            queue: None,
            event_buffer_phys: 0,
        };

        kbd.init_device();
        unsafe { arm_irq(&kbd); }
        Some(kbd)
    }

    fn init_device(&mut self) {
        unsafe {
            let cfg = self.common_cfg;
            let status = core::ptr::addr_of_mut!((*cfg).device_status);
            // 1. Reset device
            status.mmio_write(0);
            // 2. Set ACKNOWLEDGE
            status.mmio_write(status.mmio_read() | VIRTIO_STATUS_ACKNOWLEDGE);
            // 3. Set DRIVER
            status.mmio_write(status.mmio_read() | VIRTIO_STATUS_DRIVER);
            // 4. Negotiate features
            core::ptr::addr_of_mut!((*cfg).device_feature_select).mmio_write(0);
            let _f0 = core::ptr::addr_of!((*cfg).device_feature).mmio_read();
            core::ptr::addr_of_mut!((*cfg).driver_feature_select).mmio_write(0);
            core::ptr::addr_of_mut!((*cfg).driver_feature).mmio_write(0);
            // 5. Set FEATURES_OK
            status.mmio_write(status.mmio_read() | VIRTIO_STATUS_FEATURES_OK);
            // 6. Setup queue 0 (eventq)
            self.queue = self.setup_queue(0);
            // 7. Set DRIVER_OK
            status.mmio_write(status.mmio_read() | VIRTIO_STATUS_DRIVER_OK);
        }
        crate::kenv::pci::serial_debug("[KBD] VirtIO Input device initialized\n");
    }

    unsafe fn setup_queue(&mut self, id: u16) -> Option<VirtioQueue> {
        let cfg = self.common_cfg;
        core::ptr::addr_of_mut!((*cfg).queue_select).mmio_write(id);
        let max_size = core::ptr::addr_of!((*cfg).queue_size).mmio_read();
        if max_size == 0 { return None; }

        let size = max_size.min(32); // Use at most 32 descriptors
        core::ptr::addr_of_mut!((*cfg).queue_size).mmio_write(size);

        let notify_off = core::ptr::addr_of!((*cfg).queue_notify_off).mmio_read();

        let desc_phys = mm::buddy::alloc(0)?;
        let avail_phys = mm::buddy::alloc(0)?;
        let used_phys = mm::buddy::alloc(0)?;

        let desc = mm::phys_to_virt(desc_phys) as *mut VirtqDesc;
        let avail = mm::phys_to_virt(avail_phys) as *mut VirtqAvail;
        let used = mm::phys_to_virt(used_phys) as *mut VirtqUsed;

        core::ptr::write_bytes(desc as *mut u8, 0, 4096);
        core::ptr::write_bytes(avail as *mut u8, 0, 4096);
        core::ptr::write_bytes(used as *mut u8, 0, 4096);

        // Allocate a page for event buffers
        let event_buffer_phys = mm::buddy::alloc(0)?;
        self.event_buffer_phys = event_buffer_phys;
        let event_buffer_virt = mm::phys_to_virt(event_buffer_phys);
        core::ptr::write_bytes(event_buffer_virt as *mut u8, 0, 4096);

        // Populate the descriptors and submit them to the avail ring
        for i in 0..size {
            let buf_phys = event_buffer_phys + (i as usize * 64);
            let d = desc.add(i as usize);
            (*d).addr = buf_phys as u64;
            (*d).len = 64;
            (*d).flags = VIRTQ_DESC_F_WRITE;
            (*d).next = 0xFFFF;

            let ring_ptr = (avail as usize + 4) as *mut u16;
            ring_ptr.add(i as usize).write_volatile(i);
        }

        core::ptr::addr_of_mut!((*avail).flags).write_volatile(0); // interrupts on
        // The ring slots above must be visible before the index that publishes
        // them. Same shape as virtio_gpu.rs:189-191.
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        core::ptr::addr_of_mut!((*avail).idx).write_volatile(size);

        core::ptr::addr_of_mut!((*cfg).queue_desc).write_volatile(desc_phys as u64);
        core::ptr::addr_of_mut!((*cfg).queue_driver).write_volatile(avail_phys as u64);
        core::ptr::addr_of_mut!((*cfg).queue_device).write_volatile(used_phys as u64);
        core::ptr::addr_of_mut!((*cfg).queue_enable).mmio_write(1u16);

        // Notify the device
        let notify_ptr = (self.notify_cfg as usize + notify_off as usize * self.notify_off_multiplier as usize) as *mut u16;
        notify_ptr.mmio_write(id);

        Some(VirtioQueue {
            size,
            notify_off,
            last_used_idx: 0,
            _desc: desc,
            avail,
            used,
        })
    }

    pub fn poll(&mut self) {
        let evdev_index = self.evdev_index;
        let q = match &mut self.queue {
            Some(q) => q,
            None => return,
        };

        unsafe {
            let used = q.used;
            let last_used = q.last_used_idx;

            // Sampled BEFORE the drain: `avail.idx - used.idx` is how many
            // buffers the device still owns and can pop into. Zero here means
            // the host had nothing left to write the next event frame into,
            // which is the only shape in which the queue depth is the defect.
            if VQ_STATS {
                use core::sync::atomic::Ordering::Relaxed;
                let a = core::ptr::addr_of!((*q.avail).idx).read_volatile();
                let u = core::ptr::addr_of!((*used).idx).read_volatile();
                VQ_AIDX.store(a as u32, Relaxed);
                VQ_UIDX.store(u as u32, Relaxed);
                let free = a.wrapping_sub(u) as u32;
                VQ_MINFREE.fetch_min(free, Relaxed);
                if free == 0 { VQ_STARVE.fetch_add(1, Relaxed); }
            }

            // Volatile: the device writes this behind the compiler's back, so a
            // plain load may be hoisted or reused. The barrier after it is the
            // virtio read barrier — `used.idx` must be observed before the ring
            // entries it publishes.
            let current_used = core::ptr::addr_of!((*used).idx).read_volatile();
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

            if last_used == current_used {
                return;
            }

            let mut count = 0;
            let mut idx = last_used;
            while idx != current_used {
                let ring_idx = idx as usize % q.size as usize;
                let ring_ptr = (used as usize + 4) as *const VirtqUsedElem;
                let elem = ring_ptr.add(ring_idx).read_volatile();
                let desc_id = elem.id as u16;

                // Process event
                let buf_virt = mm::phys_to_virt(self.event_buffer_phys + (desc_id as usize * 64));
                let ev = (buf_virt as *const VirtioInputEvent).read_volatile();

                // Push to this device's evdev node (keyboard=0, tablet=1).
                crate::kenv::input::push_event(evdev_index, ev.type_, ev.code, ev.value);
                EVENTS_SEEN.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

                // Recycle descriptor: put it back into avail ring. The slot has
                // to be visible to the device before the index that publishes
                // it, so the write barrier between them is load-bearing, not
                // decoration — x86 store ordering hides its absence, aarch64
                // does not. Both accesses are volatile for the same reason the
                // slot write always was.
                let avail = q.avail;
                let avail_idx =
                    core::ptr::addr_of!((*avail).idx).read_volatile() as usize
                        % q.size as usize;
                let ring_ptr = (avail as usize + 4) as *mut u16;
                ring_ptr.add(avail_idx).write_volatile(desc_id);
                core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
                let next = core::ptr::addr_of!((*avail).idx)
                    .mmio_read()
                    .wrapping_add(1);
                core::ptr::addr_of_mut!((*avail).idx).write_volatile(next);

                count += 1;
                idx = idx.wrapping_add(1);
            }

            q.last_used_idx = current_used;

            if VQ_STATS {
                use core::sync::atomic::Ordering::Relaxed;
                VQ_DRAINED.fetch_add(count as u32, Relaxed);
                VQ_MAXB.fetch_max(count as u32, Relaxed);
            }

            if count > 0 {
                if VQ_STATS { VQ_NOTIFY.fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
                // Notify the device
                let notify_ptr = (self.notify_cfg as usize + q.notify_off as usize * self.notify_off_multiplier as usize) as *mut u16;
                notify_ptr.mmio_write(0); // Queue 0
            }
        }
    }
}

/// Probe the virtio-input device config for EV_ABS support (tablet vs keyboard).
unsafe fn device_supports_ev_abs(device_cfg: *mut u8) -> bool {
    // select = EV_BITS, subsel = EV_ABS; `size` (offset 2) is the bitmap length.
    cfg_write8(device_cfg.add(0), VIRTIO_INPUT_CFG_EV_BITS);
    cfg_write8(device_cfg.add(1), EV_ABS);
    cfg_read8(device_cfg.add(2)) != 0
}

/// Read the device's EV_BITS bitmap for event type `ev` into `out` (virtio
/// 1.1 §5.8.4: select/subsel at 0/1, `size` at 2, the bitmap union at 8).
/// Bytes past the device-reported size stay zero.
unsafe fn read_ev_bits(device_cfg: *mut u8, ev: u8, out: &mut [u8]) {
    cfg_write8(device_cfg.add(0), VIRTIO_INPUT_CFG_EV_BITS);
    cfg_write8(device_cfg.add(1), ev);
    let size = cfg_read8(device_cfg.add(2)) as usize;
    for i in 0..core::cmp::min(size, out.len()) {
        out[i] = cfg_read8(device_cfg.add(8 + i));
    }
}

/// Device-config byte accessors: plain `ldrb`/`strb` via the shared MMIO
/// helpers (no writeback addressing, which HVF cannot emulate).
#[inline(always)]
unsafe fn cfg_read8(p: *const u8) -> u8 { leandros_lib::mmio::read8(p) }
#[inline(always)]
unsafe fn cfg_write8(p: *mut u8, v: u8) { leandros_lib::mmio::write8(p, v) }


/// Max input devices with an interrupt slot.
const MAX_IRQ_DEVS: usize = 4;
/// Next free interrupt slot.
static NEXT_SLOT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
/// Per-slot ISR status byte (0 = MSI-X armed or none).
static ISR_PTR: [core::sync::atomic::AtomicUsize; MAX_IRQ_DEVS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; MAX_IRQ_DEVS];
/// Interrupts taken that found the device's condition (exported in /proc/vtstat).
pub static VIRTIO_INPUT_IRQS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Timer-tick polls that found events (an IRQ-driven device should leave this ~0).
static POLL_FOUND: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Set once any device has an armed interrupt: the tick poll drops to 1 Hz.
static IRQ_ARMED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static POLL_DIV: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

static EVENTS_SEEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
fn events_seen() -> u64 { EVENTS_SEEN.load(core::sync::atomic::Ordering::Relaxed) }

fn stats_hook(w: &mut dyn core::fmt::Write) {
    use core::fmt::Write as _;
    use core::sync::atomic::Ordering::Relaxed;
    let _ = writeln!(w, "virtio_input_irqs {} tick_poll_found {} irq_armed {}",
        VIRTIO_INPUT_IRQS.load(Relaxed), POLL_FOUND.load(Relaxed), IRQ_ARMED.load(Relaxed) as u8);
    let (stale, restores) = crate::kenv::input::vt_handoff_stats();
    let _ = writeln!(w, "vt_handoff stale_presents={} vt_restores={}", stale, restores);
}

/// Event-queue interrupt: drain the queues. A shared INTx line whose ISR byte
/// is clear is `None`. try_lock: if task context holds the list (init only),
/// the slow safety poll picks the events up.
extern "C" fn virtio_input_irq(ctx: usize) -> irq::IrqReturn {
    use core::sync::atomic::Ordering::{Acquire, Relaxed};
    if ctx >= MAX_IRQ_DEVS { return irq::IrqReturn::None; }
    let p = ISR_PTR[ctx].load(Acquire) as *const u8;
    if !p.is_null() {
        let isr = unsafe { p.mmio_read() };
        if isr & 1 == 0 { return irq::IrqReturn::None; }
    }
    VIRTIO_INPUT_IRQS.fetch_add(1, Relaxed);
    if let Some(mut inputs) = VIRTIO_INPUTS.try_lock() {
        for d in inputs.iter_mut() { d.poll(); }
    }
    irq::IrqReturn::Handled
}

/// Arm the event-queue interrupt: MSI-X (x86_64) or shared INTx SPI (aarch64),
/// exactly as `virtio_net::arm_rx_irq`. Failure leaves the device polled.
unsafe fn arm_irq(d: &VirtioKeyboardDevice) {
    use core::sync::atomic::Ordering::{Release, Relaxed};
    let slot = NEXT_SLOT.load(Relaxed);
    if slot >= MAX_IRQ_DEVS { return; }
    let pci = &d._pci_dev;
    #[cfg(target_arch = "x86_64")]
    if let Ok(tab) = irq::MsixTable::enable(pci.function()) {
        const NO_VECTOR: u16 = 0xFFFF;
        if tab.request(0, virtio_input_irq, slot).is_ok() {
            let cfg = d.common_cfg;
            core::ptr::addr_of_mut!((*cfg).config_msix_vector).write_volatile(NO_VECTOR);
            core::ptr::addr_of_mut!((*cfg).queue_select).write_volatile(0);
            core::ptr::addr_of_mut!((*cfg).queue_msix_vector).write_volatile(0);
            if core::ptr::addr_of!((*cfg).queue_msix_vector).read_volatile() == 0 {
                NEXT_SLOT.store(slot + 1, Relaxed);
                IRQ_ARMED.store(true, Relaxed);
                crate::kenv::pci::serial_debug("[INPUT] MSI-X armed\n");
                return;
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        let isr = crate::virtio::find_isr(pci);
        if isr.is_null() { return; }
        let pin = pci_read_config_8(pci.bus, pci.dev, pci.func, 0x3D);
        if pin == 0 || pin > 4 { return; }
        let Some(intid) = pci.function().intx_line() else { return };
        let _ = isr.mmio_read();
        ISR_PTR[slot].store(isr as usize, Release);
        if irq::request_irq(irq::IrqSource::Line(intid), virtio_input_irq, slot).is_err() {
            ISR_PTR[slot].store(0, Release);
            return;
        }
        NEXT_SLOT.store(slot + 1, Relaxed);
        let cmd = pci_read_config_16(pci.bus, pci.dev, pci.func, 0x04);
        pci_write_config_16(pci.bus, pci.dev, pci.func, 0x04, cmd & !crate::kenv::pci::PCI_CMD_INTX_DISABLE);
        let _ = isr.mmio_read();
        IRQ_ARMED.store(true, Relaxed);
        crate::kenv::pci::serial_debug("[INPUT] INTx armed\n");
    }
    let _ = (pci, Release);
}

pub fn init() {
    // Probe with the lock RELEASED. new_from() walks the PCI capability list,
    // maps each BAR (page-table work) and does MMIO config reads, and IRQs are
    // already unmasked by the time init() runs. The timer tick handler calls
    // poll_events(), which takes this same lock — so holding it across the
    // probe let any tick landing inside that window spin forever on a lock held
    // by the code it had just interrupted, deadlocking CPU 0 inside handle_irq.
    //
    // It only ever reproduced off the Apple Silicon host: under HVF the probe
    // fits comfortably between 10 ms ticks, while under cross-arch TCG
    // (aarch64 guest on an x86_64 box) it spans many emulated ticks and the
    // collision is near-certain. The window was always there.
    if !VIRTIO_INPUTS.lock().is_empty() { return; }

    // Bind every virtio-input PCI function (keyboard + tablet).
    let found = crate::kenv::pci::find_all_devices(VIRTIO_PCI_VENDOR, VIRTIO_PCI_DEVICE_INPUT);
    crate::kenv::pci::serial_debug("[INPUT] virtio-input functions found=");
    crate::kenv::pci::serial_debug_hex(found.len() as u32);
    crate::kenv::pci::serial_debug("\n");
    crate::kenv::input::register_stats_hook(stats_hook);
    let mut probed = alloc::vec::Vec::new();
    for dev in found {
        if let Some(d) = VirtioKeyboardDevice::new_from(dev) {
            probed.push(d);
        }
    }

    // Install in one shot. Re-check under the lock: a concurrent init() would
    // otherwise double-bind the same devices.
    let mut inputs = VIRTIO_INPUTS.lock();
    if inputs.is_empty() {
        *inputs = probed;
    }
    crate::kenv::pci::serial_debug("[INPUT] bound=");
    crate::kenv::pci::serial_debug_hex(inputs.len() as u32);
    crate::kenv::pci::serial_debug("\n");
}

pub fn poll_events() {
    // try_lock, never lock: this runs in IRQ context off the timer tick, so
    // blocking on a lock held by the task context we interrupted would wedge
    // the CPU (see init()). Missing one poll is harmless — the devices are
    // level-driven and the next tick, 10 ms later, drains them.
    if VQ_STATS { VQ_POLLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
    // Interrupt-driven: the tick poll is only a ~1 Hz safety net.
    if IRQ_ARMED.load(core::sync::atomic::Ordering::Relaxed)
        && POLL_DIV.fetch_add(1, core::sync::atomic::Ordering::Relaxed) % 100 != 0
    {
        return;
    }
    if let Some(mut inputs) = VIRTIO_INPUTS.try_lock() {
        let before = events_seen();
        for d in inputs.iter_mut() {
            d.poll();
        }
        if events_seen() != before { POLL_FOUND.fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
    } else if VQ_STATS {
        // A poll that never happened must not read as a poll that found
        // nothing: report the missed sample as missed.
        VQ_SKIPS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
}
