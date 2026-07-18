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

/// Latency knob: max PCM buffers queued to the device at once
/// (each = 512 B ≈ 2.9 ms at 44.1 kHz stereo S16). Safe to keep small ONLY
/// because the 100 Hz tick pump (servers/pipewire tick_pump) guarantees the
/// queue never runs empty — QEMU 11.x's split audio backend permanently
/// loses the frontend-refill wakeup the first time it polls an empty
/// stream queue (one-shot, unrecoverable without a stream restart).
const TX_MAX_INFLIGHT: u16 = 256;

/// Silence top-up watermark for the tick pump: when fewer than this many
/// buffers are queued, top_up_silence() pads with zeros up to it. Sized at
/// 2x QEMU's worst observed single-poll demand (its audio timer slips and
/// gulps several periods of catch-up at once): 32 × 512 B ≈ 93 ms. Must
/// stay below TX_MAX_INFLIGHT so real data always has room on top.
const TX_TOPUP_BUFS: u16 = 32;

/// Coarse monotonic clock for stall detection. Reads a hardware counter
/// that keeps advancing even while spinning in kernel context with IRQs
/// masked (unlike sched::ticks()). The x86_64 arm assumes ~1 GHz TSC —
/// only ever used for order-of-magnitude stall thresholds, where a few x
/// of error just makes detection proportionally slower.
pub fn monotonic_us() -> u64 {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        let cnt: u64;
        let frq: u64;
        core::arch::asm!("mrs {}, cntvct_el0", out(reg) cnt, options(nomem, nostack));
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) frq, options(nomem, nostack));
        ((cnt as u128 * 1_000_000) / frq.max(1) as u128) as u64
    }
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::x86_64::_rdtsc() / 1000
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        0
    }
}

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
    initialized: bool, stream_active: bool, stream_started: bool,
    tx_count: u32,
    tx_len: [u16; QUEUE_SIZE],
    last_freq: u32, last_channels: u8,
    dbg_first_completion: bool, dbg_ring_full: bool,
    dbg_min_level: u16,
}

unsafe impl Send for VirtioSnd {}
unsafe impl Sync for VirtioSnd {}

impl VirtioSnd {
    pub const fn new() -> Self {
        Self {
            common_cfg: 0, notify_cfg: 0, notify_off_multiplier: 0,
            vqs: [None, None, None, None], persistent: core::ptr::null_mut(),
            initialized: false, stream_active: false, stream_started: false, tx_count: 0,
            tx_len: [0; QUEUE_SIZE],
            last_freq: 44100, last_channels: 2,
            dbg_first_completion: false, dbg_ring_full: false,
            dbg_min_level: 0xFFFF,
        }
    }

