//! VirtIO network device driver (PCI transport, polling mode: the net daemon
//! polls it at 1 kHz while traffic flows, see `netdev`). Every device found
//! is its own netif (`eth0`, `eth1`, ...). With VIRTIO_NET_F_STATUS the link
//! state is real (QEMU's monitor `set_link <netdev> off|on` flips it); without
//! it the link is always up.

use spin::Mutex;
use core::sync::atomic::Ordering;
// Kernel APIs come through `kenv` so the same source builds as a module (kmod-kapi shim).
use crate::kenv::pci::{PciDevice, find_all_devices, pci_read_config_8, pci_read_config_16, pci_read_config_32, pci_write_config_16};
use crate::kenv::{irq, mm, sched};
use crate::virtio::{bar64, find_isr};
use leandros_lib::mmio::{MmioRd, MmioWr};

const VIRTIO_PCI_VENDOR: u16 = 0x1af4;
const VIRTIO_NET_DEVICE_MODERN: u16 = 0x1041;
const VIRTIO_NET_DEVICE_LEGACY: u16 = 0x1000;

const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
const VIRTIO_STATUS_DRIVER: u8 = 2;
const VIRTIO_STATUS_FEATURES_OK: u8 = 8;
const VIRTIO_STATUS_DRIVER_OK: u8 = 4;

const VIRTQ_DESC_F_NEXT:  u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

// If offered, must be negotiated and accounted for in the virtio_net_hdr size
// (adds a trailing num_buffers: u16) — QEMU's virtio-net-pci uses the 12-byte
// mergeable-header format on the wire whenever it offers this bit, regardless
// of whether the driver "rejects" it, so a driver that assumes the basic
// 10-byte header without checking this ends up with every TX/RX frame
// misframed by exactly 2 bytes (found via packet capture: DHCP broadcasts
// left the guest with a corrupted destination MAC/ethertype).
const VIRTIO_NET_F_MRG_RXBUF: u32 = 1 << 15;
/// `status` in the device config is valid; bit 0 of it is VIRTIO_NET_S_LINK_UP.
const VIRTIO_NET_F_STATUS: u32 = 1 << 16;
/// Offset of `status` in `VirtioNetConfig`.
const NET_CFG_STATUS: usize = 6;
/// The link state is re-read from the device config at most this often (a
/// config read is an MMIO exit).
const LINK_POLL_NS: u64 = 50_000_000;

const MAX_NET_DEVICES: usize = 4;

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
pub struct VirtioNetConfig {
    pub mac: [u8; 6],
    pub status: u16,
    pub max_virtqueue_pairs: u16,
    pub mtu: u16,
}

#[repr(C, packed)]
struct VirtqDesc { addr: u64, len: u32, flags: u16, next: u16 }

#[repr(C, packed)]
struct VirtqAvail { flags: u16, idx: u16, ring: [u16; 32] }

#[repr(C, packed)]
struct VirtqUsedElem { id: u32, len: u32 }

#[repr(C, packed)]
struct VirtqUsed { flags: u16, idx: u16, ring: [VirtqUsedElem; 32] }

struct VirtQueue {
    size: u16,
    notify_off: u16,
    free_head: u16,
    num_free: u16,
    last_used_idx: u16,
    desc: *mut VirtqDesc,
    avail: *mut VirtqAvail,
    used: *mut VirtqUsed,
}

unsafe impl Send for VirtQueue {}
unsafe impl Sync for VirtQueue {}

