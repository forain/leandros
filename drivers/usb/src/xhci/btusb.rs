//! Bluetooth HCI over USB (the btusb transport, Bluetooth Core spec vol 4
//! part B): HCI commands go out as class requests on EP0, events come in on
//! the interrupt IN endpoint, ACL data on the bulk pair. Interface 0
//! (class E0 / subclass 01 / protocol 01, alt 0) carries all three; the
//! isochronous SCO interface 1 is not used.
//!
//! Enumeration only sends SET_CONFIGURATION and hands the function over
//! ([`register`]); [`BtUsb::open`] configures the three endpoints and keeps
//! [`NTDS`] one-packet TDs queued on each IN endpoint at all times (like the
//! HID interrupt pipes). [`BtUsb::poll`] never blocks: it try-locks the
//! controller, harvests completed TDs and re-queues them. Packets are
//! returned as raw USB chunks; HCI packet reassembly is the caller's job
//! (drivers/src/bt/usb_reasm.rs), so a packet split over several USB
//! packets, or several packets in one, both work.
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;

use super::isoch::UsbFunction;
use super::*;

/// TDs kept queued per IN endpoint.
pub const NTDS: usize = 16;
const CMD_TIMEOUT_MS: u32 = 2000;
const MAX_RECOVERIES: u32 = 32;

/// The HCI interface's endpoints (addresses with the direction bit).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct BtEps {
    pub iface: u8,
    pub intr_in: u8,
    pub intr_mps: u16,
    pub intr_ival: u8,
    pub bulk_in: u8,
    pub bulk_in_mps: u16,
    pub bulk_out: u8,
    pub bulk_out_mps: u16,
}

impl BtEps {
    fn complete(&self) -> bool { self.intr_in != 0 && self.bulk_in != 0 && self.bulk_out != 0 }
}

