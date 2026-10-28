//! Isochronous pipes: continuous OUT (playback) and IN (capture) streams.
//!
//! Built like the interrupt pipes in `intr.rs`: an [`IsochPipe`] keeps
//! [`NTDS`] one-service-interval TDs queued on its endpoint at all times and
//! is *harvested* by [`IsochPipe::poll`], which only ever **try**-locks the
//! controller, never waits and never allocates, so a 100 Hz tick (IRQ
//! context) can drive it. Between the class driver and the TDs sits a byte
//! FIFO: [`IsochPipe::write`] (OUT) and [`IsochPipe::read`] (IN) touch only
//! the FIFO, so they need no controller lock at all.
//!
//! Scheduling: every TD is an Isoch TRB with SIA (Start Isoch ASAP), so the
//! controller places it in the service interval after the previous one, and
//! after a gap (ring underrun, missed service) in the next free interval.
//! The Frame ID is never used. Per service interval an OUT TD carries
//! `rate × interval` frames, accumulated exactly (44.1 kHz at 1 ms
//! alternates 44 and 45 frames), capped at the endpoint's Max ESIT Payload.
//!
//! Error handling (xHCI §4.10.3.1, §4.11.2.5): isochronous endpoints never
//! halt on transfer errors. A Ring Underrun (OUT) / Overrun (IN) leaves the
//! endpoint running; the next doorbell resumes it and SIA re-anchors the
//! schedule. A Missed Service Error means the controller skipped TDs; TDs
//! complete strictly in ring order, so any queued TD older than a completed
//! one is accounted as missed even if its own event never arrived. Per-TD
//! transaction / babble / buffer errors are counted and the stream goes on.
//! Counters are global ([`stats`]) and reported by the kernel's iostat
//! window as `isoch_underrun` / `isoch_missed`.
//!
//! Cache maintenance (non-coherent aarch64): each TD owns a cache-line
//! aligned, power-of-two slot of the endpoint's bounce buffer; an OUT slot is
//! cleaned after it is filled, an IN slot is cleaned before it is queued and
//! invalidated before it is read — always just that slot's range.
//!
//! Opening a stream (endpoint context, Configure Endpoint, SET_INTERFACE) and
//! closing it are task-context operations ([`IsochPipe::open`],
//! [`IsochPipe::close`]); everything in between is tick-safe.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use super::*;

/// TDs (service intervals) kept queued per isochronous endpoint: 64 ms of
/// audio at FS 1 ms intervals, enough to ride over a few skipped 10 ms ticks.
pub const NTDS: usize = 64;
/// Most TDs one pipe may keep queued (the transfer ring holds 256 TRBs,
/// one of them the Link TRB).
pub const NTDS_MAX: usize = 240;
/// Default size of the byte FIFO between the class driver and the TDs.
pub const FIFO_BYTES: usize = 64 * 1024;
/// OUT: with the FIFO dry, silence is queued only while fewer than this many
/// TDs are in flight (keeps the device clocked without adding 64 ms of
/// latency, and never pads while real data is still ahead of the device).
pub const LOW_TDS: usize = 16;

pub static ISOCH_TDS: AtomicU64 = AtomicU64::new(0);
pub static ISOCH_UNDERRUN: AtomicU64 = AtomicU64::new(0);
pub static ISOCH_MISSED: AtomicU64 = AtomicU64::new(0);
pub static ISOCH_SILENCE: AtomicU64 = AtomicU64::new(0);
pub static ISOCH_ERRORS: AtomicU64 = AtomicU64::new(0);

/// Global isochronous counters, summed over every pipe since boot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IsochStats {
    /// TDs completed (successfully or with a per-TD error).
    pub tds: u64,
    /// Ring Underrun (OUT) / Ring Overrun (IN) events.
    pub underrun: u64,
    /// TDs the controller skipped (Missed Service Error or no event at all).
    pub missed: u64,
    /// OUT TDs padded with silence because the FIFO ran dry mid-stream.
    pub silence: u64,
    /// Per-TD transfer errors (transaction, babble, buffer overrun …).
    pub errors: u64,
}

pub fn stats() -> IsochStats {
    IsochStats {
        tds: ISOCH_TDS.load(Ordering::Relaxed),
        underrun: ISOCH_UNDERRUN.load(Ordering::Relaxed),
        missed: ISOCH_MISSED.load(Ordering::Relaxed),
        silence: ISOCH_SILENCE.load(Ordering::Relaxed),
        errors: ISOCH_ERRORS.load(Ordering::Relaxed),
    }
}

