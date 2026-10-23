//! VirtIO entropy device (virtio-rng, device type 4) — PCI transport, polled.
//!
//! The device has one virtqueue (`requestq`) and no configuration space: the
//! driver posts a device-writable buffer and the device fills some prefix of
//! it with entropy from the host (QEMU's default backend is the host's
//! getrandom/`/dev/urandom`), reporting the length written in the used ring.
//! Virtio 1.2 §5.4.
//!
//! Why the kernel wants it. Under HVF on Apple Silicon (`-cpu host`) the guest
//! sees no FEAT_RNG, so the CSPRNG (`sched::random`) would be seeded from
//! timing jitter alone — and CNTVCT at 24 MHz gives weak jitter. virtio-rng is
//! the paravirtual answer every hypervisor offers. It is an *additional*
//! source: RDSEED/RDRAND/RNDR and jitter keep being mixed in, and hardware
//! without the device (Raspberry Pi 5, bare-metal x86) is unaffected — the
//! probe simply finds nothing and nothing is registered.
//!
//! Transport. Same as every other virtio device here: a PCI function on both
//! arches (x86_64 q35 and aarch64 `virt` ECAM). Modern ID 0x1044 when launched
//! with `disable-legacy=on` (what the launchers do), transitional 0x1005
//! otherwise; both are driven through the modern capability layout with
//! VIRTIO_F_VERSION_1 negotiated.
//!
//! Concurrency and latency. One descriptor, one DMA page, one outstanding
//! request at a time under a spinlock. A read waits for the used ring with a
//! wall-clock bound; QEMU completes the request from its main loop, so it is
//! asynchronous to the vCPU and can take a while on a loaded host. If the
//! bound expires the request is left in flight and the *next* read collects
//! it — the device keeps ownership of the buffer until then, so nothing is
//! ever reposted underneath it.

use spin::Mutex;
use crate::pci::{PciDevice, pci_read_config_8, pci_read_config_16, pci_read_config_32, pci_write_config_16};

const VIRTIO_PCI_VENDOR: u16 = 0x1af4;
const VIRTIO_PCI_DEVICE_RNG_MODERN: u16 = 0x1044; // 0x1040 + device type 4
const VIRTIO_PCI_DEVICE_RNG_LEGACY: u16 = 0x1005; // transitional

const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;

const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
const VIRTIO_STATUS_DRIVER: u8 = 2;
const VIRTIO_STATUS_DRIVER_OK: u8 = 4;
const VIRTIO_STATUS_FEATURES_OK: u8 = 8;
const VIRTIO_STATUS_FAILED: u8 = 128;

const VIRTQ_DESC_F_WRITE: u16 = 2;

/// Largest single request. The DMA buffer is one page; callers ask for far
/// less (a seed is 32–64 bytes).
pub const MAX_READ: usize = 4096;

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
struct VirtqDesc { addr: u64, len: u32, flags: u16, next: u16 }

#[repr(C, packed)]
struct VirtqUsedElem { id: u32, len: u32 }

struct VirtioRng {
    notify: *mut u16,
    qsize: u16,
    desc: *mut VirtqDesc,
    avail: *mut u8,
    used: *mut u8,
    buf_phys: usize,
    last_used: u16,
    in_flight: bool,
}

unsafe impl Send for VirtioRng {}

static DEVICE: Mutex<Option<VirtioRng>> = Mutex::new(None);

fn bar64(pci: &PciDevice, bar_idx: usize) -> u64 {
    let raw = pci.bars[bar_idx];
    if raw & 1 != 0 { return 0; }
    if (raw >> 1) & 3 == 2 && bar_idx + 1 < 6 {
        (raw & !0xF) as u64 | ((pci.bars[bar_idx + 1] as u64) << 32)
    } else {
        (raw & !0xF) as u64
    }
}