impl VirtQueue {
    unsafe fn alloc(id: u16, cfg: *mut VirtioPciCommonCfg) -> Option<Self> {
        core::ptr::addr_of_mut!((*cfg).queue_select).mmio_write(id);
        let reported = core::ptr::addr_of!((*cfg).queue_size).mmio_read();
        if reported == 0 { return None; }

        const MAX_QUEUE: u16 = 256;
        let size = if reported > MAX_QUEUE { MAX_QUEUE } else { reported };
        if size != reported {
            core::ptr::addr_of_mut!((*cfg).queue_size).mmio_write(size);
        }
        let raw_notify_off = core::ptr::addr_of!((*cfg).queue_notify_off).mmio_read();
        let notify_off = if raw_notify_off == 0xFFFF { 0 } else { raw_notify_off };

        let desc_phys = mm::buddy::alloc(0)?;
        let avail_phys = mm::buddy::alloc(0)?;
        let used_phys  = mm::buddy::alloc(0)?;

        let desc = mm::phys_to_virt(desc_phys) as *mut VirtqDesc;
        let avail = mm::phys_to_virt(avail_phys) as *mut VirtqAvail;
        let used  = mm::phys_to_virt(used_phys) as *mut VirtqUsed;

        core::ptr::write_bytes(desc  as *mut u8, 0, 4096);
        core::ptr::write_bytes(avail as *mut u8, 0, 4096);
        core::ptr::write_bytes(used  as *mut u8, 0, 4096);

        for i in 0..(size - 1) { (*desc.add(i as usize)).next = i + 1; }
        (*desc.add((size - 1) as usize)).next = 0xFFFF;

        core::ptr::addr_of_mut!((*cfg).queue_desc).mmio_write(desc_phys as u64);
        core::ptr::addr_of_mut!((*cfg).queue_driver).mmio_write(avail_phys as u64);
        core::ptr::addr_of_mut!((*cfg).queue_device).mmio_write(used_phys as u64);
        core::ptr::addr_of_mut!((*cfg).queue_enable).mmio_write(1u16);

        Some(Self { size, notify_off, free_head: 0, num_free: size, last_used_idx: 0, desc, avail, used })
    }

    unsafe fn alloc_desc(&mut self, addr: u64, len: u32, flags: u16) -> u16 {
        let id = self.free_head;
        let d = self.desc.add(id as usize);
        self.free_head = (*d).next;
        self.num_free -= 1;
        (*d).addr = addr; (*d).len = len; (*d).flags = flags; (*d).next = 0;
        id
    }

    unsafe fn chain(&mut self, from: u16, to: u16) {
        let d = self.desc.add(from as usize);
        (*d).flags |= VIRTQ_DESC_F_NEXT;
        (*d).next = to;
    }

    unsafe fn submit(&mut self, head: u16) {
        let a = self.avail;
        let idx_ptr = core::ptr::addr_of_mut!((*a).idx);
        let cur_idx = idx_ptr.read_volatile();
        let ring_slot = cur_idx as usize % self.size as usize;
        let ring_ptr = (a as usize + 4) as *mut u16;
        ring_ptr.add(ring_slot).write_volatile(head);

        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        idx_ptr.write_volatile(cur_idx.wrapping_add(1));
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    }

    unsafe fn free_chain(&mut self, mut head: u16) {
        loop {
            let d = self.desc.add(head as usize);
            let flags = (*d).flags;
            let next = (*d).next;
            (*d).next = self.free_head;
            self.free_head = head;
            self.num_free += 1;
            if (flags & VIRTQ_DESC_F_NEXT) != 0 { head = next; } else { break; }
        }
    }

}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
struct VirtioNetHdr {
    flags: u8,
    gso_type: u8,
    hdr_len: u16,
    gso_size: u16,
    csum_start: u16,
    csum_offset: u16,
    num_buffers: u16, // only present on the wire when mrg_rxbuf is negotiated
}

struct VirtioNetDevice {
    _pci: PciDevice,
    common_cfg: *mut VirtioPciCommonCfg,
    notify_cfg: *mut u32,
    notify_off_multiplier: u32,
    _device_cfg: *mut VirtioNetConfig,
    rx_queue: VirtQueue,
    tx_queue: VirtQueue,
    mac: [u8; 6],
    hdr_len: usize,
    /// VIRTIO_NET_F_STATUS was negotiated: `link` follows the device.
    has_status: bool,
    /// Link state as last read, the time it was read, and the number of
    /// down-to-up transitions seen (the netif's link generation).
    link: bool,
    link_read_ns: u64,
    link_gen: u32,