// ── descriptors ─────────────────────────────────────────────────────────────

/// One isochronous endpoint of one interface alternate setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IsochEpDesc {
    pub iface: u8,
    pub alt: u8,
    pub class: u8,
    pub subclass: u8,
    pub ep_addr: u8,
    /// bmAttributes (bits 3:2 sync type, 5:4 usage).
    pub attrs: u8,
    /// Raw wMaxPacketSize (HS: bits 12:11 are additional transactions).
    pub mps: u16,
    pub interval: u8,
    /// SuperSpeed companion: bMaxBurst, Mult, wBytesPerInterval.
    pub ss_burst: u8,
    pub ss_mult: u8,
    pub ss_bytes: u16,
}

impl IsochEpDesc {
    pub fn is_in(&self) -> bool { self.ep_addr & 0x80 != 0 }
    /// A feedback endpoint (usage type 01), not a data stream.
    pub fn is_feedback(&self) -> bool { (self.attrs >> 4) & 3 == 1 }
}

/// Every isochronous endpoint in a configuration descriptor, in order.
pub fn scan_isoch(cfg: &[u8]) -> Vec<IsochEpDesc> {
    let mut out: Vec<IsochEpDesc> = Vec::new();
    let (mut iface, mut alt, mut class, mut sub) = (0u8, 0u8, 0u8, 0u8);
    let mut last_isoch = false;
    let mut i = 0usize;
    while i + 2 <= cfg.len() {
        let len = cfg[i] as usize;
        if len < 2 || i + len > cfg.len() { break; }
        let d = &cfg[i..i + len];
        match d[1] {
            4 if len >= 9 => {
                iface = d[2]; alt = d[3]; class = d[5]; sub = d[6];
                last_isoch = false;
            }
            5 if len >= 7 => {
                last_isoch = d[3] & 3 == 1;
                if last_isoch {
                    out.push(IsochEpDesc {
                        iface, alt, class, subclass: sub, ep_addr: d[2], attrs: d[3],
                        mps: u16::from_le_bytes([d[4], d[5]]), interval: d[6],
                        ..Default::default()
                    });
                }
            }
            0x30 if len >= 6 && last_isoch => {
                if let Some(e) = out.last_mut() {
                    e.ss_burst = d[2];
                    e.ss_mult = d[3] & 3;
                    e.ss_bytes = u16::from_le_bytes([d[4], d[5]]);
                }
            }
            _ => {}
        }
        i += len;
    }
    out
}

/// Whether the configuration has an audio streaming interface (class 01,
/// subclass 02) with an isochronous endpoint.
pub fn has_audio_streaming(cfg: &[u8]) -> bool {
    scan_isoch(cfg).iter().any(|e| e.class == 0x01 && e.subclass == 0x02)
}

// ── the configured function ─────────────────────────────────────────────────

/// A configured device function a class driver opens isochronous streams on.
pub struct UsbFunction {
    pub hc: Arc<Hc>,
    pub slot: u8,
    pub speed: u8,
    pub config_value: u8,
    /// The full configuration descriptor (for class-specific parsing).
    pub cfg: Vec<u8>,
}

impl UsbFunction {
    pub fn isoch_endpoints(&self) -> Vec<IsochEpDesc> { scan_isoch(&self.cfg) }

    /// A class/vendor request with an OUT (or no) data stage. Task context.
    pub fn control_out(&self, req_type: u8, req: u8, value: u16, index: u16, data: &[u8]) -> Result<(), UsbErr> {
        let d = if data.is_empty() { CtrlData::None } else { CtrlData::Out(data) };
        control(&self.hc, self.slot, req_type & 0x7F, req, value, index, d, CLASS_CTRL_TIMEOUT_MS).map(|_| ())
    }

    /// A request with an IN data stage; returns the bytes received. Task context.
    pub fn control_in(&self, req_type: u8, req: u8, value: u16, index: u16, buf: &mut [u8]) -> Result<usize, UsbErr> {
        control(&self.hc, self.slot, req_type | 0x80, req, value, index, CtrlData::In(buf), CLASS_CTRL_TIMEOUT_MS)
    }

    /// SET_INTERFACE. Task context.
    pub fn set_interface(&self, iface: u8, alt: u8) -> Result<(), UsbErr> {
        control(&self.hc, self.slot, 0x01, 11, alt as u16, iface as u16, CtrlData::None, CLASS_CTRL_TIMEOUT_MS).map(|_| ())
    }