impl VirtioRng {
    unsafe fn probe(pci: PciDevice) -> Option<Self> {
        // Memory Space + Bus Master; INTx Disable because this driver polls
        // and never reads ISR status (see PCI_CMD_INTX_DISABLE).
        let cmd = pci_read_config_16(pci.bus, pci.dev, pci.func, 0x04);
        pci_write_config_16(pci.bus, pci.dev, pci.func, 0x04,
            (cmd | 0x0006) | crate::pci::PCI_CMD_INTX_DISABLE);

        let mut common: *mut VirtioPciCommonCfg = core::ptr::null_mut();
        let mut notify_base: usize = 0;
        let mut notify_mult: u32 = 0;
        let mut cap = pci_read_config_8(pci.bus, pci.dev, pci.func, 0x34);
        while cap != 0 {
            if pci_read_config_8(pci.bus, pci.dev, pci.func, cap) == 0x09 {
                let cfg_type = pci_read_config_8(pci.bus, pci.dev, pci.func, cap + 3);
                let bar_idx = pci_read_config_8(pci.bus, pci.dev, pci.func, cap + 4) as usize;
                let offset = pci_read_config_32(pci.bus, pci.dev, pci.func, cap + 8);
                let length = pci_read_config_32(pci.bus, pci.dev, pci.func, cap + 12);
                if bar_idx < 6 && matches!(cfg_type, VIRTIO_PCI_CAP_COMMON_CFG | VIRTIO_PCI_CAP_NOTIFY_CFG) {
                    let base = bar64(&pci, bar_idx);
                    if base != 0 {
                        let phys = base as usize + offset as usize;
                        let virt = mm::paging::map_kernel_device(
                            phys, length as usize,
                            mm::paging::PageFlags::PRESENT
                                | mm::paging::PageFlags::WRITABLE
                                | mm::paging::PageFlags::MMIO,
                        ).unwrap_or_else(|| mm::phys_to_virt(phys));
                        if cfg_type == VIRTIO_PCI_CAP_COMMON_CFG {
                            common = virt as *mut VirtioPciCommonCfg;
                        } else {
                            notify_base = virt;
                            notify_mult = pci_read_config_32(pci.bus, pci.dev, pci.func, cap + 16);
                        }
                    }
                }
            }
            cap = pci_read_config_8(pci.bus, pci.dev, pci.func, cap + 1);
        }
        if common.is_null() || notify_base == 0 {
            crate::pci::serial_debug("[RNG] virtio-rng: missing modern capabilities\n");
            return None;
        }

        let ds = core::ptr::addr_of_mut!((*common).device_status);
        ds.write_volatile(0);
        ds.write_volatile(VIRTIO_STATUS_ACKNOWLEDGE);
        ds.write_volatile(VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER);

        // No device-specific feature bits exist for entropy devices; only
        // VIRTIO_F_VERSION_1 (bit 32), which the modern transport requires.
        core::ptr::addr_of_mut!((*common).driver_feature_select).write_volatile(0);
        core::ptr::addr_of_mut!((*common).driver_feature).write_volatile(0);
        core::ptr::addr_of_mut!((*common).device_feature_select).write_volatile(1);
        let hi = core::ptr::addr_of!((*common).device_feature).read_volatile();
        core::ptr::addr_of_mut!((*common).driver_feature_select).write_volatile(1);
        core::ptr::addr_of_mut!((*common).driver_feature).write_volatile(hi & 1);
        ds.write_volatile(VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK);
        if ds.read_volatile() & VIRTIO_STATUS_FEATURES_OK == 0 {
            ds.write_volatile(VIRTIO_STATUS_FAILED);
            crate::pci::serial_debug("[RNG] virtio-rng: FEATURES_OK refused\n");
            return None;
        }

        // requestq (queue 0). Each ring gets its own zeroed page, as in the
        // other virtio drivers; one descriptor is all this driver ever uses.
        core::ptr::addr_of_mut!((*common).queue_select).write_volatile(0);
        let max = core::ptr::addr_of!((*common).queue_size).read_volatile();
        if max == 0 || max == 0xFFFF {
            ds.write_volatile(VIRTIO_STATUS_FAILED);
            return None;
        }
        let qsize = max.min(16);
        core::ptr::addr_of_mut!((*common).queue_size).write_volatile(qsize);
        let raw_noff = core::ptr::addr_of!((*common).queue_notify_off).read_volatile();
        let noff = if raw_noff == 0xFFFF { 0 } else { raw_noff };

        let desc_phys = mm::buddy::alloc(0)?;
        let avail_phys = mm::buddy::alloc(0)?;
        let used_phys = mm::buddy::alloc(0)?;
        let buf_phys = mm::buddy::alloc(0)?;
        for p in [desc_phys, avail_phys, used_phys, buf_phys] {
            core::ptr::write_bytes(mm::phys_to_virt(p) as *mut u8, 0, 4096);
        }
        core::ptr::addr_of_mut!((*common).queue_desc).write_volatile(desc_phys as u64);
        core::ptr::addr_of_mut!((*common).queue_driver).write_volatile(avail_phys as u64);
        core::ptr::addr_of_mut!((*common).queue_device).write_volatile(used_phys as u64);
        core::ptr::addr_of_mut!((*common).queue_enable).write_volatile(1u16);

        ds.write_volatile(VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER
            | VIRTIO_STATUS_FEATURES_OK | VIRTIO_STATUS_DRIVER_OK);

        Some(Self {
            notify: (notify_base + noff as usize * notify_mult as usize) as *mut u16,
            qsize,
            desc: mm::phys_to_virt(desc_phys) as *mut VirtqDesc,
            avail: mm::phys_to_virt(avail_phys) as *mut u8,
            used: mm::phys_to_virt(used_phys) as *mut u8,
            buf_phys,
            last_used: 0,
            in_flight: false,
        })
    }

