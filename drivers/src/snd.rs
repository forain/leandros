//! VirtIO Sound Driver.

use super::{Driver, DriverError, pci};
use ipc::Message;
use mm::{phys_to_virt, virt_to_phys, buddy};
use mm::paging::{map_kernel_device, PageFlags};
use leandros_lib;
use core::sync::atomic::{self, Ordering};

pub const VIRTIO_SND_VENDOR_ID: u16 = 0x1AF4;
pub const VIRTIO_SND_DEVICE_ID: u16 = 0x1059;

pub const VIRTIO_SND_VQ_CONTROL: u16 = 0;
pub const VIRTIO_SND_VQ_EVENT:   u16 = 1;
pub const VIRTIO_SND_VQ_TX:      u16 = 2;
pub const VIRTIO_SND_VQ_RX:      u16 = 3;

pub const VIRTIO_SND_R_JACK_INFO:        u32 = 0x0001;
pub const VIRTIO_SND_R_PCM_INFO:         u32 = 0x0100;
pub const VIRTIO_SND_R_PCM_SET_PARAMS:   u32 = 0x0101;
pub const VIRTIO_SND_R_PCM_PREPARE:      u32 = 0x0102;
pub const VIRTIO_SND_R_PCM_RELEASE:      u32 = 0x0103;
pub const VIRTIO_SND_R_PCM_START:        u32 = 0x0104;
pub const VIRTIO_SND_R_PCM_STOP:         u32 = 0x0105;

pub const VIRTIO_SND_S_OK:               u32 = 0x8000;

pub const VIRTIO_SND_PCM_FMT_S16:        u8 = 5;
pub const VIRTIO_SND_PCM_RATE_44100:     u8 = 6;
pub const VIRTIO_SND_PCM_RATE_48000:     u8 = 7;

const QUEUE_SIZE: usize = 256;

#[repr(C)]
struct VirtioSndHdr { code: u32 }

#[repr(C)]
struct VirtioSndPcmHdr { hdr: VirtioSndHdr, stream_id: u32 }

#[repr(C)]
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

#[repr(C)]
struct VirtioSndPcmXfer { stream_id: u32 }

#[repr(C)]
struct VirtioSndPcmStatus { status: u32, latency_bytes: u32 }

#[repr(C)]
struct VirtioDesc { addr: u64, len: u32, flags: u16, next: u16 }

#[repr(C, align(2))]
struct VirtioAvail { flags: u16, idx: u16, ring: [u16; QUEUE_SIZE], used_event: u16 }

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtioUsedElem { id: u32, len: u32 }

#[repr(C, align(4))]
struct VirtioUsed { flags: u16, idx: u16, ring: [VirtioUsedElem; QUEUE_SIZE], avail_event: u16 }

struct VirtQueue {
    id: u16, notify_off: u16,
    desc: *mut VirtioDesc, avail: *mut VirtioAvail, used: *mut VirtioUsed,
    last_avail_idx: u16, last_used_idx: u16, free_head: u16, num_free: u16,
}

#[repr(C)]
struct VirtioSndPersistent {
    ctrl_cmd: [u8; 128],
    ctrl_status: VirtioSndHdr,
    tx_xfer: [VirtioSndPcmXfer; QUEUE_SIZE],
    tx_status: [VirtioSndPcmStatus; QUEUE_SIZE],
    tx_data: [[u8; 512]; QUEUE_SIZE],
}

pub struct VirtioSnd {
    common_cfg: usize, notify_cfg: usize, notify_off_multiplier: u32,
    vqs: [Option<VirtQueue>; 4],
    persistent: *mut VirtioSndPersistent,
    initialized: bool, stream_active: bool,
    tx_count: u32,
}

unsafe impl Send for VirtioSnd {}
unsafe impl Sync for VirtioSnd {}

impl VirtioSnd {
    pub const fn new() -> Self {
        Self {
            common_cfg: 0, notify_cfg: 0, notify_off_multiplier: 0,
            vqs: [None, None, None, None], persistent: core::ptr::null_mut(),
            initialized: false, stream_active: false, tx_count: 0,
        }
    }