    /// One 4 KiB page per RX descriptor, indexed by descriptor id.
    rx_buffers_phys: [usize; RX_BUFS],
    /// RX descriptors handed back since the last notify (batched: one
    /// doorbell per `RX_NOTIFY_BATCH` frames or per drained ring).
    rx_unnotified: u16,
    /// `TX_SLOTS` copy buffers of `TX_SLOT_BYTES` (one order-4 block).
    tx_req_phys: usize,
    /// Free TX slots (bit i = slot i).
    tx_free: u32,
    /// The TX slot a descriptor id carries (valid while in flight).
    tx_desc_slot: [u8; 256],
}

/// Frames in flight on the TX queue. `send_packet` returns once the frame is
/// queued; its slot is reclaimed from the used ring by a later send (no TX
/// interrupt, no wait per frame). Only a full window waits for the device.
const TX_SLOTS: usize = 32;
const TX_SLOT_BYTES: usize = 2048;

/// RX descriptors posted. It used to be 16: a 10 ms poll could then take at
/// most 16 frames (24 KB) per pass from the device, about 2.4 MB/s, and QEMU
/// held the rest back.
const RX_BUFS: usize = 256;
const RX_NOTIFY_BATCH: u16 = 32;

unsafe impl Send for VirtioNetDevice {}
unsafe impl Sync for VirtioNetDevice {}

unsafe fn enable_pci_mmio(pci: &PciDevice) {
    let cmd = pci_read_config_16(pci.bus, pci.dev, pci.func, 0x04);
    // Memory Space + Bus Master on; INTx Disable (bit 10) on: this driver
    // polls and never reads its ISR status, so an asserted INTx would stay
    // asserted forever and, on the virt board's shared PCIe lines, storm the
    // GPU's completion interrupt (`virtio_gpu::enable_intx`).
    pci_write_config_16(pci.bus, pci.dev, pci.func, 0x04, (cmd | 0x0006) | crate::kenv::pci::PCI_CMD_INTX_DISABLE);
}

unsafe fn walk_caps(
    pci: &PciDevice,
) -> (Option<*mut VirtioPciCommonCfg>, Option<*mut u32>, u32, Option<*mut VirtioNetConfig>) {
    let mut common_cfg: Option<*mut VirtioPciCommonCfg> = None;
    let mut notify_cfg: Option<*mut u32> = None;
    let mut notify_off_multiplier = 0u32;
    let mut device_cfg: Option<*mut VirtioNetConfig> = None;

    let mut cap_ptr = pci_read_config_8(pci.bus, pci.dev, pci.func, 0x34);
    while cap_ptr != 0 {
        let cap_id = pci_read_config_8(pci.bus, pci.dev, pci.func, cap_ptr);
        if cap_id == 0x09 {
            let cfg_type = pci_read_config_8(pci.bus, pci.dev, pci.func, cap_ptr + 3);
            let bar_idx  = pci_read_config_8(pci.bus, pci.dev, pci.func, cap_ptr + 4) as usize;
            let offset   = pci_read_config_32(pci.bus, pci.dev, pci.func, cap_ptr + 8);
            let length   = pci_read_config_32(pci.bus, pci.dev, pci.func, cap_ptr + 12);

            if bar_idx < 6 {
                let base = bar64(pci, bar_idx);
                if base != 0 {
                    let phys = base as usize + offset as usize;
                    let virt = mm::paging::map_kernel_device(
                        phys, length as usize,
                        mm::paging::PageFlags::PRESENT
                            | mm::paging::PageFlags::WRITABLE
                            | mm::paging::PageFlags::MMIO,
                    ).unwrap_or_else(|| mm::phys_to_virt(phys));

                    match cfg_type {
                        VIRTIO_PCI_CAP_COMMON_CFG => {
                            common_cfg = Some(virt as *mut VirtioPciCommonCfg);
                        }
                        VIRTIO_PCI_CAP_NOTIFY_CFG => {
                            notify_off_multiplier =
                                pci_read_config_32(pci.bus, pci.dev, pci.func, cap_ptr + 16);
                            notify_cfg = Some(virt as *mut u32);
                        }
                        VIRTIO_PCI_CAP_DEVICE_CFG => {
                            device_cfg = Some(virt as *mut VirtioNetConfig);
                        }
                        _ => {}
                    }
                }
            }
        }
        cap_ptr = pci_read_config_8(pci.bus, pci.dev, pci.func, cap_ptr + 1);
    }

    (common_cfg, notify_cfg, notify_off_multiplier, device_cfg)
}

