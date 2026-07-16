//! VirtIO input/keyboard driver (PCI transport, polling mode).

use spin::Mutex;
use crate::pci::{PciDevice, find_device, pci_read_config_8, pci_read_config_16, pci_read_config_32, pci_write_config_16};
use mm;

const VIRTIO_PCI_VENDOR: u16 = 0x1af4;
const VIRTIO_PCI_DEVICE_INPUT: u16 = 0x1052; // modern virtio-input

const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;

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
    notify_off_multiplier: u32,
    
    queue: Option<VirtioQueue>,
    event_buffer_phys: usize,
}

unsafe impl Send for VirtioKeyboardDevice {}
unsafe impl Sync for VirtioKeyboardDevice {}

pub static VIRTIO_KEYBOARD: Mutex<Option<VirtioKeyboardDevice>> = Mutex::new(None);

impl VirtioKeyboardDevice {
    pub fn new() -> Option<Self> {
        let dev = find_device(VIRTIO_PCI_VENDOR, VIRTIO_PCI_DEVICE_INPUT)?;
        crate::pci::serial_debug("[KBD] Found VirtIO Input device\n");

        // Enable PCI Memory Space (bit 1) and Bus Master (bit 2)
        unsafe {
            let cmd = pci_read_config_16(dev.bus, dev.dev, dev.func, 0x04);
            pci_write_config_16(dev.bus, dev.dev, dev.func, 0x04, cmd | 0x0006);
        }

        let mut common_cfg = core::ptr::null_mut();
        let mut notify_cfg = core::ptr::null_mut();
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
            crate::pci::serial_debug("[KBD] Missing required VirtIO capabilities\n");
            return None;
        }

        let mut kbd = Self {
            _pci_dev: dev,
            common_cfg,
            notify_cfg,
            notify_off_multiplier,
            queue: None,
            event_buffer_phys: 0,
        };

        kbd.init_device();
        Some(kbd)
    }

    fn init_device(&mut self) {
        unsafe {
            let cfg = self.common_cfg;
            let status = core::ptr::addr_of_mut!((*cfg).device_status);
            // 1. Reset device
            status.write_volatile(0);
            // 2. Set ACKNOWLEDGE
            status.write_volatile(status.read_volatile() | VIRTIO_STATUS_ACKNOWLEDGE);
            // 3. Set DRIVER
            status.write_volatile(status.read_volatile() | VIRTIO_STATUS_DRIVER);
            // 4. Negotiate features
            core::ptr::addr_of_mut!((*cfg).device_feature_select).write_volatile(0);
            let _f0 = core::ptr::addr_of!((*cfg).device_feature).read_volatile();
            core::ptr::addr_of_mut!((*cfg).driver_feature_select).write_volatile(0);
            core::ptr::addr_of_mut!((*cfg).driver_feature).write_volatile(0);
            // 5. Set FEATURES_OK
            status.write_volatile(status.read_volatile() | VIRTIO_STATUS_FEATURES_OK);
            // 6. Setup queue 0 (eventq)
            self.queue = self.setup_queue(0);
            // 7. Set DRIVER_OK
            status.write_volatile(status.read_volatile() | VIRTIO_STATUS_DRIVER_OK);
        }
        crate::pci::serial_debug("[KBD] VirtIO Input device initialized\n");
    }

    unsafe fn setup_queue(&mut self, id: u16) -> Option<VirtioQueue> {
        let cfg = self.common_cfg;
        core::ptr::addr_of_mut!((*cfg).queue_select).write_volatile(id);
        let max_size = core::ptr::addr_of!((*cfg).queue_size).read_volatile();
        if max_size == 0 { return None; }

        let size = max_size.min(32); // Use at most 32 descriptors
        core::ptr::addr_of_mut!((*cfg).queue_size).write_volatile(size);

        let notify_off = core::ptr::addr_of!((*cfg).queue_notify_off).read_volatile();

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

        (*avail).idx = size;
        (*avail).flags = 0; // Request interrupts (though we will poll)

        core::ptr::addr_of_mut!((*cfg).queue_desc).write_volatile(desc_phys as u64);
        core::ptr::addr_of_mut!((*cfg).queue_driver).write_volatile(avail_phys as u64);
        core::ptr::addr_of_mut!((*cfg).queue_device).write_volatile(used_phys as u64);
        core::ptr::addr_of_mut!((*cfg).queue_enable).write_volatile(1u16);

        // Notify the device
        let notify_ptr = (self.notify_cfg as usize + notify_off as usize * self.notify_off_multiplier as usize) as *mut u16;
        notify_ptr.write_volatile(id);

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
        let q = match &mut self.queue {
            Some(q) => q,
            None => return,
        };

        unsafe {
            let used = q.used;
            let last_used = q.last_used_idx;
            let current_used = (*used).idx;

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

                // Push to evdev
                evdev_server::push_event(0, ev.type_, ev.code, ev.value);

                // Recycle descriptor: put it back into avail ring
                let avail = q.avail;
                let avail_idx = (*avail).idx as usize % q.size as usize;
                let ring_ptr = (avail as usize + 4) as *mut u16;
                ring_ptr.add(avail_idx).write_volatile(desc_id);
                (*avail).idx = (*avail).idx.wrapping_add(1);

                count += 1;
                idx = idx.wrapping_add(1);
            }

            q.last_used_idx = current_used;

            if count > 0 {
                // Notify the device
                let notify_ptr = (self.notify_cfg as usize + q.notify_off as usize * self.notify_off_multiplier as usize) as *mut u16;
                notify_ptr.write_volatile(0); // Queue 0
            }
        }
    }
}

pub fn init() {
    let mut kbd = VIRTIO_KEYBOARD.lock();
    if kbd.is_none() {
        *kbd = VirtioKeyboardDevice::new();
    }
}

pub fn poll_events() {
    if let Some(kbd) = &mut *VIRTIO_KEYBOARD.lock() {
        kbd.poll();
    }
}