    unsafe fn init_device(&mut self) -> Result<(), DriverError> {
        pci::serial_debug("[SND] Probing VirtIO Sound...\n");
        let dev = pci::find_device(VIRTIO_SND_VENDOR_ID, VIRTIO_SND_DEVICE_ID).ok_or_else(|| {
            pci::serial_debug("[SND] Device not found in PCI scan\n");
            DriverError::NotFound
        })?;
        
        let pci_cmd = pci::pci_read_config_16(dev.bus, dev.dev, dev.func, 0x04);
        pci::pci_write_config_16(dev.bus, dev.dev, dev.func, 0x04, pci_cmd | 0x06);

        let phys = buddy::alloc(6).ok_or(DriverError::Io)?; // Allocate 64 pages (order 6)
        self.persistent = phys_to_virt(phys) as *mut VirtioSndPersistent;
        core::ptr::write_bytes(self.persistent as *mut u8, 0, 64 * 4096);

        self.parse_caps(&dev)?;
        if self.common_cfg == 0 { 
            pci::serial_debug("[SND] common_cfg not found!\n");
            return Err(DriverError::NotFound); 
        }
        pci::serial_debug("[SND] common_cfg mapped at "); pci::serial_debug_hex(self.common_cfg as u32); pci::serial_debug("\n");

        self.write_common_8(20, 0); // Reset
        let mut status = 3; // ACKNOWLEDGE | DRIVER
        self.write_common_8(20, status);
        
        self.write_common_32(8, 0); // Feature selector 0
        self.write_common_32(12, 0); // Reject all features in selector 0
        
        self.write_common_32(8, 1); // Feature selector 1
        let f1 = self.read_common_32(4);
        self.write_common_32(12, f1 & 1); // Accept VERSION_1
        
        status |= 8; // FEATURES_OK
        self.write_common_8(20, status);
        if self.read_common_8(20) & 8 == 0 { return Err(DriverError::Unsupported); }

        for q in 0..4 { self.init_vq(q)?; }
        status |= 4; // DRIVER_OK
        self.write_common_8(20, status);

        self.initialized = true;
        pci::serial_debug("[SND] Initialized successfully.\n");
        Ok(())
    }

    unsafe fn init_vq(&mut self, qid: u16) -> Result<(), DriverError> {
        self.write_common_16(22, qid);
        if self.read_common_16(24) == 0 { return Err(DriverError::Unsupported); }
        let phys = buddy::alloc(1).ok_or(DriverError::Io)?;
        let virt = phys_to_virt(phys);
        let desc = virt as *mut VirtioDesc;
        let avail = (virt + 16 * QUEUE_SIZE) as *mut VirtioAvail;
        let used = leandros_lib::align_up(virt + 16 * QUEUE_SIZE + 6 + 2 * QUEUE_SIZE, 4) as *mut VirtioUsed;
        core::ptr::write_bytes(virt as *mut u8, 0, 8192);
        for i in 0..QUEUE_SIZE as u16 {
            (*desc.add(i as usize)).next = (i + 1) % QUEUE_SIZE as u16;
            (*desc.add(i as usize)).flags = 0;
        }
        self.write_common_16(24, QUEUE_SIZE as u16);
        self.write_common_64(32, phys as u64);
        self.write_common_64(40, (phys + (avail as usize - virt)) as u64);
        self.write_common_64(48, (phys + (used as usize - virt)) as u64);
        self.write_common_16(28, 1);
        self.vqs[qid as usize] = Some(VirtQueue {
            id: qid, notify_off: self.read_common_16(30), desc, avail, used,
            last_avail_idx: 0, last_used_idx: 0, free_head: 0, num_free: QUEUE_SIZE as u16,
        });
        Ok(())
    }

    unsafe fn parse_caps(&mut self, dev: &pci::PciDevice) -> Result<(), DriverError> {
        let mut ptr = pci::pci_read_config_8(dev.bus, dev.dev, dev.func, 0x34);
        while ptr != 0 {
            let id = pci::pci_read_config_8(dev.bus, dev.dev, dev.func, ptr);
            let next = pci::pci_read_config_8(dev.bus, dev.dev, dev.func, ptr + 1);
            if id == 0x09 {
                let typ = pci::pci_read_config_8(dev.bus, dev.dev, dev.func, ptr + 3);
                let bar_idx = pci::pci_read_config_8(dev.bus, dev.dev, dev.func, ptr + 4);
                let off = pci::pci_read_config_32_any(dev.bus, dev.dev, dev.func, ptr + 8);
                let len = pci::pci_read_config_32_any(dev.bus, dev.dev, dev.func, ptr + 12);
                
                if bar_idx < 6 {
                    let mut bar_val = dev.bars[bar_idx as usize] as u64;
                    if (bar_val & 0x06) == 0x04 && bar_idx < 5 {
                        let high = pci::pci_read_config_32(dev.bus, dev.dev, dev.func, 0x10 + (bar_idx + 1) * 4);
                        bar_val |= (high as u64) << 32;
                    }
                    
                    let phys = (bar_val & !0xF) as usize;
                    if phys != 0 {
                        let map_size = (off as usize + len as usize + 4095) & !4095;
                        let base = map_kernel_device(phys, map_size.max(0x10000), PageFlags::PRESENT|PageFlags::WRITABLE|PageFlags::NOCACHE).ok_or(DriverError::Io)?;
                        
                        match typ {
                            1 => { self.common_cfg = base + off as usize; }
                            2 => {
                                self.notify_cfg = base + off as usize;
                                self.notify_off_multiplier = pci::pci_read_config_32_any(dev.bus, dev.dev, dev.func, ptr + 16);
                            }
                            _ => {}
                        }
                    }
                }
            }
            ptr = next;
        }
        Ok(())
    }