unsafe fn pre_reset_device(pci: &PciDevice) {
    enable_pci_mmio(pci);
    let (common_cfg, _, _, _) = walk_caps(pci);
    if let Some(cfg) = common_cfg {
        core::ptr::addr_of_mut!((*cfg).device_status).mmio_write(0u8);
    }
}

impl VirtioNetDevice {
    unsafe fn new(pci: PciDevice, reset_done: bool) -> Option<Self> {
        enable_pci_mmio(&pci);
        let (common_cfg, notify_cfg, notify_off_multiplier, device_cfg) = walk_caps(&pci);
        let common_cfg = common_cfg?;
        let notify_cfg = notify_cfg?;
        let device_cfg = device_cfg?;

        let ds = core::ptr::addr_of_mut!((*common_cfg).device_status);

        if !reset_done {
            ds.mmio_write(0u8);
        }
        ds.mmio_write(VIRTIO_STATUS_ACKNOWLEDGE);
        ds.mmio_write(VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER);

        core::ptr::addr_of_mut!((*common_cfg).device_feature_select).mmio_write(0u32);
        let dev_feat_lo = core::ptr::addr_of!((*common_cfg).device_feature).mmio_read();
        let mrg_rxbuf = dev_feat_lo & VIRTIO_NET_F_MRG_RXBUF != 0;
        let has_status = dev_feat_lo & VIRTIO_NET_F_STATUS != 0;
        core::ptr::addr_of_mut!((*common_cfg).driver_feature_select).mmio_write(0u32);
        core::ptr::addr_of_mut!((*common_cfg).driver_feature).mmio_write(
            (if mrg_rxbuf { VIRTIO_NET_F_MRG_RXBUF } else { 0 })
                | (if has_status { VIRTIO_NET_F_STATUS } else { 0 })
        );

        core::ptr::addr_of_mut!((*common_cfg).device_feature_select).mmio_write(1u32);
        let dev_feat_hi = core::ptr::addr_of!((*common_cfg).device_feature).mmio_read();
        const VIRTIO_F_VERSION_1: u32 = 1 << 0;
        if dev_feat_hi & VIRTIO_F_VERSION_1 != 0 {
            core::ptr::addr_of_mut!((*common_cfg).driver_feature_select).mmio_write(1u32);
            core::ptr::addr_of_mut!((*common_cfg).driver_feature).mmio_write(VIRTIO_F_VERSION_1);
        }

        ds.mmio_write(
            VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK,
        );
        if ds.mmio_read() & VIRTIO_STATUS_FEATURES_OK == 0 {
            return None;
        }

        let rx_queue = VirtQueue::alloc(0, common_cfg)?;
        let tx_queue = VirtQueue::alloc(1, common_cfg)?;

        ds.mmio_write(
            VIRTIO_STATUS_ACKNOWLEDGE
                | VIRTIO_STATUS_DRIVER
                | VIRTIO_STATUS_FEATURES_OK
                | VIRTIO_STATUS_DRIVER_OK,
        );

        let mac = (*device_cfg).mac;

        let nbufs = (rx_queue.size as usize).min(RX_BUFS);
        let mut rx_buffers_phys = [0usize; RX_BUFS];
        for i in 0..nbufs {
            let phys = mm::buddy::alloc(0)?;
            core::ptr::write_bytes(mm::phys_to_virt(phys) as *mut u8, 0, 4096);
            rx_buffers_phys[i] = phys;
        }

        let mut dev = Self {
            _pci: pci,
            common_cfg,
            notify_cfg,
            notify_off_multiplier,
            _device_cfg: device_cfg,
            rx_queue,
            tx_queue,
            mac,
            hdr_len: if mrg_rxbuf { 12 } else { 10 },
            has_status,
            link: true,
            link_read_ns: 0,
            link_gen: 0,
            rx_buffers_phys,
            rx_unnotified: 0,
            tx_req_phys: mm::buddy::alloc(4)?,
            tx_free: u32::MAX,
            tx_desc_slot: [0; 256],
        };

        for i in 0..nbufs {
            let phys = dev.rx_buffers_phys[i];
            let d = dev.rx_queue.alloc_desc(phys as u64, 4096, VIRTQ_DESC_F_WRITE);
            dev.rx_queue.submit(d);
        }
        dev.notify_rx();

        crate::kenv::serial_raw("[NET] VirtIO Net device initialized successfully\r\n");

        Some(dev)
    }