/// Finds the primary Bluetooth HCI interface (E0/01/01, alt 0) and its
/// interrupt IN + bulk IN/OUT endpoints in a configuration descriptor.
pub fn scan_bt(cfg: &[u8]) -> Option<BtEps> {
    let mut cur: Option<BtEps> = None;
    let mut off = 0usize;
    while off + 2 <= cfg.len() {
        let len = cfg[off] as usize;
        if len < 2 || off + len > cfg.len() { break; }
        let b = &cfg[off..off + len];
        match b[1] {
            crate::descriptor::DT_INTERFACE if len >= 9 => {
                if let Some(c) = cur.take() { if c.complete() { return Some(c); } }
                if b[3] == 0 && b[5] == 0xE0 && b[6] == 0x01 && b[7] == 0x01 {
                    cur = Some(BtEps { iface: b[2], ..Default::default() });
                }
            }
            crate::descriptor::DT_ENDPOINT if len >= 7 => {
                if let Some(c) = cur.as_mut() {
                    let (addr, ty) = (b[2], b[3] & 3);
                    let mps = u16::from_le_bytes([b[4], b[5]]) & 0x7FF;
                    match (ty, addr & 0x80 != 0) {
                        (3, true) if c.intr_in == 0 => { c.intr_in = addr; c.intr_mps = mps; c.intr_ival = b[6]; }
                        (2, true) if c.bulk_in == 0 => { c.bulk_in = addr; c.bulk_in_mps = mps; }
                        (2, false) if c.bulk_out == 0 => { c.bulk_out = addr; c.bulk_out_mps = mps; }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        off += len;
    }
    cur.filter(|c| c.complete())
}

static BT: Mutex<Vec<UsbFunction>> = Mutex::new(Vec::new());

/// Called by enumeration for a configured Bluetooth function.
pub fn register(f: UsbFunction) { BT.lock().push(f); }

/// The next enumerated Bluetooth function not yet claimed by a driver.
pub fn take() -> Option<UsbFunction> {
    let mut v = BT.lock();
    if v.is_empty() { None } else { Some(v.remove(0)) }
}

struct InPipe {
    dci: usize,
    chunk: usize,
    pending: Vec<(u64, u8)>,
    failed: Option<u8>,
    recoveries: u32,
    /// A good completion was harvested since the last recovery.
    good: bool,
}

impl InPipe {
    fn prime(&mut self, x: &mut Xhci, slot: u8) {
        let Some(ep) = x.ep_mut(slot, self.dci) else { return };
        let Some(buf) = ep.bounce.as_ref() else { return };
        let base = buf.phys();
        let mut queued = false;
        for idx in 0..NTDS as u8 {
            if self.pending.iter().any(|p| p.1 == idx) { continue; }
            let t = Trb::new(TRB_NORMAL, base + (idx as usize * self.chunk) as u64,
                             self.chunk as u32, TRB_ISP | TRB_IOC);
            let mut p = [0u64; 1];
            ep.ring.enqueue_td(&[t], &mut p);
            self.pending.push((p[0], idx));
            queued = true;
        }
        if queued { x.regs.doorbell(slot, self.dci as u32); }
    }

    /// Completed TDs, in completion order, appended to `out` as (is_bulk, bytes).
    fn harvest(&mut self, x: &mut Xhci, slot: u8, bulk: bool, out: &mut Vec<(bool, Vec<u8>)>) {
        let chunk = self.chunk;
        let Some(ep) = x.ep_mut(slot, self.dci) else { self.failed = Some(0xFF); return };
        if ep.events.is_empty() { return; }
        let Endpoint { events, bounce, .. } = ep;
        let Some(buf) = bounce.as_ref() else { self.failed = Some(0xFF); return };
        buf.sync_for_cpu();
        for ev in events.iter() {
            let Some(k) = self.pending.iter().position(|p| p.0 == ev.ptr) else { continue };
            let (_, idx) = self.pending.remove(k);
            match ev.cc {
                CC_SUCCESS | CC_SHORT => {
                    let len = chunk.saturating_sub(ev.residual as usize);
                    if len > 0 {
                        let mut v = alloc::vec![0u8; len];
                        buf.copy_out(idx as usize * chunk, &mut v);
                        out.push((bulk, v));
                        self.good = true;
                    }
                }
                cc => { if self.failed.is_none() { self.failed = Some(cc); } }
            }
        }
        events.clear();
    }
}

/// An opened btusb function.
pub struct BtUsb {
    hc: Arc<Hc>,
    slot: u8,
    eps: BtEps,
    dci_out: usize,
    ev: InPipe,
    acl: InPipe,
    dead: bool,
    /// An IN pipe was recovered since the last `take_resets` (event, ACL).
    reset: (bool, bool),
}

/// What one [`BtUsb::poll`] saw.
pub enum BtPoll {
    /// Raw chunks: (from the bulk IN endpoint, bytes).
    Data(Vec<(bool, Vec<u8>)>),
    /// The controller was busy (another thread holds it); try again.
    Busy,
    /// Device unplugged, controller dead, or an endpoint unrecoverable.
    Gone,
}

impl BtUsb {
    pub fn endpoints(&self) -> BtEps { self.eps }
    pub fn slot(&self) -> u8 { self.slot }

    /// (events, ACL): that pipe lost data to a recovery, so its reassembler
    /// must drop any half packet. Cleared by the call.
    pub fn take_resets(&mut self) -> (bool, bool) { core::mem::take(&mut self.reset) }

    pub fn open(f: &UsbFunction) -> Result<BtUsb, &'static str> {
        let e = scan_bt(&f.cfg).ok_or("no Bluetooth HCI interface")?;
        let (hc, slot, speed) = (&f.hc, f.slot, f.speed);
        let (di, dbi, dbo) = (dci_of(e.intr_in), dci_of(e.bulk_in), dci_of(e.bulk_out));
        let ichunk = (e.intr_mps as usize).clamp(8, 1024);
        let bchunk = (e.bulk_in_mps as usize).clamp(8, 1024);
        let in_phys = {
            let mut x = hc.lock();
            let dma32 = x.dma32;
            let s = x.slot_mut(slot).ok_or("slot vanished")?;
            if s.gone { return Err("device gone"); }
            let mut ep = Endpoint::new(ProducerRing::new(dma32).ok_or("oom: ring")?, EP_TYPE_INTR_IN,
                                       e.intr_mps, 0, interval_field(speed, false, e.intr_ival), e.intr_mps.max(8));
            ep.esit = e.intr_mps as u32;
            ep.bounce = Some(DmaBuf::new((NTDS * ichunk).next_power_of_two().max(4096), dma32).ok_or("oom: buffers")?);
            s.eps[di] = Some(ep);
            let mut ep = Endpoint::new(ProducerRing::new(dma32).ok_or("oom: ring")?, EP_TYPE_BULK_IN,
                                       e.bulk_in_mps, 0, 0, 3072);
            ep.bounce = Some(DmaBuf::new((NTDS * bchunk).next_power_of_two().max(4096), dma32).ok_or("oom: buffers")?);
            s.eps[dbi] = Some(ep);
            s.eps[dbo] = Some(Endpoint::new(ProducerRing::new(dma32).ok_or("oom: ring")?, EP_TYPE_BULK_OUT,
                                            e.bulk_out_mps, 0, 0, 3072));
            x.build_configure_input(slot, 0, (1 << di) | (1 << dbi) | (1 << dbo)).ok_or("slot vanished")?
        };
        match run_cmd(hc, Trb::new(TRB_CONFIGURE_EP, in_phys, 0, (slot as u32) << 24)) {
            Ok((CC_SUCCESS, _)) => {}
            Ok((cc, _)) => {
                usb_log!("[USB-BT] slot {}: Configure Endpoint cc={}\n", slot, cc);
                return Err("Configure Endpoint failed");
            }
            Err(_) => return Err("Configure Endpoint timed out"),
        }
        let mut d = BtUsb {
            hc: hc.clone(), slot, eps: e, dci_out: dbo,
            ev: InPipe { dci: di, chunk: ichunk, pending: Vec::with_capacity(NTDS), failed: None, recoveries: 0, good: false },
            acl: InPipe { dci: dbi, chunk: bchunk, pending: Vec::with_capacity(NTDS), failed: None, recoveries: 0, good: false },
            dead: false,
            reset: (false, false),
        };
        {
            let mut x = d.hc.lock();
            d.ev.prime(&mut x, slot);
            d.acl.prime(&mut x, slot);
        }
        usb_log!("[USB-BT] slot {}: HCI iface {} evt {:02x} (mps {}, ival {}) acl in {:02x} (mps {}) out {:02x} (mps {})\n",
                 slot, e.iface, e.intr_in, e.intr_mps, e.intr_ival, e.bulk_in, e.bulk_in_mps, e.bulk_out, e.bulk_out_mps);
        Ok(d)
    }

    /// One HCI command (`op_lo op_hi plen params…`, no H4 type byte) on EP0.
    pub fn send_cmd(&mut self, pkt: &[u8]) -> Result<(), UsbErr> {
        if self.dead { return Err(UsbErr::Disconnected); }
        control(&self.hc, self.slot, 0x20, 0, 0, self.eps.iface as u16, CtrlData::Out(pkt), CMD_TIMEOUT_MS).map(|_| ())
    }

    /// One packet on the bulk OUT endpoint (ACL data; secure-send commands in
    /// the Intel bootloader).
    pub fn send_bulk(&mut self, pkt: &[u8]) -> Result<(), UsbErr> {
        if self.dead { return Err(UsbErr::Disconnected); }
        bulk(&self.hc, self.slot, self.dci_out, Some(pkt), None, CMD_TIMEOUT_MS).map(|_| ())
    }

    pub fn poll(&mut self) -> BtPoll {
        if self.dead { return BtPoll::Gone; }
        let mut out = Vec::new();
        {
            let hc = self.hc.clone();
            let Some(mut x) = hc.try_lock() else { return BtPoll::Busy };
            x.drain_events();
            if x.dead || !x.port_connected(self.slot) {
                self.dead = true;
                return BtPoll::Gone;
            }
            self.ev.harvest(&mut x, self.slot, false, &mut out);
            self.acl.harvest(&mut x, self.slot, true, &mut out);
            for p in [&mut self.ev, &mut self.acl] {
                if p.good && p.failed.is_none() { p.recoveries = 0; }
                p.good = false;
            }
            if self.ev.failed.is_none() { self.ev.prime(&mut x, self.slot); }
            if self.acl.failed.is_none() { self.acl.prime(&mut x, self.slot); }
        }
        for i in 0..2 {
            let p = if i == 0 { &mut self.ev } else { &mut self.acl };
            let Some(cc) = p.failed else { continue };
            p.recoveries += 1;
            usb_log!("[USB-BT] slot {} dci {}: transfer error cc {}, resetting endpoint\n", self.slot, p.dci, cc);
            // recover_ep_inner, not recover_ep: a failed recovery must not
            // take the whole controller (and the USB root disk) down.
            if p.recoveries > MAX_RECOVERIES || recover_ep_inner(&self.hc, self.slot, p.dci).is_err() {
                usb_log!("[USB-BT] slot {} dci {}: endpoint unrecoverable\n", self.slot, p.dci);
                quarantine_slot(&self.hc, self.slot, "BT endpoint unrecoverable");
                self.dead = true;
                return BtPoll::Gone;
            }
            if i == 0 { self.reset.0 = true } else { self.reset.1 = true }
            p.failed = None;
            p.pending.clear();
            let mut x = self.hc.lock();
            if let Some(ep) = x.ep_mut(self.slot, p.dci) { ep.events.clear(); }
            p.prime(&mut x, self.slot);
        }
        BtPoll::Data(out)
    }
}