    pub fn connected(&self) -> bool { self.hc.lock().port_connected(self.slot) }
}

// ── the pipe ────────────────────────────────────────────────────────────────

/// Stream format for an OUT pipe's packet sizing. `frame_bytes == 0` sends
/// full Max ESIT Payload packets (raw byte streams).
#[derive(Clone, Copy, Debug)]
pub struct StreamFormat {
    pub rate: u32,
    pub frame_bytes: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State { Running, Dead }

/// What one poll found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Poll {
    Ok,
    /// The controller lock was busy; try again next tick.
    Busy,
    /// The device is gone or the controller died. Reported once.
    Gone,
}

#[derive(Clone, Copy)]
struct Pending { ptr: u64, idx: u16, len: u16 }

/// One isochronous endpoint with TDs permanently queued on it.
pub struct IsochPipe {
    hc: Arc<Hc>,
    slot: u8,
    dci: usize,
    ep_addr: u8,
    iface: u8,
    dir_in: bool,
    speed: u8,
    /// Bytes per TD slot of the bounce buffer (power of two, >= 64).
    chunk: usize,
    mps: u32,
    burst: u32,
    esit: u32,
    /// Service interval in microseconds.
    ival_us: u32,
    fmt: StreamFormat,
    /// Remainder of the frames-per-interval accumulator (units: frames·µs).
    acc: u64,
    pending: Vec<Pending>,
    next_idx: usize,
    /// TDs kept queued (and bounce-buffer slots).
    ntds: usize,
    /// IN: the FIFO holds `[len u16 LE][data]` records, one per TD.
    framed: bool,
    fifo: Vec<u8>,
    /// Monotonic FIFO counters (bytes written / consumed).
    wr: u64,
    rd: u64,
    /// Bytes of completed TDs (OUT: sent; IN: received).
    done_bytes: u64,
    /// OUT: the producer has written at least once (silence before that is
    /// priming, not an underrun).
    flowing: bool,
    state: State,
    reported_gone: bool,
    /// Per-pipe counters (the globals are sums over all pipes).
    pub local: IsochStats,
}

impl IsochPipe {
    pub fn slot(&self) -> u8 { self.slot }
    pub fn endpoint(&self) -> u8 { self.ep_addr }
    pub fn interface(&self) -> u8 { self.iface }
    pub fn is_in(&self) -> bool { self.dir_in }
    /// Max bytes one service interval carries.
    pub fn max_packet(&self) -> u32 { self.esit }
    pub fn interval_us(&self) -> u32 { self.ival_us }
    /// Bytes waiting in the FIFO (OUT: not yet in a TD; IN: not yet read).
    pub fn queued(&self) -> usize { (self.wr - self.rd) as usize }
    /// FIFO space left for [`write`](Self::write).
    pub fn space(&self) -> usize { self.fifo.len() - self.queued() }
    /// Bytes of completed TDs: the stream's play (OUT) / capture (IN) position.
    pub fn done_bytes(&self) -> u64 { self.done_bytes }
    /// Bytes handed to the controller but not completed yet.
    pub fn in_flight(&self) -> usize { self.pending.iter().map(|p| p.len as usize).sum() }
    pub fn is_dead(&self) -> bool { self.state == State::Dead }