    unsafe fn init_device(&mut self) -> Result<(), DriverError> {
        pci::rdebug("[SND] Probing VirtIO Sound...\n");
        let dev = pci::find_device(VIRTIO_SND_VENDOR_ID, VIRTIO_SND_DEVICE_ID).ok_or_else(|| {
            pci::rdebug("[SND] Device not found in PCI scan\n");
            DriverError::NotFound
        })?;
        
        let pci_cmd = pci::pci_read_config_16(dev.bus, dev.dev, dev.func, 0x04);
        pci::pci_write_config_16(dev.bus, dev.dev, dev.func, 0x04, pci_cmd | 0x06);

        let phys = buddy::alloc(6).ok_or(DriverError::Io)?; // Allocate 64 pages (order 6)
        self.persistent = phys_to_virt(phys) as *mut VirtioSndPersistent;
        core::ptr::write_bytes(self.persistent as *mut u8, 0, 64 * 4096);

        self.parse_caps(&dev)?;
        if self.common_cfg == 0 { 
            pci::rdebug("[SND] common_cfg not found!\n");
            return Err(DriverError::NotFound); 
        }
        pci::rdebug("[SND] common_cfg mapped at "); pci::rdebug_hex(self.common_cfg as u32); pci::rdebug("\n");

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
        pci::rdebug("[SND] Initialized successfully.\n");
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
        pci::serial_debug("[SND] reconfigure: freq=");
        pci::serial_debug_hex(freq);
        pci::serial_debug(" ch=");
        pci::serial_debug_hex(channels as u32);
        pci::serial_debug("\n");

        self.last_freq = freq;
        self.last_channels = channels;

        // Tear down unconditionally: QEMU completes any in-flight TX buffers
        // on STOP/RELEASE, and an invalid transition (stream never started)
        // just returns BAD_MSG which is harmless. Gating this on
        // stream_active leaves the stream in an undefined state when our
        // bookkeeping disagrees with the device.
        self.send_control_cmd(&VirtioSndPcmHdr { hdr: VirtioSndHdr { code: VIRTIO_SND_R_PCM_STOP }, stream_id });
        self.send_control_cmd(&VirtioSndPcmHdr { hdr: VirtioSndHdr { code: VIRTIO_SND_R_PCM_RELEASE }, stream_id });

        let s1 = self.send_control_cmd(&VirtioSndPcmSetParams {
            hdr: VirtioSndPcmHdr { hdr: VirtioSndHdr { code: VIRTIO_SND_R_PCM_SET_PARAMS }, stream_id },
            buffer_bytes: 65536, period_bytes: 4096, features: 0, channels, format: VIRTIO_SND_PCM_FMT_S16, rate, padding: 0,
        });
        let s2 = self.send_control_cmd(&VirtioSndPcmHdr { hdr: VirtioSndHdr { code: VIRTIO_SND_R_PCM_PREPARE }, stream_id });
        let s3 = self.send_control_cmd(&VirtioSndPcmHdr { hdr: VirtioSndHdr { code: VIRTIO_SND_R_PCM_START }, stream_id });

        // START must be sent HERE, before any TX buffers are queued: QEMU
        // does not reliably move buffers submitted on a merely-PREPARE'd
        // stream into its playback queue (lazy-START experiments left the
        // voice consuming exactly one timer tick, then permanently silent
        // with a full guest ring). The initial-silence death that immediate
        // START causes (producer opens the stream, then pauses before its
        // first PCM) is handled once by the stall-recovery path.
        self.stream_active = s1 == 0x8000 && s2 == 0x8000 && s3 == 0x8000;
        self.stream_started = self.stream_active;
        if !self.stream_active {
            pci::serial_debug("[SND] stream setup FAILED: set_params=");
            pci::serial_debug_hex(s1);
            pci::serial_debug(" prepare=");
            pci::serial_debug_hex(s2);
            pci::serial_debug(" start=");
            pci::serial_debug_hex(s3);
            pci::serial_debug("\n");
        }
        self.tx_count = 0;
        self.dbg_first_completion = false;
        self.dbg_ring_full = false;
    }

    /// TX used-index snapshot for stall detection: a live stream advances
    /// this every ~3 ms (one 512-byte buffer at 44.1 kHz stereo); a stream
    /// killed by QEMU's underrun auto-disable never advances it again.
    pub fn tx_used_idx(&self) -> u16 {
        match self.vqs[2].as_ref() {
            Some(vq) => unsafe { core::ptr::read_volatile(&(*vq.used).idx) },
            None => 0,
        }
    }

    /// Queued-but-uncompleted TX buffers (device's backlog view).
    pub fn tx_level(&self) -> u16 {
        match self.vqs[2].as_ref() {
            Some(vq) => {
                let used = unsafe { core::ptr::read_volatile(&(*vq.used).idx) };
                vq.last_avail_idx.wrapping_sub(used)
            }
            None => 0,
        }
    }

    /// Pad the TX queue with silence up to TX_TOPUP_BUFS. Called from the
    /// 100 Hz tick pump (and harmless anywhere else): keeps the device-side
    /// queue non-empty during producer pauses, which is a hard liveness
    /// requirement on QEMU 11.x (empty poll = permanently stalled voice).
    /// Silence is semantically correct here — the producer had nothing to
    /// play for this wall-clock interval.
    pub fn top_up_silence(&mut self) {
        if !self.initialized || !self.stream_active { return; }
        let silence = [0u8; 512];
        while self.tx_level() < TX_TOPUP_BUFS {
            if self.send_pcm_data(&silence) == 0 { break; }
        }
    }

    /// Recover a stream that QEMU has stopped consuming (underrun
    /// auto-disable, see reconfigure_stream caller docs): full teardown +
    /// bring-up with the last-configured params. QEMU completes all
    /// in-flight ring buffers on STOP/RELEASE, so the caller's next
    /// drain refills the ring and audio resumes. Must be called while data
    /// is queued to flow — a revived stream that underruns again dies again.
    pub fn recover_stream(&mut self) {
        pci::serial_debug("[SND] TX stalled t_ms=");
        pci::serial_debug_hex((monotonic_us() / 1000) as u32);
        pci::serial_debug(" level=");
        pci::serial_debug_hex(self.tx_level() as u32);
        pci::serial_debug(" min_level=");
        pci::serial_debug_hex(self.dbg_min_level as u32);
        pci::serial_debug(" — recovering stream\n");
        self.dbg_min_level = 0xFFFF;

        let (f, c) = (self.last_freq, self.last_channels);
        self.reconfigure_stream(0, f, c);
    }