    unsafe fn notify_queue(notify_cfg: *mut u32, notify_off: u16, multiplier: u32) {
        let notify_addr = (notify_cfg as usize
            + notify_off as usize * multiplier as usize)
            as *mut u16;
        notify_addr.mmio_write(0);
    }

    /// The link state, re-read from the device at most every `LINK_POLL_NS`.
    fn link_up(&mut self) -> bool {
        if !self.has_status { return true; }
        let now = sched::monotonic_ns();
        if self.link_read_ns != 0 && now.saturating_sub(self.link_read_ns) < LINK_POLL_NS {
            return self.link;
        }
        self.link_read_ns = now.max(1);
        let st = unsafe { (self._device_cfg as *const u8).add(NET_CFG_STATUS).mmio_read() };
        let up = st & 1 != 0;
        if up && !self.link { self.link_gen = self.link_gen.wrapping_add(1); }
        self.link = up;
        up
    }

    fn notify_rx(&self) {
        unsafe {
            Self::notify_queue(self.notify_cfg, self.rx_queue.notify_off, self.notify_off_multiplier);
        }
    }

    fn notify_tx(&self) {
        unsafe {
            Self::notify_queue(self.notify_cfg, self.tx_queue.notify_off, self.notify_off_multiplier);
        }
    }

    /// Return every TX slot the device has finished with. Returns how many.
    fn reclaim_tx(&mut self) -> usize {
        let mut n = 0;
        unsafe {
            let q = &mut self.tx_queue;
            loop {
                let used_idx = core::ptr::addr_of!((*q.used).idx).read_volatile();
                if q.last_used_idx == used_idx { break; }
                core::sync::atomic::fence(Ordering::Acquire);
                let slot = q.last_used_idx as usize % q.size as usize;
                let elem = (q.used as usize + 4 + slot * 8) as *const VirtqUsedElem;
                let id = core::ptr::addr_of!((*elem).id).read_volatile() as u16;
                q.free_chain(id);
                if (id as usize) < self.tx_desc_slot.len() {
                    self.tx_free |= 1 << self.tx_desc_slot[id as usize];
                }
                q.last_used_idx = q.last_used_idx.wrapping_add(1);
                n += 1;
            }
        }
        n
    }

    fn send_packet(&mut self, buf: &[u8]) -> bool {
        if buf.len() > 1514 || self.hdr_len + buf.len() > TX_SLOT_BYTES {
            return false;
        }
        self.reclaim_tx();
        let nslots = (self.tx_queue.size as usize).min(TX_SLOTS);
        let window = ((1u64 << nslots) - 1) as u32;
        while self.tx_free & window == 0 || self.tx_queue.num_free == 0 {
            // Every slot in flight: the device is behind; wait for one.
            if self.reclaim_tx() == 0 { core::hint::spin_loop(); }
        }
        let slot = (self.tx_free & window).trailing_zeros() as usize;
        self.tx_free &= !(1 << slot);
        unsafe {
            let phys = self.tx_req_phys + slot * TX_SLOT_BYTES;
            let req_virt = mm::phys_to_virt(phys) as *mut u8;
            core::ptr::write(req_virt as *mut VirtioNetHdr, VirtioNetHdr::default());
            core::ptr::copy_nonoverlapping(buf.as_ptr(), req_virt.add(self.hdr_len), buf.len());
            let q = &mut self.tx_queue;
            let d = q.alloc_desc(phys as u64, (self.hdr_len + buf.len()) as u32, 0);
            if (d as usize) < self.tx_desc_slot.len() { self.tx_desc_slot[d as usize] = slot as u8; }
            q.submit(d);
            self.notify_tx();
            true
        }
    }