    /// Configure endpoint `d` of `f` (bandwidth via Configure Endpoint, then
    /// SET_INTERFACE to its alternate setting) and return its pipe, not yet
    /// primed: the first [`poll`](Self::poll) queues the TDs. `fifo_bytes`
    /// is rounded up to a power of two. Task context.
    pub fn open(f: &UsbFunction, d: &IsochEpDesc, fmt: StreamFormat, fifo_bytes: usize)
                -> Result<IsochPipe, &'static str> {
        Self::open_ex(f, d, fmt, fifo_bytes, NTDS, false)
    }

    /// [`open`](Self::open) with `ntds` TDs kept queued (clamped to
    /// 8..=[`NTDS_MAX`]) and, for IN, `framed`: the FIFO keeps per-TD packet
    /// boundaries (one record per service interval, read back with
    /// [`read_packet`](Self::read_packet)) — what UVC needs, since every
    /// payload starts with its own header. A framed FIFO that is full drops
    /// the new packet (counted as missed) instead of overwriting old data.
    pub fn open_ex(f: &UsbFunction, d: &IsochEpDesc, fmt: StreamFormat, fifo_bytes: usize,
                   ntds: usize, framed: bool) -> Result<IsochPipe, &'static str> {
        let ntds = ntds.clamp(8, NTDS_MAX);
        let hc = &f.hc;
        let (slot, speed) = (f.slot, f.speed);
        let dir_in = d.is_in();
        let dci = dci_of(d.ep_addr);
        let mps = (d.mps & 0x7FF) as u32;
        if mps == 0 { return Err("isochronous endpoint with zero max packet size"); }
        // HS: additional transactions per µframe go in Max Burst (§6.2.3.4).
        // SS isoch Mult > 0 is not supported (Mult stays 0 in the context).
        let burst = if speed >= SPEED_SS { d.ss_burst as u32 } else if speed == SPEED_HS { ((d.mps >> 11) & 3) as u32 } else { 0 };
        let esit = if speed >= SPEED_SS && d.ss_bytes != 0 {
            (d.ss_bytes as u32).min(mps * (burst + 1))
        } else {
            mps * (burst + 1)
        };
        let interval = interval_field(speed, true, d.interval);
        let ival_us = 125u32 << interval.min(15);
        let chunk = (esit as usize).next_power_of_two().max(64);
        let (dma32, cmd_ok) = {
            let mut x = hc.lock();
            let dma32 = x.dma32;
            let s = x.slot_mut(slot).ok_or("slot vanished")?;
            if s.gone { return Err("device gone"); }
            if s.eps.get(dci).map_or(true, |e| e.is_some()) { return Err("endpoint already configured"); }
            let ring = ProducerRing::new(dma32).ok_or("oom: isoch ring")?;
            let bounce = DmaBuf::new(ntds * chunk, dma32).ok_or("oom: isoch buffers")?;
            let ty = if dir_in { EP_TYPE_ISOCH_IN } else { EP_TYPE_ISOCH_OUT };
            let mut ep = Endpoint::new(ring, ty, mps as u16, burst as u8, interval, esit.min(0xFFFF) as u16);
            ep.esit = esit;
            ep.bounce = Some(bounce);
            ep.events = Vec::with_capacity(2 * ntds + 8);
            s.eps[dci] = Some(ep);
            (dma32, x.build_configure_input(slot, 0, 1 << dci))
        };
        let _ = dma32;
        let fail = |why: &'static str| -> Result<IsochPipe, &'static str> {
            if let Some(s) = hc.lock().slot_mut(slot) { s.eps[dci] = None; }
            Err(why)
        };
        let Some(in_phys) = cmd_ok else { return fail("slot vanished") };
        match run_cmd(hc, Trb::new(TRB_CONFIGURE_EP, in_phys, 0, (slot as u32) << 24)) {
            Ok((CC_SUCCESS, _)) => {}
            Ok((cc, _)) => {
                usb_log!("[USB-ISOCH] slot {} ep {:02x}: Configure Endpoint cc={}{}\n", slot, d.ep_addr, cc,
                         if cc == CC_BANDWIDTH { " (not enough bandwidth)" } else { "" });
                return fail("Configure Endpoint failed");
            }
            Err(_) => return fail("Configure Endpoint timed out"),
        }
        if control(hc, slot, 0x01, 11, d.alt as u16, d.iface as u16, CtrlData::None, ENUM_CTRL_TIMEOUT_MS).is_err() {
            let _ = drop_endpoint(hc, slot, dci);
            return Err("SET_INTERFACE failed");
        }
        usb_log!("[USB-ISOCH] slot {} ep {:02x} ({}) iface {} alt {}: mps {} burst {} esit {} interval {} us, {} TDs of {} B\n",
                 slot, d.ep_addr, if dir_in { "in" } else { "out" }, d.iface, d.alt, mps, burst, esit, ival_us, ntds, chunk);
        Ok(IsochPipe {
            hc: hc.clone(), slot, dci, ep_addr: d.ep_addr, iface: d.iface, dir_in, speed, chunk, mps, burst, esit,
            ival_us, fmt, acc: 0, pending: Vec::with_capacity(ntds), next_idx: 0, ntds, framed: framed && dir_in,
            fifo: vec![0u8; fifo_bytes.max(2 * chunk).next_power_of_two()], wr: 0, rd: 0, done_bytes: 0,
            flowing: false, state: State::Running, reported_gone: false, local: IsochStats::default(),
        })
    }

    /// OUT: stop queueing TDs once the FIFO is empty (end of stream); the
    /// ring drains and the next [`write`](Self::write) restarts it.
    pub fn pause(&mut self) { self.flowing = false; }

    /// OUT: append to the FIFO; returns how many bytes fit. No lock, no wait.
    pub fn write(&mut self, data: &[u8]) -> usize {
        if self.dir_in { return 0; }
        let n = data.len().min(self.space());
        let mask = self.fifo.len() - 1;
        let mut off = (self.wr as usize) & mask;
        let mut src = &data[..n];
        while !src.is_empty() {
            let k = src.len().min(self.fifo.len() - off);
            self.fifo[off..off + k].copy_from_slice(&src[..k]);
            src = &src[k..];
            off = (off + k) & mask;
        }
        self.wr += n as u64;
        if n > 0 { self.flowing = true; }
        n
    }

    /// IN: take captured bytes from the FIFO. No lock, no wait.
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        if !self.dir_in { return 0; }
        let n = out.len().min(self.queued());
        self.fifo_take(&mut out[..n]);
        n
    }

    /// IN, framed: take the oldest packet (one service interval's data).
    /// `Some(n)` is the packet's length; bytes beyond `out.len()` are
    /// dropped. `None` when no packet is queued. No lock, no wait.
    pub fn read_packet(&mut self, out: &mut [u8]) -> Option<usize> {
        if !self.dir_in || !self.framed || self.queued() < 2 { return None; }
        let mut h = [0u8; 2];
        self.fifo_take(&mut h);
        let n = u16::from_le_bytes(h) as usize;
        let k = n.min(out.len());
        self.fifo_take(&mut out[..k]);
        self.rd += (n - k) as u64;
        Some(n)
    }

    fn fifo_take(&mut self, out: &mut [u8]) {
        let mask = self.fifo.len() - 1;
        let mut off = (self.rd as usize) & mask;
        let mut i = 0;
        while i < out.len() {
            let k = (out.len() - i).min(self.fifo.len() - off);
            out[i..i + k].copy_from_slice(&self.fifo[off..off + k]);
            i += k;
            off = (off + k) & mask;
        }
        self.rd += out.len() as u64;
    }

    /// Bytes the next OUT service interval carries.
    fn next_out_len(&mut self) -> u32 {
        let fb = self.fmt.frame_bytes;
        if fb == 0 || self.fmt.rate == 0 { return self.esit; }
        let total = self.acc + self.fmt.rate as u64 * self.ival_us as u64;
        let frames = (total / 1_000_000) as u32;
        self.acc = total % 1_000_000;
        (frames * fb).min(self.esit / fb * fb)
    }

    /// Isoch TRB control flags: TBC / TLBPC (xHCI §4.11.2.3, Linux's
    /// `xhci_get_burst_count` / `xhci_get_last_burst_packet_count`).
    fn burst_fields(&self, hci_version: u16, len: u32) -> u32 {
        if hci_version < 0x100 { return 0; }
        let tdpc = len.div_ceil(self.mps).max(1);
        if self.speed >= SPEED_SS {
            let tbc = tdpc.div_ceil(self.burst + 1) - 1;
            let r = tdpc % (self.burst + 1);
            let tlbpc = if r == 0 { self.burst } else { r - 1 };
            (tbc & 3) << 7 | (tlbpc & 0xF) << 16
        } else {
            ((tdpc - 1) & 0xF) << 16
        }
    }

    /// Queue TDs until [`NTDS`] are on the ring. Lock held.
    fn prime(&mut self, x: &mut Xhci) {
        let hci_version = x.hci_version;
        let regs = x.regs;
        if x.ep_mut(self.slot, self.dci).is_none() { self.state = State::Dead; return; }
        let mut queued = false;
        // An OUT stream starts with the producer's first write; an idle one
        // (paused, or never started) queues nothing and drains.
        if !self.dir_in && !self.flowing { return; }
        while self.pending.len() < self.ntds {
            if !self.dir_in && (self.wr - self.rd) < self.esit as u64 && self.pending.len() >= LOW_TDS {
                break;
            }
            let idx = self.next_idx;
            let len = if self.dir_in { self.esit } else { self.next_out_len() };
            let off = idx * self.chunk;
            let ep = x.ep_mut(self.slot, self.dci).unwrap();
            let Some(buf) = ep.bounce.as_ref() else { self.state = State::Dead; return };
            if self.dir_in {
                buf.sync_range_for_device(off, self.chunk);
            } else {
                // FIFO → TD slot, the shortfall as silence.
                let have = (self.wr - self.rd) as usize;
                let n = have.min(len as usize);
                if n > 0 {
                    let mask = self.fifo.len() - 1;
                    let start = (self.rd as usize) & mask;
                    let k = n.min(self.fifo.len() - start);
                    buf.copy_in(off, &self.fifo[start..start + k]);
                    if k < n { buf.copy_in(off + k, &self.fifo[..n - k]); }
                    self.rd += n as u64;
                }
                if n < len as usize {
                    buf.zero(off + n, len as usize - n);
                    self.local.silence += 1;
                    ISOCH_SILENCE.fetch_add(1, Ordering::Relaxed);
                }
                buf.sync_range_for_device(off, self.chunk);
            }
            let base = buf.phys();
            let flags = TRB_IOC | TRB_SIA | self.burst_fields(hci_version, len)
                | if self.dir_in { TRB_ISP } else { 0 };
            let t = Trb::new(TRB_ISOCH, base + off as u64, len, flags);
            let mut p = [0u64; 1];
            ep.ring.enqueue_td(&[t], &mut p);
            self.pending.push(Pending { ptr: p[0], idx: idx as u16, len: len as u16 });
            self.next_idx = (idx + 1) % self.ntds;
            queued = true;
        }
        if queued { regs.doorbell(self.slot, self.dci as u32); }
    }

    fn count(&mut self, missed: u64, underrun: u64, errors: u64, tds: u64) {
        self.local.missed += missed;
        self.local.underrun += underrun;
        self.local.errors += errors;
        self.local.tds += tds;
        if missed != 0 { ISOCH_MISSED.fetch_add(missed, Ordering::Relaxed); }
        if underrun != 0 { ISOCH_UNDERRUN.fetch_add(underrun, Ordering::Relaxed); }
        if errors != 0 { ISOCH_ERRORS.fetch_add(errors, Ordering::Relaxed); }
        if tds != 0 { ISOCH_TDS.fetch_add(tds, Ordering::Relaxed); }
    }

    /// Retire completed TDs (copying IN data to the FIFO) and keep the ring
    /// full. IRQ-safe: try_lock only, no waits, no allocation.
    pub fn poll(&mut self) -> Poll {
        if self.state == State::Dead {
            if self.reported_gone { return Poll::Ok; }
            self.reported_gone = true;
            return Poll::Gone;
        }
        let hc = self.hc.clone();
        let Some(mut x) = hc.try_lock() else { return Poll::Busy };
        x.drain_events();
        if x.dead || !x.port_connected(self.slot) {
            self.state = State::Dead;
            self.reported_gone = true;
            return Poll::Gone;
        }
        self.harvest(&mut x);
        if self.state == State::Running { self.prime(&mut x); }
        Poll::Ok
    }

    fn harvest(&mut self, x: &mut Xhci) {
        let Some(ep) = x.ep_mut(self.slot, self.dci) else { self.state = State::Dead; return };
        if ep.events.is_empty() { return; }
        let Endpoint { events, bounce, .. } = ep;
        let Some(buf) = bounce.as_ref() else { self.state = State::Dead; return };
        let (mut missed, mut underrun, mut errors, mut tds) = (0u64, 0u64, 0u64, 0u64);
        for ev in events.iter() {
            if ev.cc == CC_RING_UNDERRUN || ev.cc == CC_RING_OVERRUN {
                // The ring ran empty: every TD before this point is done, so
                // one still pending lost its event (counted as skipped).
                // An idle OUT stream draining its ring is not an underrun.
                if self.dir_in || self.flowing { underrun += 1; }
                missed += self.pending.len() as u64;
                self.pending.clear();
                continue;
            }
            let Some(k) = self.pending.iter().position(|p| p.ptr == ev.ptr) else {
                if ev.cc == CC_MISSED_SERVICE { missed += 1; }
                continue;
            };
            // In-order completion: TDs ahead of this one were skipped.
            missed += k as u64;
            let p = self.pending[k];
            self.pending.drain(..=k);
            tds += 1;
            match ev.cc {
                CC_SUCCESS | CC_SHORT => {
                    let got = (p.len as u32).saturating_sub(ev.residual) as usize;
                    self.done_bytes += if self.dir_in { got as u64 } else { p.len as u64 };
                    if self.dir_in && got > 0 {
                        let off = p.idx as usize * self.chunk;
                        buf.sync_range_for_cpu(off, self.chunk);
                        let space = self.fifo.len() - (self.wr - self.rd) as usize;
                        let mask = self.fifo.len() - 1;
                        if self.framed {
                            // A record must stay whole: drop the new packet when full.
                            if got + 2 > space { missed += 1; continue; }
                            let h = (got as u16).to_le_bytes();
                            for b in h {
                                let o = (self.wr as usize) & mask;
                                self.fifo[o] = b;
                                self.wr += 1;
                            }
                        } else if got > space {
                            // Keep the newest data when the reader falls behind.
                            self.rd += (got - space) as u64;
                        }
                        let start = (self.wr as usize) & mask;
                        let k1 = got.min(self.fifo.len() - start);
                        buf.copy_out(off, &mut self.fifo[start..start + k1]);
                        if k1 < got { buf.copy_out(off + k1, &mut self.fifo[..got - k1]); }
                        self.wr += got as u64;
                    }
                }
                CC_MISSED_SERVICE => missed += 1,
                CC_STOPPED => {}
                _ => errors += 1,
            }
        }
        events.clear();
        self.count(missed, underrun, errors, tds);
    }

    /// Stop the endpoint, select alternate setting 0 and drop the endpoint
    /// (freeing its ring and buffers). Task context.
    pub fn close(self) {
        let (hc, slot, dci) = (self.hc.clone(), self.slot, self.dci);
        if self.state != State::Dead || hc.lock().port_connected(slot) {
            let _ = run_cmd(&hc, ep_cmd(TRB_STOP_EP, slot, dci));
            let _ = control(&hc, slot, 0x01, 11, 0, self.iface as u16, CtrlData::None, ENUM_CTRL_TIMEOUT_MS);
        }
        let _ = drop_endpoint(&hc, slot, dci);
    }
}

