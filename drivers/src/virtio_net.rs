//! VirtIO network device driver (PCI transport, polling mode).

use spin::Mutex;
use crate::pci::{PciDevice, find_all_devices, pci_read_config_8, pci_read_config_16, pci_read_config_32, pci_write_config_16};
use mm;

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
        core::ptr::addr_of_mut!((*cfg).queue_select).write_volatile(id);
        let reported = core::ptr::addr_of!((*cfg).queue_size).read_volatile();
        if reported == 0 { return None; }

        const MAX_QUEUE: u16 = 256;
        let size = if reported > MAX_QUEUE { MAX_QUEUE } else { reported };
        if size != reported {
            core::ptr::addr_of_mut!((*cfg).queue_size).write_volatile(size);
        }
        let raw_notify_off = core::ptr::addr_of!((*cfg).queue_notify_off).read_volatile();
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

        core::ptr::addr_of_mut!((*cfg).queue_desc).write_volatile(desc_phys as u64);
        core::ptr::addr_of_mut!((*cfg).queue_driver).write_volatile(avail_phys as u64);
        core::ptr::addr_of_mut!((*cfg).queue_device).write_volatile(used_phys as u64);
        core::ptr::addr_of_mut!((*cfg).queue_enable).write_volatile(1u16);

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

    unsafe fn wait_for_completion(&mut self) {
        while self.last_used_idx == core::ptr::addr_of!((*self.used).idx).read_volatile() {
            core::hint::spin_loop();
        }
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
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

    rx_buffers_phys: [usize; 16],
    tx_req_phys: usize,
}

unsafe impl Send for VirtioNetDevice {}
unsafe impl Sync for VirtioNetDevice {}

unsafe fn enable_pci_mmio(pci: &PciDevice) {
    let cmd = pci_read_config_16(pci.bus, pci.dev, pci.func, 0x04);
    pci_write_config_16(pci.bus, pci.dev, pci.func, 0x04, cmd | 0x0006);
}