    /// Sends one control command and returns the device status code
    /// (VIRTIO_SND_S_OK = 0x8000), or 0xFFFF_FFFF on ctrl-queue timeout.
    fn send_control_cmd<T>(&mut self, cmd: &T) -> u32 {
        let code = unsafe { *(cmd as *const T as *const u32) };
        pci::rdebug("[SND] CTRL CMD "); pci::rdebug_hex(code); pci::rdebug(" -> ");

        let vq_id;
        let notify_off;
        let _head = {
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
                atomic::fence(Ordering::SeqCst);
                core::ptr::write_volatile(&mut (*vq.avail).idx, vq.last_avail_idx);
                atomic::fence(Ordering::SeqCst);
                h
            }
        };
        unsafe {
            let addr = self.notify_cfg + (notify_off as u32 * self.notify_off_multiplier) as usize;
            core::ptr::write_volatile(addr as *mut u16, vq_id);
            let vq = self.vqs[0].as_mut().unwrap();
            let mut timeout = 5000000;
            while vq.last_used_idx == core::ptr::read_volatile(&(*vq.used).idx) && timeout > 0 { core::hint::spin_loop(); timeout -= 1; }
            if timeout == 0 {
                pci::serial_debug("[SND] CTRL CMD ");
                pci::serial_debug_hex(code);
                pci::serial_debug(" TIMEOUT\n");
                return 0xFFFF_FFFF;
            }
            while vq.last_used_idx != core::ptr::read_volatile(&(*vq.used).idx) {
                vq.last_used_idx = vq.last_used_idx.wrapping_add(1);
                vq.num_free += 2;
            }
            let s = core::ptr::read_volatile(&(*self.persistent).ctrl_status.code);
            pci::rdebug_hex(s); pci::rdebug("\n");
            s
        }
    }

    /// Non-blocking PCM transmission. Returns bytes actually queued.
    pub fn send_pcm_data(&mut self, data: &[u8]) -> usize {
        if !self.initialized { return 0; }
        
        let dbg_first = !self.dbg_first_completion;
        let vq = self.vqs[2].as_mut().unwrap();
        // Reclaim processed descriptors
        let used = unsafe { core::ptr::read_volatile(&(*vq.used).idx) };
        if dbg_first && vq.last_used_idx != used {
            let slot = vq.last_used_idx as usize % QUEUE_SIZE;
            let st = unsafe { core::ptr::read_volatile(&(*self.persistent).tx_status[slot].status) };
            pci::serial_debug("[SND] first TX completion: status=");
            pci::serial_debug_hex(st);
            pci::serial_debug("\n");
            self.dbg_first_completion = true;
        }
        while vq.last_used_idx != used {
            vq.last_used_idx = vq.last_used_idx.wrapping_add(1);
            vq.num_free += 3;
        }
        let level_now = vq.last_avail_idx.wrapping_sub(vq.last_used_idx);
        if level_now < self.dbg_min_level { self.dbg_min_level = level_now; }

        // In-flight cap: each queued buffer is ~2.9 ms of audio, so the cap
        // (not the 256-descriptor ring) sets the hardware-side latency:
        // TX_MAX_INFLIGHT × 512 B. Must stay above TX_TOPUP_BUFS.
        if vq.num_free < 3 || level_now >= TX_MAX_INFLIGHT {
            if !self.dbg_ring_full {
                pci::serial_debug("[SND] TX ring full (first time), submitted=");
                pci::serial_debug_hex(self.tx_count);
                pci::serial_debug("\n");
                self.dbg_ring_full = true;
            }
            return 0;
        }
        
        let chunk_len = data.len().min(512);
        let vq_id = vq.id;
        let notify_off = vq.notify_off;
        unsafe {
            let slot = vq.last_avail_idx as usize % QUEUE_SIZE;
            (*self.persistent).tx_xfer[slot].stream_id = 0;
            core::ptr::copy_nonoverlapping(data.as_ptr(), (*self.persistent).tx_data[slot].as_mut_ptr(), chunk_len);
            self.tx_len[slot] = chunk_len as u16;
            
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
            atomic::fence(Ordering::SeqCst);
            core::ptr::write_volatile(&mut (*vq.avail).idx, vq.last_avail_idx);
            atomic::fence(Ordering::SeqCst);
        };
        unsafe {
            let addr = self.notify_cfg + (notify_off as u32 * self.notify_off_multiplier) as usize;
            core::ptr::write_volatile(addr as *mut u16, vq_id);
        }
        
        self.tx_count += 1;
        if self.tx_count % 1000 == 0 {
            pci::rdebug("[SND] TX pkts: "); pci::rdebug_hex(self.tx_count); pci::rdebug("\n");
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