/// Drop endpoint `dci` (Configure Endpoint with its drop flag) and free it.
fn drop_endpoint(hc: &Arc<Hc>, slot: u8, dci: usize) -> Result<(), &'static str> {
    let (ep, in_phys) = {
        let mut x = hc.lock();
        let ep = x.slot_mut(slot).and_then(|s| s.eps.get_mut(dci)).and_then(|e| e.take());
        let p = x.build_configure_input(slot, 1 << dci, 0);
        (ep, p)
    };
    let r = match in_phys {
        Some(p) if hc.lock().port_connected(slot) =>
            run_cmd(hc, Trb::new(TRB_CONFIGURE_EP, p, 0, (slot as u32) << 24)).map(|_| ()).map_err(|_| "timeout"),
        _ => Ok(()),
    };
    // The controller no longer owns the ring / buffers once the drop
    // completed. If it timed out, leak them rather than free live DMA memory.
    if r.is_err() { core::mem::forget(ep); } else { drop(ep); }
    r
}

// ── registry and self-test ──────────────────────────────────────────────────

/// Audio functions found at enumeration, waiting for a class driver.
static AUDIO: Mutex<Vec<UsbFunction>> = Mutex::new(Vec::new());

/// Hand an enumerated audio function to the registry (and to the self-test
/// when it is enabled). Task context.
pub fn register_audio(f: UsbFunction) {
    if SELFTEST_MS.load(Ordering::Relaxed) != 0 && selftest_start(&f) { return; }
    AUDIO.lock().push(f);
}