    fn poll_receive(&mut self, packet_buf: &mut [u8]) -> Option<usize> {
        unsafe {
            let q = &mut self.rx_queue;
            let used_idx = core::ptr::addr_of!((*q.used).idx).read_volatile();
            if q.last_used_idx == used_idx {
                self.flush_rx_notify();
                return None;
            }
            // The used-ring entry is read only after its index was seen.
            core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);

            let slot = q.last_used_idx as usize % q.size as usize;
            // The VirtqUsed overlay declares ring[32] but the real ring has
            // q.size (up to 256) entries; index via raw pointer like submit()
            // does, or slot >= 32 trips the array bounds check and panics.
            let elem_ptr = (q.used as usize + 4 + slot * 8) as *const VirtqUsedElem;
            let desc_id = core::ptr::addr_of!((*elem_ptr).id).read_volatile() as usize;
            let len = core::ptr::addr_of!((*elem_ptr).len).read_volatile() as usize;

            let page_virt = mm::phys_to_virt(self.rx_buffers_phys[desc_id]) as *const u8;
            let got = if len > self.hdr_len {
                let payload_len = len - self.hdr_len;
                let copy_len = payload_len.min(packet_buf.len());
                core::ptr::copy_nonoverlapping(
                    page_virt.add(self.hdr_len),
                    packet_buf.as_mut_ptr(),
                    copy_len,
                );
                Some(copy_len)
            } else {
                None
            };
            // The buffer goes straight back to the device; the doorbell is
            // batched (QEMU picks reposted buffers up on the next notify).
            q.submit(desc_id as u16);
            q.last_used_idx = q.last_used_idx.wrapping_add(1);
            self.rx_unnotified += 1;
            if self.rx_unnotified >= RX_NOTIFY_BATCH { self.flush_rx_notify(); }
            got
        }
    }

    fn flush_rx_notify(&mut self) {
        if self.rx_unnotified > 0 {
            self.rx_unnotified = 0;
            self.notify_rx();
        }
    }

    /// Flush the deferred RX doorbell; true when a frame is already waiting.
    fn rx_rearm(&mut self) -> bool {
        self.flush_rx_notify();
        let q = &self.rx_queue;
        let used_idx = unsafe { core::ptr::addr_of!((*q.used).idx).read_volatile() };
        used_idx != q.last_used_idx
    }
}

static DEVICES: Mutex<[Option<VirtioNetDevice>; MAX_NET_DEVICES]> =
    Mutex::new([const { None }; MAX_NET_DEVICES]);

static DEVICE_COUNT: Mutex<usize> = Mutex::new(0);

/// Per-device ISR status byte address (0 = MSI-X armed or no IRQ), for the
/// handler, which must not take the device lock.
static ISR_PTR: [core::sync::atomic::AtomicUsize; MAX_NET_DEVICES] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; MAX_NET_DEVICES];
/// RX interrupts taken per device (diagnostics).
static RX_IRQS: [core::sync::atomic::AtomicU64; MAX_NET_DEVICES] =
    [const { core::sync::atomic::AtomicU64::new(0) }; MAX_NET_DEVICES];

/// RX interrupt: wake the net daemon. `ctx` is the device index. With INTx the
/// ISR status byte is read first (the read deasserts the level line); a
/// shared line whose condition is not ours is `NotHandled`.
extern "C" fn virtio_net_irq(ctx: usize) -> irq::IrqReturn {
    use core::sync::atomic::Ordering::Relaxed;
    if ctx >= MAX_NET_DEVICES { return irq::IrqReturn::None; }
    let p = ISR_PTR[ctx].load(Ordering::Acquire) as *const u8;
    if !p.is_null() {
        let isr = unsafe { p.mmio_read() };
        // Bit 0: a queue; bit 1: a device config change (link status).
        if isr & 3 == 0 { return irq::IrqReturn::None; }
    }
    if RX_IRQS[ctx].fetch_add(1, Relaxed) == 0 {
        crate::kenv::pci::serial_debug("[NET] first RX interrupt\n");
    }
    crate::kenv::netdev::kick_from_irq();
    irq::IrqReturn::Handled
}