    unsafe fn write_common_8(&self, o: usize, v: u8) { core::ptr::write_volatile((self.common_cfg+o) as *mut u8, v); }
    unsafe fn write_common_16(&self, o: usize, v: u16) { core::ptr::write_volatile((self.common_cfg+o) as *mut u16, v); }
    unsafe fn write_common_32(&self, o: usize, v: u32) { core::ptr::write_volatile((self.common_cfg+o) as *mut u32, v); }
    unsafe fn write_common_64(&self, o: usize, v: u64) { core::ptr::write_volatile((self.common_cfg+o) as *mut u64, v); }
    unsafe fn read_common_16(&self, o: usize) -> u16 { core::ptr::read_volatile((self.common_cfg+o) as *const u16) }
    unsafe fn read_common_32(&self, o: usize) -> u32 { core::ptr::read_volatile((self.common_cfg+o) as *const u32) }
    unsafe fn read_common_8(&self, o: usize) -> u8 { core::ptr::read_volatile((self.common_cfg+o) as *const u8) }

    pub fn reconfigure_stream(&mut self, stream_id: u32, freq: u32, channels: u8) {
        let rate = match freq { 11025=>2, 22050=>4, 44100=>6, 48000=>7, _=>6 };
        pci::serial_debug("[SND] Reconfiguring stream 0: freq="); pci::serial_debug_hex(freq);
        pci::serial_debug(" rate="); pci::serial_debug_hex(rate as u32); pci::serial_debug("\n");

        if self.stream_active {
            self.send_control_cmd(&VirtioSndPcmHdr { hdr: VirtioSndHdr { code: VIRTIO_SND_R_PCM_STOP }, stream_id });
            self.send_control_cmd(&VirtioSndPcmHdr { hdr: VirtioSndHdr { code: VIRTIO_SND_R_PCM_RELEASE }, stream_id });
        }
        self.send_control_cmd(&VirtioSndPcmSetParams {
            hdr: VirtioSndPcmHdr { hdr: VirtioSndHdr { code: VIRTIO_SND_R_PCM_SET_PARAMS }, stream_id },
            buffer_bytes: 65536, period_bytes: 4096, features: 0, channels, format: VIRTIO_SND_PCM_FMT_S16, rate, padding: 0,
        });
        self.send_control_cmd(&VirtioSndPcmHdr { hdr: VirtioSndHdr { code: VIRTIO_SND_R_PCM_PREPARE }, stream_id });
        self.send_control_cmd(&VirtioSndPcmHdr { hdr: VirtioSndHdr { code: VIRTIO_SND_R_PCM_START }, stream_id });
        self.stream_active = true;
        self.tx_count = 0;
    }

    fn send_control_cmd<T>(&mut self, cmd: &T) {
        let code = unsafe { *(cmd as *const T as *const u32) };
        pci::serial_debug("[SND] CTRL CMD "); pci::serial_debug_hex(code); pci::serial_debug(" -> ");
        
        let mut vq_id = 0;
        let mut notify_off = 0;
        let head = {
            let vq = self.vqs[0].as_mut().unwrap();
            vq_id = vq.id;
            notify_off = vq.notify_off;
            unsafe {
                core::ptr::copy_nonoverlapping(cmd as *const T as *const u8, (*self.persistent).ctrl_cmd.as_mut_ptr(), core::mem::size_of::<T>());
                core::ptr::write_volatile(&mut (*self.persistent).ctrl_status.code, 0xFFFF);
                let h = vq.free_head;
                let d1 = vq.desc.add(h as usize);
                (*d1).addr = virt_to_phys((*self.persistent).ctrl_cmd.as_ptr() as usize) as u64;
                (*d1).len = core::mem::size_of::<T>() as u32; (*d1).flags = 1;
                let d2 = vq.desc.add((*d1).next as usize);
                (*d2).addr = virt_to_phys(&(*self.persistent).ctrl_status as *const _ as usize) as u64;
                (*d2).len = 4; (*d2).flags = 2;
                vq.free_head = (*d2).next; vq.num_free -= 2;
                (*vq.avail).ring[vq.last_avail_idx as usize % QUEUE_SIZE] = h;
                vq.last_avail_idx = vq.last_avail_idx.wrapping_add(1);
                atomic::compiler_fence(Ordering::SeqCst);
                (*vq.avail).idx = vq.last_avail_idx;
                h
            }
        };
        unsafe {
            let addr = self.notify_cfg + (notify_off as u32 * self.notify_off_multiplier) as usize;
            core::ptr::write_volatile(addr as *mut u16, vq_id);
            let vq = self.vqs[0].as_mut().unwrap();
            let mut timeout = 5000000;
            while vq.last_used_idx == core::ptr::read_volatile(&(*vq.used).idx) && timeout > 0 { core::hint::spin_loop(); timeout -= 1; }
            if timeout == 0 { pci::serial_debug("TIMEOUT\n"); return; }
            while vq.last_used_idx != core::ptr::read_volatile(&(*vq.used).idx) {
                vq.last_used_idx = vq.last_used_idx.wrapping_add(1);
                vq.num_free += 2;
            }
            let s = core::ptr::read_volatile(&(*self.persistent).ctrl_status.code);
            pci::serial_debug_hex(s); pci::serial_debug("\n");
        }
    }