    /// Post the whole buffer page (or `len` of it) to the device.
    unsafe fn post(&mut self, len: usize) {
        let d = &mut *self.desc;
        d.addr = self.buf_phys as u64;
        d.len = len as u32;
        d.flags = VIRTQ_DESC_F_WRITE;
        d.next = 0;
        let idx_ptr = self.avail.add(2) as *mut u16;
        let idx = idx_ptr.read_volatile();
        let ring = self.avail.add(4) as *mut u16;
        ring.add(idx as usize % self.qsize as usize).write_volatile(0);
        // Ring slot (and descriptor) before the index that publishes them.
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        idx_ptr.write_volatile(idx.wrapping_add(1));
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        self.notify.write_volatile(0);
        self.in_flight = true;
    }

    /// Collect a completed request, if any: returns the byte count written.
    unsafe fn collect(&mut self) -> Option<usize> {
        let used_idx = (self.used.add(2) as *const u16).read_volatile();
        if used_idx == self.last_used { return None; }
        // used.idx before the element it publishes.
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        let elems = self.used.add(4) as *const VirtqUsedElem;
        let e = elems.add(self.last_used as usize % self.qsize as usize).read_volatile();
        self.last_used = self.last_used.wrapping_add(1);
        self.in_flight = false;
        Some((e.len as usize).min(MAX_READ))
    }

    fn read(&mut self, out: &mut [u8], timeout_ns: u64) -> usize {
        let want = out.len().min(MAX_READ);
        if want == 0 { return 0; }
        unsafe {
            if !self.in_flight { self.post(want); }
            let start = sched::monotonic_ns();
            loop {
                if let Some(n) = self.collect() {
                    // A request left over from a timed-out read may have been
                    // sized differently; whatever the device wrote is entropy.
                    let n = n.min(want);
                    core::ptr::copy_nonoverlapping(
                        mm::phys_to_virt(self.buf_phys) as *const u8, out.as_mut_ptr(), n);
                    // Do not leave entropy that has been handed out lying in
                    // the DMA page.
                    core::ptr::write_bytes(mm::phys_to_virt(self.buf_phys) as *mut u8, 0, n);
                    return n;
                }
                if sched::monotonic_ns().wrapping_sub(start) >= timeout_ns { return 0; }
                core::hint::spin_loop();
            }
        }
    }
}

/// Upper bound on one read's wait for the device. Generous because the first
/// request is served at boot, possibly under cross-arch TCG on a busy host.
const READ_TIMEOUT_NS: u64 = 200_000_000;

/// Fill `out` from the device; returns the number of bytes written (0 if there
/// is no device, it is busy on another CPU, or it did not answer in time).
/// Never blocks on the lock: callers are the CSPRNG's seed paths, which must
/// not stall behind each other.
pub fn read(out: &mut [u8]) -> usize {
    let Some(mut g) = DEVICE.try_lock() else { return 0 };
    let Some(dev) = g.as_mut() else { return 0 };
    let mut got = 0;
    // The device may return fewer bytes than asked; a few rounds fill a seed.
    for _ in 0..4 {
        if got >= out.len() { break; }
        let n = dev.read(&mut out[got..], READ_TIMEOUT_NS);
        if n == 0 { break; }
        got += n;
    }
    got
}

/// Probe the PCI bus for a virtio-rng function and, if found, register it with
/// the kernel CSPRNG as an entropy source. Idempotent; a no-op on machines
/// without the device (or without PCI).
pub fn init() {
    if DEVICE.lock().is_some() { return; }
    let pci = crate::pci::find_device(VIRTIO_PCI_VENDOR, VIRTIO_PCI_DEVICE_RNG_MODERN)
        .or_else(|| crate::pci::find_device(VIRTIO_PCI_VENDOR, VIRTIO_PCI_DEVICE_RNG_LEGACY));
    let Some(pci) = pci else {
        crate::pci::serial_debug("[RNG] no virtio-rng device\n");
        return;
    };
    match unsafe { VirtioRng::probe(pci) } {
        Some(d) => {
            *DEVICE.lock() = Some(d);
            crate::pci::serial_debug("[RNG] virtio-rng bound\n");
            sched::random::register_source("virtio-rng", read);
        }
        None => crate::pci::serial_debug("[RNG] virtio-rng probe failed\n"),
    }
}