fn bar64(pci: &PciDevice, bar_idx: usize) -> u64 {
    let raw = pci.bars[bar_idx];
    if raw & 1 != 0 { return 0; }
    if (raw >> 1) & 3 == 2 && bar_idx + 1 < 6 {
        (raw & !0xF) as u64 | ((pci.bars[bar_idx + 1] as u64) << 32)
    } else {
        (raw & !0xF) as u64
    }
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
        core::ptr::addr_of_mut!((*cfg).device_status).write_volatile(0u8);
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
            ds.write_volatile(0u8);
        }
        ds.write_volatile(VIRTIO_STATUS_ACKNOWLEDGE);
        ds.write_volatile(VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER);

        core::ptr::addr_of_mut!((*common_cfg).device_feature_select).write_volatile(0u32);
        let dev_feat_lo = core::ptr::addr_of!((*common_cfg).device_feature).read_volatile();
        let mrg_rxbuf = dev_feat_lo & VIRTIO_NET_F_MRG_RXBUF != 0;
        core::ptr::addr_of_mut!((*common_cfg).driver_feature_select).write_volatile(0u32);
        core::ptr::addr_of_mut!((*common_cfg).driver_feature).write_volatile(
            if mrg_rxbuf { VIRTIO_NET_F_MRG_RXBUF } else { 0 }
        );

        core::ptr::addr_of_mut!((*common_cfg).device_feature_select).write_volatile(1u32);
        let dev_feat_hi = core::ptr::addr_of!((*common_cfg).device_feature).read_volatile();
        const VIRTIO_F_VERSION_1: u32 = 1 << 0;
        if dev_feat_hi & VIRTIO_F_VERSION_1 != 0 {
            core::ptr::addr_of_mut!((*common_cfg).driver_feature_select).write_volatile(1u32);
            core::ptr::addr_of_mut!((*common_cfg).driver_feature).write_volatile(VIRTIO_F_VERSION_1);
        }

        ds.write_volatile(
            VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK,
        );
        if ds.read_volatile() & VIRTIO_STATUS_FEATURES_OK == 0 {
            return None;
        }

        let rx_queue = VirtQueue::alloc(0, common_cfg)?;
        let tx_queue = VirtQueue::alloc(1, common_cfg)?;

        ds.write_volatile(
            VIRTIO_STATUS_ACKNOWLEDGE
                | VIRTIO_STATUS_DRIVER
                | VIRTIO_STATUS_FEATURES_OK
                | VIRTIO_STATUS_DRIVER_OK,
        );

        let mac = (*device_cfg).mac;

        let mut rx_buffers_phys = [0usize; 16];
        for i in 0..16 {
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
            rx_buffers_phys,
            tx_req_phys: mm::buddy::alloc(0)?,
        };

        for i in 0..16 {
            let phys = dev.rx_buffers_phys[i];
            let d = dev.rx_queue.alloc_desc(phys as u64, 4096, VIRTQ_DESC_F_WRITE);
            dev.rx_queue.submit(d);
        }
        dev.notify_rx();

        extern "C" { fn arch_serial_putc(b: u8); }
        let msg = b"[NET] VirtIO Net device initialized successfully\r\n";
        for &b in msg { unsafe { arch_serial_putc(b); } }

        Some(dev)
    }

    unsafe fn notify_queue(notify_cfg: *mut u32, notify_off: u16, multiplier: u32) {
        let notify_addr = (notify_cfg as usize
            + notify_off as usize * multiplier as usize)
            as *mut u16;
        core::ptr::write_volatile(notify_addr, 0);
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

    fn send_packet(&mut self, buf: &[u8]) -> bool {
        if buf.len() > 1514 {
            return false;
        }
        unsafe {
            let req_virt = mm::phys_to_virt(self.tx_req_phys) as *mut u8;
            let hdr = VirtioNetHdr::default();
            core::ptr::write(req_virt as *mut VirtioNetHdr, hdr);

            core::ptr::copy_nonoverlapping(
                buf.as_ptr(),
                req_virt.add(self.hdr_len),
                buf.len(),
            );

            let d = {
                let q = &mut self.tx_queue;
                let d = q.alloc_desc(self.tx_req_phys as u64, (self.hdr_len + buf.len()) as u32, 0);
                q.submit(d);
                d
            };

            self.notify_tx();
            self.tx_queue.wait_for_completion();
            self.tx_queue.free_chain(d);
            true
        }
    }

    fn poll_receive(&mut self, packet_buf: &mut [u8]) -> Option<usize> {
        let notify_cfg = self.notify_cfg;
        let notify_off_multiplier = self.notify_off_multiplier;
        unsafe {
            let q = &mut self.rx_queue;
            let notify_off = q.notify_off;
            let used_idx = core::ptr::addr_of!((*q.used).idx).read_volatile();
            if q.last_used_idx == used_idx {
                return None;
            }

            let slot = q.last_used_idx as usize % q.size as usize;
            // The VirtqUsed overlay declares ring[32] but the real ring has
            // q.size (up to 256) entries; index via raw pointer like submit()
            // does, or slot >= 32 trips the array bounds check and panics.
            let elem_ptr = (q.used as usize + 4 + slot * 8) as *const VirtqUsedElem;
            let desc_id = core::ptr::addr_of!((*elem_ptr).id).read_volatile() as usize;
            let len = core::ptr::addr_of!((*elem_ptr).len).read_volatile() as usize;

            let page_virt = mm::phys_to_virt(self.rx_buffers_phys[desc_id]) as *const u8;
            if len > self.hdr_len {
                let payload_len = len - self.hdr_len;
                let copy_len = payload_len.min(packet_buf.len());
                core::ptr::copy_nonoverlapping(
                    page_virt.add(self.hdr_len),
                    packet_buf.as_mut_ptr(),
                    copy_len,
                );

                q.submit(desc_id as u16);
                Self::notify_queue(notify_cfg, notify_off, notify_off_multiplier);

                q.last_used_idx = q.last_used_idx.wrapping_add(1);
                Some(copy_len)
            } else {
                q.submit(desc_id as u16);
                Self::notify_queue(notify_cfg, notify_off, notify_off_multiplier);
                q.last_used_idx = q.last_used_idx.wrapping_add(1);
                None
            }
        }
    }
}

static DEVICES: Mutex<[Option<VirtioNetDevice>; MAX_NET_DEVICES]> =
    Mutex::new([const { None }; MAX_NET_DEVICES]);

static DEVICE_COUNT: Mutex<usize> = Mutex::new(0);

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
                    devs[found] = Some(d);
                    found += 1;
                }
            }
        }
        if found >= MAX_NET_DEVICES { break; }
    }

    *cnt = found;
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

pub fn poll_receive(dev_idx: usize, buf: &mut [u8]) -> Option<usize> {
    let mut devs = DEVICES.lock();
    if let Some(ref mut d) = devs[dev_idx] {
        d.poll_receive(buf)
    } else {
        None
    }
}
