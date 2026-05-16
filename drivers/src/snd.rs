//! VirtIO Sound Driver.

use super::{Driver, DriverError, pci};
use ipc::Message;
use mm::{phys_to_virt, virt_to_phys, buddy};
use mm::paging::{map_kernel_device, PageFlags};
use leandros_lib;

pub const VIRTIO_SND_VENDOR_ID: u16 = 0x1AF4;
pub const VIRTIO_SND_DEVICE_ID: u16 = 0x1059;

// ── VirtIO PCI Constants ─────────────────────────────────────────────────────

pub const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
pub const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
pub const VIRTIO_PCI_CAP_ISR_CFG:    u8 = 3;
pub const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

// ── VirtIO Sound Constants ───────────────────────────────────────────────────

pub const VIRTIO_SND_R_JACK_INFO: u32 = 1;
pub const VIRTIO_SND_R_PCM_INFO: u32 = 0x0101;
pub const VIRTIO_SND_R_PCM_SET_PARAMS: u32 = 0x0102;
pub const VIRTIO_SND_R_PCM_PREPARE: u32 = 0x0103;
pub const VIRTIO_SND_R_PCM_RELEASE: u32 = 0x0104;
pub const VIRTIO_SND_R_PCM_START: u32 = 0x0105;
pub const VIRTIO_SND_R_PCM_STOP: u32 = 0x0106;

pub const VIRTIO_SND_S_OK: u64 = 0;

pub const VIRTIO_SND_PCM_FMT_S16: u8 = 3;
pub const VIRTIO_SND_PCM_RATE_11025: u8 = 2;
pub const VIRTIO_SND_PCM_RATE_22050: u8 = 4;
pub const VIRTIO_SND_PCM_RATE_44100: u8 = 7;

// ── VirtIO Status Bits ───────────────────────────────────────────────────────

pub const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
pub const VIRTIO_STATUS_DRIVER:      u8 = 2;
pub const VIRTIO_STATUS_DRIVER_OK:   u8 = 4;
pub const VIRTIO_STATUS_FEATURES_OK: u8 = 8;

// ── Virtqueues ───────────────────────────────────────────────────────────────