/// Arm the RX-queue interrupt for device `idx`: MSI-X when available (x86_64),
/// else INTx through a shared GIC line (aarch64). Any failure leaves the
/// device polled, which stays the fallback either way. TX slots are reclaimed
/// by the next send, so the TX queue asks for no interrupt.
unsafe fn arm_rx_irq(idx: usize, d: &VirtioNetDevice) {
    if idx >= MAX_NET_DEVICES { return; }
    core::ptr::addr_of_mut!((*d.tx_queue.avail).flags).write_volatile(1); // NO_INTERRUPT
    let pci = &d._pci;
    #[cfg(target_arch = "x86_64")]
    if let Ok(tab) = irq::MsixTable::enable(pci.function()) {
        const NO_VECTOR: u16 = 0xFFFF;
        if let Ok(h) = tab.request(0, virtio_net_irq, idx) {
            let cfg = d.common_cfg;
            // Config changes (link status) share vector table entry 0.
            core::ptr::addr_of_mut!((*cfg).config_msix_vector).mmio_write(0);
            core::ptr::addr_of_mut!((*cfg).queue_select).mmio_write(1);
            core::ptr::addr_of_mut!((*cfg).queue_msix_vector).mmio_write(NO_VECTOR);
            core::ptr::addr_of_mut!((*cfg).queue_select).mmio_write(0);
            core::ptr::addr_of_mut!((*cfg).queue_msix_vector).mmio_write(0);
            if core::ptr::addr_of!((*cfg).queue_msix_vector).mmio_read() == 0 {
                IRQ_ARMED[idx].store(true, Ordering::Release);
                crate::kenv::pci::serial_debug("[NET] MSI-X armed: RX queue -> vector ");
                crate::kenv::pci::serial_debug_hex(h.vector().unwrap_or(0) as u32);
                crate::kenv::pci::serial_debug("\n");
                return;
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        // QEMU virt: virtio-pci INTA..D are SPIs 3..6 (INTID 35..38),
        // swizzled by slot and pin, like `virtio_gpu::enable_intx`.
        let isr = find_isr(pci);
        if isr.is_null() { return; }
        let pin = pci_read_config_8(pci.bus, pci.dev, pci.func, 0x3D);
        if pin == 0 || pin > 4 { return; }
        let Some(intid) = pci.function().intx_line() else { return };
        // Publish the ISR pointer, drain stale state, register, only then
        // lift INTx Disable.
        let _ = isr.mmio_read();
        ISR_PTR[idx].store(isr as usize, Ordering::Release);
        if irq::request_irq(irq::IrqSource::Line(intid), virtio_net_irq, idx).is_err() {
            ISR_PTR[idx].store(0, Ordering::Release);
            crate::kenv::pci::serial_debug("[NET] INTx: line registration refused; polling only\n");
            return;
        }
        let cmd = pci_read_config_16(pci.bus, pci.dev, pci.func, 0x04);
        pci_write_config_16(pci.bus, pci.dev, pci.func, 0x04, cmd & !crate::kenv::pci::PCI_CMD_INTX_DISABLE);
        let _ = isr.mmio_read();
        IRQ_ARMED[idx].store(true, Ordering::Release);
        crate::kenv::pci::serial_debug("[NET] INTx armed: INTID ");
        crate::kenv::pci::serial_debug_hex(intid);
        crate::kenv::pci::serial_debug("\n");
    }
    let _ = pci;
}

pub fn init() {
    for device_id in [VIRTIO_NET_DEVICE_MODERN, VIRTIO_NET_DEVICE_LEGACY] {
        for pci in find_all_devices(VIRTIO_PCI_VENDOR, device_id) {
            unsafe { pre_reset_device(&pci); }
        }
    }

    let mut found = 0usize;
    let mut devs = DEVICES.lock();
    let mut cnt = DEVICE_COUNT.lock();

    for device_id in [VIRTIO_NET_DEVICE_MODERN, VIRTIO_NET_DEVICE_LEGACY] {
        for pci in find_all_devices(VIRTIO_PCI_VENDOR, device_id) {
            if found >= MAX_NET_DEVICES { break; }
            unsafe {
                if let Some(d) = VirtioNetDevice::new(pci, true) {
                    arm_rx_irq(found, &d);
                    devs[found] = Some(d);
                    found += 1;
                }
            }
        }
        if found >= MAX_NET_DEVICES { break; }
    }

    *cnt = found;
}

/// Link state of device `dev_idx` (always up without VIRTIO_NET_F_STATUS).
pub fn link_up(dev_idx: usize) -> bool {
    let mut devs = DEVICES.lock();
    match devs.get_mut(dev_idx) { Some(Some(d)) => d.link_up(), _ => false }
}

/// Down-to-up transitions of device `dev_idx`'s link (the net server
/// restarts DHCP when it changes).
pub fn link_generation(dev_idx: usize) -> u32 {
    let mut devs = DEVICES.lock();
    match devs.get_mut(dev_idx) { Some(Some(d)) => { d.link_up(); d.link_gen } _ => 0 }
}

pub fn device_count() -> usize {
    *DEVICE_COUNT.lock()
}

pub fn get_mac_address(dev_idx: usize) -> Option<[u8; 6]> {
    let devs = DEVICES.lock();
    devs[dev_idx].as_ref().map(|d| d.mac)
}

pub fn send_packet(dev_idx: usize, buf: &[u8]) -> bool {
    let mut devs = DEVICES.lock();
    if let Some(ref mut d) = devs[dev_idx] {
        d.send_packet(buf)
    } else {
        false
    }
}

/// The RX-queue interrupt of each device is registered and unmasked: every
/// received frame kicks the net daemon, which may then sleep long when idle.
static IRQ_ARMED: [core::sync::atomic::AtomicBool; MAX_NET_DEVICES] =
    [const { core::sync::atomic::AtomicBool::new(false) }; MAX_NET_DEVICES];

pub fn rx_irq_armed(dev_idx: usize) -> bool {
    dev_idx < MAX_NET_DEVICES && IRQ_ARMED[dev_idx].load(Ordering::Acquire)
}

/// RX interrupts taken by device `dev_idx` (0 when polling only).
pub fn rx_irq_count(dev_idx: usize) -> u64 {
    if dev_idx >= MAX_NET_DEVICES { return 0; }
    RX_IRQS[dev_idx].load(core::sync::atomic::Ordering::Relaxed)
}

pub fn rx_rearm(dev_idx: usize) -> bool {
    let mut devs = DEVICES.lock();
    match devs[dev_idx] { Some(ref mut d) => d.rx_rearm(), None => false }
}

pub fn poll_receive(dev_idx: usize, buf: &mut [u8]) -> Option<usize> {
    let mut devs = DEVICES.lock();
    if let Some(ref mut d) = devs[dev_idx] {
        d.poll_receive(buf)
    } else {
        None
    }
}

/// Module unload: reset every device (it stops DMA and interrupts), then free the RX buffers and
/// the TX copy block. Ring pages stay owned (the kernel logs them as leftovers). A device that does
/// not confirm the reset keeps all its pages.
#[allow(dead_code)]
pub fn shutdown() {
    let mut devs = DEVICES.lock();
    for i in 0..MAX_NET_DEVICES {
        IRQ_ARMED[i].store(false, Ordering::Release);
        ISR_PTR[i].store(0, Ordering::Release);
        let Some(d) = devs[i].take() else { continue };
        unsafe {
            let ds = core::ptr::addr_of_mut!((*d.common_cfg).device_status);
            ds.mmio_write(0);
            let end = crate::kenv::sched::monotonic_ns() + 100_000_000;
            let mut ok = true;
            while ds.mmio_read() != 0 {
                if crate::kenv::sched::monotonic_ns() >= end { ok = false; break; }
                core::hint::spin_loop();
            }
            if !ok { continue; }
            for &p in d.rx_buffers_phys.iter() { if p != 0 { mm::buddy::free(p, 0); } }
            mm::buddy::free(d.tx_req_phys, 4);
        }
    }
    *DEVICE_COUNT.lock() = 0;
}