/// Take the oldest registered audio function (a class driver binding it).
pub fn take_audio() -> Option<UsbFunction> {
    let mut a = AUDIO.lock();
    if a.is_empty() { None } else { Some(a.remove(0)) }
}

/// Number of audio functions waiting for a class driver.
pub fn audio_count() -> usize { AUDIO.lock().len() }

/// The device on (`hc`, `slot`) left: forget its function and test stream.
pub fn detach(hc: &Hc, slot: u8) {
    AUDIO.lock().retain(|f| !(core::ptr::eq(&*f.hc, hc) && f.slot == slot));
    let mut t = TEST.lock();
    if t.as_ref().is_some_and(|s| core::ptr::eq(&*s.pipe.hc, hc) && s.pipe.slot == slot) {
        *t = None;
        ACTIVE.store(false, Ordering::Release);
    }
}

/// `usb.isochtest=<ms>`: stream a 1 kHz sine (48 kHz S16 stereo, −6 dBFS)
/// to the first audio device's isochronous OUT endpoint for that long.
static SELFTEST_MS: AtomicU32 = AtomicU32::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static TEST: Mutex<Option<TestStream>> = Mutex::new(None);

struct TestStream {
    pipe: IsochPipe,
    frames_left: u64,
    phase: usize,
    reported: bool,
}