    /// Non-blocking PCM transmission. Returns bytes actually queued.
    pub fn send_pcm_data(&mut self, data: &[u8]) -> usize {
        if !self.initialized { return 0; }
        
        let vq = self.vqs[2].as_mut().unwrap();
        // Reclaim processed descriptors
        let used = unsafe { core::ptr::read_volatile(&(*vq.used).idx) };
        while vq.last_used_idx != used {
            vq.last_used_idx = vq.last_used_idx.wrapping_add(1);
            vq.num_free += 3;
        }

        if vq.num_free < 3 { return 0; }
        
        let chunk_len = data.len().min(512);
        let vq_id = vq.id;
        let notify_off = vq.notify_off;
        unsafe {
            let slot = vq.last_avail_idx as usize % QUEUE_SIZE;
            (*self.persistent).tx_xfer[slot].stream_id = 0;
            core::ptr::copy_nonoverlapping(data.as_ptr(), (*self.persistent).tx_data[slot].as_mut_ptr(), chunk_len);
            
            let h = vq.free_head;
            let d1 = vq.desc.add(h as usize);
            (*d1).addr = virt_to_phys(&(*self.persistent).tx_xfer[slot] as *const _ as usize) as u64;
            (*d1).len = 4; (*d1).flags = 1;
            let d2 = vq.desc.add((*d1).next as usize);
            (*d2).addr = virt_to_phys((*self.persistent).tx_data[slot].as_ptr() as usize) as u64;
            (*d2).len = chunk_len as u32; (*d2).flags = 1;
            let d3 = vq.desc.add((*d2).next as usize);
            (*d3).addr = virt_to_phys(&(*self.persistent).tx_status[slot] as *const _ as usize) as u64;
            (*d3).len = 8; (*d3).flags = 2;
            
            vq.free_head = (*d3).next; vq.num_free -= 3;
            (*vq.avail).ring[vq.last_avail_idx as usize % QUEUE_SIZE] = h;
            vq.last_avail_idx = vq.last_avail_idx.wrapping_add(1);
            atomic::compiler_fence(Ordering::SeqCst);
            (*vq.avail).idx = vq.last_avail_idx;
        };
        unsafe {
            let addr = self.notify_cfg + (notify_off as u32 * self.notify_off_multiplier) as usize;
            core::ptr::write_volatile(addr as *mut u16, vq_id);
        }
        
        self.tx_count += 1;
        if self.tx_count % 1000 == 0 {
            pci::serial_debug("[SND] TX pkts: "); pci::serial_debug_hex(self.tx_count); pci::serial_debug("\n");
        }
        chunk_len
    }
}

impl Driver for VirtioSnd {
    fn probe(&mut self) -> Result<(), DriverError> { unsafe { self.init_device() } }
    fn handle(&mut self, msg: Message) -> Message {
        match msg.tag {
            0x100 => {
                let freq = u32::from_le_bytes([msg.data[0], msg.data[1], msg.data[2], msg.data[3]]);
                self.reconfigure_stream(0, freq, msg.data[4]);
                Message::empty()
            }
            0x200 => {
                let len = u16::from_le_bytes([msg.data[0], msg.data[1]]) as usize;
                if len > 0 { self.send_pcm_data(&msg.data[2..2+len]); }
                Message::empty()
            }
            0x1000 => { let mut r = Message::empty(); r.tag = 0x1001; r }
            _ => Message::empty()
        }
    }
}