pub const VIRTIO_SND_VQ_CONTROL: u16 = 0;
pub const VIRTIO_SND_VQ_EVENT: u16 = 1;
pub const VIRTIO_SND_VQ_TX: u16 = 2;
pub const VIRTIO_SND_VQ_RX: u16 = 3;

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtioSndHdr {
    code: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtioSndPcmHdr {
    hdr: VirtioSndHdr,
    stream_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtioSndPcmXfer {
    stream_id: u32,
    _padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtioSndPcmStatus {
    status: u32,
    latency_bytes: u32,
}

// ── VirtQueue Structure ───────────────────────────────────────────────────────

const QUEUE_SIZE: usize = 256;

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtioSndPcmSetParams {
    hdr: VirtioSndPcmHdr,
    buffer_bytes: u32,
    period_bytes: u32,
    features: u32,
    channels: u8,
    format: u8,
    rate: u8,
    padding: u8,
}

#[repr(C, align(16))]
struct VirtioDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C, align(2))]
struct VirtioAvail {
    flags: u16,
    idx: u16,
    ring: [u16; QUEUE_SIZE],
    used_event: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtioUsedElem {
    id: u32,
    len: u32,
}

#[repr(C, align(4))]
struct VirtioUsed {
    flags: u16,
    idx: u16,
    ring: [VirtioUsedElem; QUEUE_SIZE],
    avail_event: u16,
}

struct VirtQueue {
    id: u16,
    notify_off: u16,
    desc: *mut VirtioDesc,
    avail: *mut VirtioAvail,
    used: *mut VirtioUsed,
    last_used_idx: u16,
    free_head: u16,
    num_free: u16,
}

unsafe impl Send for VirtQueue {}
unsafe impl Sync for VirtQueue {}

#[repr(C)]
struct VirtioSndPersistent {
    ctrl_cmd: [u8; 128],
    ctrl_status: u32,
    tx_xfer: [VirtioSndPcmXfer; QUEUE_SIZE],
    tx_status: [VirtioSndPcmStatus; QUEUE_SIZE],
    tx_data: [[u8; 512]; QUEUE_SIZE],
}

// ── Driver Implementation ─────────────────────────────────────────────────────

pub struct VirtioSnd {
    pci_dev: Option<pci::PciDevice>,
    common_cfg: usize,
    device_cfg: usize,
    notify_cfg: usize,
    notify_off_multiplier: u32,
    initialized: bool,
    _stream_id: u32,
    vqs: [Option<VirtQueue>; 4],
    persistent: *mut VirtioSndPersistent,
}

unsafe impl Send for VirtioSnd {}
unsafe impl Sync for VirtioSnd {}

impl VirtioSnd {
    pub const fn new() -> Self {
        Self {
            pci_dev: None,
            common_cfg: 0,
            device_cfg: 0,
            notify_cfg: 0,
            notify_off_multiplier: 0,
            initialized: false,
            _stream_id: 0,
            vqs: [None, None, None, None],
            persistent: core::ptr::null_mut(),
        }
    }

    fn init_device(&mut self) -> Result<(), DriverError> {
        pci::serial_debug("[SND] Initializing VirtIO Sound...\n");
        let dev = pci::find_device(VIRTIO_SND_VENDOR_ID, VIRTIO_SND_DEVICE_ID)
            .ok_or_else(|| {
                pci::serial_debug("[SND] Device not found!\n");
                DriverError::NotFound
            })?;
        
        self.pci_dev = Some(dev);
        pci::serial_debug("[SND] Device found.\n");

        unsafe {
            let pci_cmd = pci::pci_read_config_16(dev.bus, dev.dev, dev.func, 0x04);
            pci::pci_write_config_16(dev.bus, dev.dev, dev.func, 0x04, pci_cmd | 0x06);

            let pages = (core::mem::size_of::<VirtioSndPersistent>() + 4095) / 4096;
            let order = if pages > 16 { 5 } else if pages > 8 { 4 } else if pages > 4 { 3 } else if pages > 2 { 2 } else if pages > 1 { 1 } else { 0 };
            let phys = buddy::alloc(order).ok_or(DriverError::Io)?;
            self.persistent = phys_to_virt(phys) as *mut VirtioSndPersistent;
            core::ptr::write_bytes(self.persistent as *mut u8, 0, (1 << order) * 4096);

            self.parse_caps(&dev)?;
            
            if self.common_cfg == 0 { return Err(DriverError::NotFound); }

            self.write_common_8(20, 0);
            let mut status = VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER;
            self.write_common_8(20, status);

            self.write_common_32(8, 1);
            let f1 = self.read_common_32(4);
            self.write_common_32(12, f1 & 1);

            status |= VIRTIO_STATUS_FEATURES_OK;
            self.write_common_8(20, status);
            
            if self.read_common_8(20) & VIRTIO_STATUS_FEATURES_OK == 0 {
                return Err(DriverError::Unsupported);
            }

            // Initialize ALL mandatory queues
            self.init_vq(VIRTIO_SND_VQ_CONTROL)?;
            self.init_vq(VIRTIO_SND_VQ_EVENT)?;
            self.init_vq(VIRTIO_SND_VQ_TX)?;
            self.init_vq(VIRTIO_SND_VQ_RX)?;

            status |= VIRTIO_STATUS_DRIVER_OK;
            self.write_common_8(20, status);
        }

        self.initialized = true;
        pci::serial_debug("[SND] Initialized.\n");
        self.reconfigure_stream(0, 44100, 2);
        Ok(())
    }

    unsafe fn init_vq(&mut self, qid: u16) -> Result<(), DriverError> {
        self.write_common_16(22, qid);
        let size = self.read_common_16(24);
        pci::serial_debug("[SND] QID ");
        pci::serial_debug_hex(qid as u32);
        pci::serial_debug(" size=");
        pci::serial_debug_hex(size as u32);
        pci::serial_debug("\n");

        if size == 0 { return Err(DriverError::Unsupported); }
        
        let phys = buddy::alloc(0).ok_or(DriverError::Io)?;
        let virt = phys_to_virt(phys);
        let desc = virt as *mut VirtioDesc;
        let avail = (virt + 16 * QUEUE_SIZE) as *mut VirtioAvail;
        let used = leandros_lib::align_up(virt + 16 * QUEUE_SIZE + 6 + 2 * QUEUE_SIZE, 4) as *mut VirtioUsed;

        core::ptr::write_bytes(virt as *mut u8, 0, 4096);
        for i in 0..QUEUE_SIZE as u16 - 1 {
            (*desc.add(i as usize)).next = i + 1;
            (*desc.add(i as usize)).flags = 1;
        }

        self.write_common_16(24, QUEUE_SIZE as u16);
        self.write_common_64(32, phys as u64);
        self.write_common_64(40, (phys + (avail as usize - virt)) as u64);
        self.write_common_64(48, (phys + (used as usize - virt)) as u64);
        self.write_common_16(28, 1);

        let notify_off = self.read_common_16(30);
        self.vqs[qid as usize] = Some(VirtQueue {
            id: qid, notify_off, desc, avail, used,
            last_used_idx: 0, free_head: 0, num_free: QUEUE_SIZE as u16,
        });
        Ok(())
    }

    unsafe fn parse_caps(&mut self, dev: &pci::PciDevice) -> Result<(), DriverError> {
        let mut cap_ptr = pci::pci_read_config_8(dev.bus, dev.dev, dev.func, 0x34);
        while cap_ptr != 0 {
            let dw0 = pci::pci_read_config_32(dev.bus, dev.dev, dev.func, cap_ptr);
            let dw1 = pci::pci_read_config_32(dev.bus, dev.dev, dev.func, cap_ptr + 4);
            let cap_id = (dw0 & 0xFF) as u8;
            let cap_next = ((dw0 >> 8) & 0xFF) as u8;
            let cfg_type = ((dw0 >> 24) & 0xFF) as u8;
            let bar_idx = (dw1 & 0xFF) as u8;
            if cap_id == 0x09 && bar_idx < 6 {
                let mut bar_val = dev.bars[bar_idx as usize] as u64;
                if (bar_val & 0x6) == 0x4 && bar_idx < 5 {
                    bar_val |= (dev.bars[bar_idx as usize + 1] as u64) << 32;
                }
                if bar_val != 0 {
                    let offset = pci::pci_read_config_32(dev.bus, dev.dev, dev.func, cap_ptr + 8);
                    let flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::NOCACHE;
                    let base = map_kernel_device((bar_val & !0xF) as usize, 0x10000, flags)
                        .ok_or(DriverError::Io)?;
                    match cfg_type {
                        VIRTIO_PCI_CAP_COMMON_CFG => { self.common_cfg = base + offset as usize; }
                        VIRTIO_PCI_CAP_DEVICE_CFG => { self.device_cfg = base + offset as usize; }
                        VIRTIO_PCI_CAP_NOTIFY_CFG => {
                            self.notify_cfg = base + offset as usize;
                            self.notify_off_multiplier = pci::pci_read_config_32(dev.bus, dev.dev, dev.func, cap_ptr + 16);
                        }
                        _ => {}
                    }
                }
            }
            cap_ptr = cap_next;
        }
        Ok(())
    }

    unsafe fn write_common_8(&self, offset: usize, val: u8) { core::ptr::write_volatile((self.common_cfg + offset) as *mut u8, val); }
    unsafe fn write_common_16(&self, offset: usize, val: u16) { core::ptr::write_volatile((self.common_cfg + offset) as *mut u16, val); }
    unsafe fn write_common_32(&self, offset: usize, val: u32) { core::ptr::write_volatile((self.common_cfg + offset) as *mut u32, val); }
    unsafe fn write_common_64(&self, offset: usize, val: u64) { core::ptr::write_volatile((self.common_cfg + offset) as *mut u64, val); }
    unsafe fn read_common_8(&self, offset: usize) -> u8 { core::ptr::read_volatile((self.common_cfg + offset) as *const u8) }
    unsafe fn read_common_16(&self, offset: usize) -> u16 { core::ptr::read_volatile((self.common_cfg + offset) as *const u16) }
    unsafe fn read_common_32(&self, offset: usize) -> u32 { core::ptr::read_volatile((self.common_cfg + offset) as *const u32) }

    fn reconfigure_stream(&mut self, stream_id: u32, freq: u32, channels: u8) {
        let virtio_rate = match freq {
            11025 => VIRTIO_SND_PCM_RATE_11025,
            22050 => VIRTIO_SND_PCM_RATE_22050,
            44100 => VIRTIO_SND_PCM_RATE_44100,
            _ => VIRTIO_SND_PCM_RATE_44100,
        };
        pci::serial_debug("[SND] Config stream freq=");
        pci::serial_debug_hex(freq);
        pci::serial_debug("\n");

        let params = VirtioSndPcmSetParams {
            hdr: VirtioSndPcmHdr { hdr: VirtioSndHdr { code: VIRTIO_SND_R_PCM_SET_PARAMS }, stream_id },
            buffer_bytes: 65536, period_bytes: 4096, features: 0,
            channels, format: VIRTIO_SND_PCM_FMT_S16, rate: virtio_rate, padding: 0,
        };
        self.send_control_cmd(&params);
        self.send_control_cmd(&VirtioSndPcmHdr { hdr: VirtioSndHdr { code: VIRTIO_SND_R_PCM_PREPARE }, stream_id });
        self.send_control_cmd(&VirtioSndPcmHdr { hdr: VirtioSndHdr { code: VIRTIO_SND_R_PCM_START }, stream_id });
    }

    fn send_control_cmd<T>(&mut self, cmd: &T) {
        let (desc_idx, vq_id, notify_off) = {
            let vq = match self.vqs[VIRTIO_SND_VQ_CONTROL as usize].as_mut() { Some(v) => v, None => return };
            unsafe {
                let cmd_ptr = &(*self.persistent).ctrl_cmd as *const [u8; 128] as *mut u8;
                core::ptr::copy_nonoverlapping(cmd as *const T as *const u8, cmd_ptr, core::mem::size_of::<T>());
                let phys = virt_to_phys(cmd_ptr as usize);
                let d_idx = vq.free_head;
                let d = vq.desc.add(d_idx as usize);
                (*d).addr = phys as u64;
                (*d).len = core::mem::size_of::<T>() as u32;
                (*d).flags = 1;
                let s_idx = (*d).next;
                let s = vq.desc.add(s_idx as usize);
                (*s).addr = virt_to_phys(&(*self.persistent).ctrl_status as *const _ as usize) as u64;
                (*s).len = 4;
                (*s).flags = 2;
                vq.free_head = (*s).next;
                vq.num_free -= 2;
                let avail_idx = (*vq.avail).idx % QUEUE_SIZE as u16;
                (*vq.avail).ring[avail_idx as usize] = d_idx;
                core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
                (*vq.avail).idx += 1;
                (d_idx, vq.id, vq.notify_off)
            }
        };
        unsafe {
            core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
            self.notify_qid(vq_id, notify_off);
            self.wait_vq(VIRTIO_SND_VQ_CONTROL);
        }
    }

    fn wait_vq(&mut self, qid: u16) {
        let vq = self.vqs[qid as usize].as_mut().unwrap();
        unsafe { while vq.last_used_idx == (*vq.used).idx { core::hint::spin_loop(); } vq.last_used_idx += 1; }
    }

    fn reclaim_vq(&mut self, qid: u16) {
        let vq = self.vqs[qid as usize].as_mut().unwrap();
        unsafe {
            while vq.last_used_idx != (*vq.used).idx {
                vq.last_used_idx += 1;
                vq.num_free += 3;
            }
        }
    }

    unsafe fn notify_qid(&self, qid: u16, notify_off: u16) {
        let addr = self.notify_cfg + (notify_off as u32 * self.notify_off_multiplier) as usize;
        core::ptr::write_volatile(addr as *mut u16, qid);
    }

    fn send_pcm_data(&mut self, data: &[u8]) {
        if !self.initialized { return; }
        loop {
            self.reclaim_vq(VIRTIO_SND_VQ_TX);
            let vq = self.vqs[VIRTIO_SND_VQ_TX as usize].as_ref().unwrap();
            if vq.num_free >= 3 { break; }
            core::hint::spin_loop();
        }
        let (h_idx, vq_id, notify_off) = {
            let vq = self.vqs[VIRTIO_SND_VQ_TX as usize].as_mut().unwrap();
            unsafe {
                let slot = ((*vq.avail).idx % QUEUE_SIZE as u16) as usize;
                let xfer = &mut (*self.persistent).tx_xfer[slot];
                xfer.stream_id = 0; xfer._padding = 0;
                
                let data_ptr = &mut (*self.persistent).tx_data[slot] as *mut u8;
                let chunk_len = data.len().min(512);
                core::ptr::copy_nonoverlapping(data.as_ptr(), data_ptr, chunk_len);

                let h_idx = vq.free_head;
                let h_desc = vq.desc.add(h_idx as usize);
                (*h_desc).addr = virt_to_phys(xfer as *const _ as usize) as u64;
                (*h_desc).len = 8; (*h_desc).flags = 1;
                let d_idx = (*h_desc).next;
                let d_desc = vq.desc.add(d_idx as usize);
                (*d_desc).addr = virt_to_phys(data_ptr as usize) as u64;
                (*d_desc).len = chunk_len as u32; (*d_desc).flags = 1;
                let s_idx = (*d_desc).next;
                let s_desc = vq.desc.add(s_idx as usize);
                (*s_desc).addr = virt_to_phys(&(*self.persistent).tx_status[slot] as *const _ as usize) as u64;
                (*s_desc).len = 8; (*s_desc).flags = 2;
                vq.free_head = (*s_desc).next;
                vq.num_free -= 3;
                let avail_idx = (*vq.avail).idx % QUEUE_SIZE as u16;
                (*vq.avail).ring[avail_idx as usize] = h_idx;
                core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
                (*vq.avail).idx += 1;
                (h_idx, vq.id, vq.notify_off)
            }
        };
        unsafe { 
            core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
            self.notify_qid(vq_id, notify_off); 
            self.wait_vq(VIRTIO_SND_VQ_TX);
        }
    }
}

impl Driver for VirtioSnd {
    fn probe(&mut self) -> Result<(), DriverError> { self.init_device() }
    fn handle(&mut self, msg: Message) -> Message {
        match msg.tag {
            0x100 => {
                let freq = (msg.data[0] as u32) | ((msg.data[1] as u32) << 8) | ((msg.data[2] as u32) << 16) | ((msg.data[3] as u32) << 24);
                let channels = msg.data[4];
                self.reconfigure_stream(0, freq, channels);
                Message::empty()
            },
            0x200 => {
                let len_low = msg.data[0] as u16;
                let len_high = msg.data[1] as u16;
                let len = (len_low | (len_high << 8)) as usize;
                if len > 0 { self.send_pcm_data(&msg.data[2..2+len]); }
                Message::empty()
            },
            0x1000 => {
                let mut resp = Message::empty();
                resp.tag = 0x1001;
                resp
            },
            _ => Message::empty()
        }
    }
}