/// Parse `usb.isochtest=` from the kernel command line.
pub fn set_cmdline(cmdline: &str) {
    for tok in cmdline.split_ascii_whitespace() {
        if let Some(v) = tok.strip_prefix("usb.isochtest=") {
            if let Ok(ms) = v.parse::<u32>() { SELFTEST_MS.store(ms, Ordering::Relaxed); }
        }
    }
}

/// One period of 1 kHz at 48 kHz, amplitude 16000.
const SINE_1K: [i16; 48] = [
    0, 2088, 4141, 6123, 8000, 9740, 11314, 12694, 13856, 14782, 15455, 15863, 16000, 15863, 15455,
    14782, 13856, 12694, 11314, 9740, 8000, 6123, 4141, 2088, 0, -2088, -4141, -6123, -8000, -9740,
    -11314, -12694, -13856, -14782, -15455, -15863, -16000, -15863, -15455, -14782, -13856, -12694,
    -11314, -9740, -8000, -6123, -4141, -2088,
];
const TEST_RATE: u32 = 48_000;

fn selftest_start(f: &UsbFunction) -> bool {
    if TEST.lock().is_some() { return false; }
    let eps = f.isoch_endpoints();
    let Some(d) = eps.iter().find(|e| e.class == 0x01 && e.subclass == 0x02 && !e.is_in() && !e.is_feedback()) else {
        usb_log!("[USB-ISOCH] selftest: slot {} has no isochronous OUT stream\n", f.slot);
        return false;
    };
    // UAC1 SET_CUR SAMPLING_FREQ on the endpoint (a STALL is fine: fixed-rate).
    let rate = TEST_RATE.to_le_bytes();
    let _ = f.control_out(0x22, 0x01, 0x0100, d.ep_addr as u16, &rate[..3]);
    let pipe = match IsochPipe::open(f, d, StreamFormat { rate: TEST_RATE, frame_bytes: 4 }, FIFO_BYTES) {
        Ok(p) => p,
        Err(e) => { usb_log!("[USB-ISOCH] selftest: open failed: {}\n", e); return false; }
    };
    let ms = SELFTEST_MS.load(Ordering::Relaxed) as u64;
    usb_log!("[USB-ISOCH] selftest: streaming 1 kHz to slot {} ep {:02x} for {} ms\n", f.slot, d.ep_addr, ms);
    *TEST.lock() = Some(TestStream {
        pipe, frames_left: ms * TEST_RATE as u64 / 1000, phase: 0, reported: false,
    });
    ACTIVE.store(true, Ordering::Release);
    true
}

/// Timer-tick hook: feed and poll the self-test stream. IRQ context.
pub fn tick() {
    if !ACTIVE.load(Ordering::Acquire) { return; }
    let Some(mut g) = TEST.try_lock() else { return };
    let Some(t) = g.as_mut() else { return };
    // Keep ~40 ms in the FIFO ahead of the TDs.
    let target = (TEST_RATE as usize / 1000) * 4 * 40;
    let mut buf = [0u8; 4 * 48];
    while t.frames_left > 0 && t.pipe.queued() < target {
        let n = (t.frames_left as usize).min(48);
        for i in 0..n {
            let s = SINE_1K[(t.phase + i) % 48].to_le_bytes();
            buf[4 * i..4 * i + 4].copy_from_slice(&[s[0], s[1], s[0], s[1]]);
        }
        t.phase = (t.phase + n) % 48;
        t.frames_left -= n as u64;
        t.pipe.write(&buf[..4 * n]);
    }
    // All test data is in TDs: the silence that follows is not an underrun.
    if t.frames_left == 0 && t.pipe.queued() == 0 { t.pipe.pause(); }
    match t.pipe.poll() {
        Poll::Gone => {
            usb_log!("[USB-ISOCH] selftest: device gone\n");
            *g = None;
            ACTIVE.store(false, Ordering::Release);
            return;
        }
        _ => {}
    }
    // Done once the data and its TDs have all gone out: report once (the
    // pipe keeps streaming silence afterwards).
    if !t.reported && t.frames_left == 0 && t.pipe.queued() == 0
        && t.pipe.done_bytes() >= SELFTEST_MS.load(Ordering::Relaxed) as u64 * TEST_RATE as u64 / 1000 * 4 {
        t.reported = true;
        let l = t.pipe.local;
        usb_log!("[USB-ISOCH] selftest done: {} B in {} TDs, underrun {}, missed {}, silence {}, errors {}\n",
                 t.pipe.done_bytes(), l.tds, l.underrun, l.missed, l.silence, l.errors);
    }
}
