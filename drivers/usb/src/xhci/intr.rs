//! Interrupt-IN pipes for HID devices, driven from the timer tick.
//!
//! Everything else in this driver is synchronous: a caller in task context
//! enqueues one TD and polls until it completes. A keyboard cannot work that
//! way — nobody is waiting for the next keystroke — so an interrupt pipe keeps
//! [`NTDS`] one-packet TDs queued on its endpoint at all times and is
//! *harvested* from the BSP timer tick (100 Hz, IRQ context) by
//! [`IntrPipe::poll`]:
//!
//! * The controller lock is only ever **try**-locked here. If a task holds it
//!   (block I/O between two polls of its own TD, boot enumeration) this tick
//!   is skipped; nothing in this file waits for anything.
//! * Events are drained with [`Xhci::drain_events`], which never halts the
//!   controller itself (halting waits); a fatal controller error is left for
//!   the next task-context poll to act on.
//! * A halted endpoint (STALL, three transaction errors) is recovered with
//!   commands that are submitted here and *collected on later ticks* (Reset
//!   Endpoint → Set TR Dequeue → re-prime), never waited for.
//! * Report buffers are the endpoint's `bounce` `DmaBuf`, owned by the slot,
//!   so they follow the slot's lifetime rules (freed only after Disable Slot).
//!
//! Configuration (endpoint contexts, SET_CONFIGURATION, SET_PROTOCOL,
//! SET_IDLE) happens once at enumeration, in task context, with the ordinary
//! blocking control path: [`setup_hid`].

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::*;
use crate::hid::{HidIfaceDesc, HID_PROTO_KEYBOARD};

/// TDs kept queued per interrupt endpoint: reports the controller can take
/// between two 10 ms ticks without the device having to hold one back.
pub const NTDS: usize = 8;
/// Largest report kept per TD (boot reports are 3..8 bytes; gaming devices
/// with 64-byte endpoints are truncated to what the boot protocol defines).
pub const REPORT_MAX: usize = 64;
/// Endpoint recoveries before the pipe gives up for good.
const MAX_RECOVERIES: u32 = 16;
/// A recovery command not completed in this long gives the pipe up.
const RECOVERY_CMD_NS: u64 = 2_000_000_000;

// HID class requests (HID 1.11 §7.2).
const HID_SET_REPORT: u8 = 0x09;
const HID_SET_IDLE: u8 = 0x0A;
const HID_SET_PROTOCOL: u8 = 0x0B;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Running,
    /// The endpoint halted; a Reset Endpoint goes out on the next poll.
    NeedReset { stalled: bool },
    ResetPending { cmd: u64, stalled: bool },
    DeqPending { cmd: u64, stalled: bool },
    /// Port gone, controller dead, or too many recoveries.
    Dead,
}

/// Reports harvested by one poll (fixed size: the tick must not allocate).
pub struct Reports {
    pub n: usize,
    pub len: [u8; NTDS],
    pub data: [[u8; REPORT_MAX]; NTDS],
}

impl Reports {
    pub const fn new() -> Self { Reports { n: 0, len: [0; NTDS], data: [[0; REPORT_MAX]; NTDS] } }
    pub fn get(&self, i: usize) -> &[u8] { &self.data[i][..self.len[i] as usize] }
}

/// What one poll found.
pub enum Poll {
    /// Reports harvested (possibly none).
    Ok,
    /// The controller lock was busy; try again next tick.
    Busy,
    /// The device is gone (or the pipe gave up). Reported once.
    Gone,
}

/// One interrupt-IN endpoint with TDs permanently queued on it.
pub struct IntrPipe {
    hc: Arc<Hc>,
    slot: u8,
    dci: usize,
    ep_addr: u8,
    iface: u8,
    chunk: usize,
    /// (TRB pointer, buffer index) of every TD on the ring, oldest first.
    pending: Vec<(u64, u8)>,
    state: State,
    recoveries: u32,
    /// Deadline of the recovery command in flight.
    cmd_deadline: u64,
    /// Asynchronous EP0 request (SET_REPORT for the LEDs) in flight.
    ctrl: Option<Td>,
    ctrl_broken: bool,
    reported_gone: bool,
}

impl IntrPipe {
    pub fn slot(&self) -> u8 { self.slot }
    pub fn interface(&self) -> u8 { self.iface }
    pub fn endpoint(&self) -> u8 { self.ep_addr }

    /// Queue TDs on every free buffer. Lock held.
    fn prime(&mut self, x: &mut Xhci) {
        let Some(ep) = x.ep_mut(self.slot, self.dci) else { return };
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
        if queued { x.regs.doorbell(self.slot, self.dci as u32); }
    }

    /// Harvest completed reports into `out` (each `(len, bytes)`), re-queue
    /// their TDs, and advance any recovery in progress. IRQ-safe: try_lock
    /// only, no waits.
    pub fn poll(&mut self, out: &mut Reports) -> Poll {
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

        match self.state {
            State::Running => self.harvest(&mut x, out),
            State::NeedReset { stalled } => {
                let cmd = x.submit_cmd(ep_cmd(TRB_RESET_EP, self.slot, self.dci));
                self.cmd_deadline = platform::now_ns() + RECOVERY_CMD_NS;
                self.state = State::ResetPending { cmd, stalled };
            }
            State::ResetPending { cmd, stalled } => {
                if let Some((cc, _)) = x.take_cmd(cmd) {
                    if cc != CC_SUCCESS && cc != CC_CONTEXT_STATE {
                        usb_log!("[USB-HID] slot {} ep {:02x}: Reset Endpoint failed (cc {}), giving up\n",
                                 self.slot, self.ep_addr, cc);
                        self.state = State::Dead;
                    } else {
                        let deq = match x.ep_mut(self.slot, self.dci) {
                            Some(ep) => ep.ring.dequeue_ptr(),
                            None => { self.state = State::Dead; return Poll::Ok; }
                        };
                        let t = Trb::new(TRB_SET_TR_DEQ, deq, 0,
                                         (self.slot as u32) << 24 | (self.dci as u32) << 16);
                        let cmd = x.submit_cmd(t);
                        self.cmd_deadline = platform::now_ns() + RECOVERY_CMD_NS;
                        self.state = State::DeqPending { cmd, stalled };
                    }
                } else if platform::now_ns() > self.cmd_deadline {
                    usb_log!("[USB-HID] slot {} ep {:02x}: Reset Endpoint never completed, giving up\n",
                             self.slot, self.ep_addr);
                    self.state = State::Dead;
                }
            }
            State::DeqPending { cmd, stalled } => {
                if let Some((cc, _)) = x.take_cmd(cmd) {
                    if cc != CC_SUCCESS {
                        usb_log!("[USB-HID] slot {} ep {:02x}: Set TR Dequeue failed (cc {}), giving up\n",
                                 self.slot, self.ep_addr, cc);
                        self.state = State::Dead;
                    } else {
                        // Every TD that was on the ring is gone with the
                        // dequeue pointer move.
                        self.pending.clear();
                        if let Some(ep) = x.ep_mut(self.slot, self.dci) { ep.events.clear(); }
                        if stalled && self.ctrl.is_none() && !self.ctrl_broken {
                            // CLEAR_FEATURE(ENDPOINT_HALT): the device halted
                            // its side too. Asynchronous; result ignored.
                            self.ctrl = start_ctrl(&mut x, self.slot, 0x02, 1, 0,
                                                   self.ep_addr as u16, None);
                        }
                        self.state = State::Running;
                        self.prime(&mut x);
                    }
                } else if platform::now_ns() > self.cmd_deadline {
                    usb_log!("[USB-HID] slot {} ep {:02x}: Set TR Dequeue never completed, giving up\n",
                             self.slot, self.ep_addr);
                    self.state = State::Dead;
                }
            }
            State::Dead => {}
        }

        // An asynchronous EP0 request finishing (or failing).
        if let Some(td) = self.ctrl.as_ref() {
            if let Some(r) = x.check_td(self.slot, 1, td) {
                if r.is_err() {
                    // EP0 is halted now and recovering it needs waits; LED
                    // updates simply stop for this device.
                    self.ctrl_broken = true;
                    usb_log!("[USB-HID] slot {}: control request failed ({:?}); keyboard LEDs disabled\n",
                             self.slot, r.err());
                }
                self.ctrl = None;
            }
        }
        Poll::Ok
    }

    fn harvest(&mut self, x: &mut Xhci, out: &mut Reports) {
        let (slot, dci, chunk) = (self.slot, self.dci, self.chunk);
        let Some(ep) = x.ep_mut(slot, dci) else { self.state = State::Dead; return };
        if ep.events.is_empty() { return; }
        // Disjoint fields: the filed events and the report buffers.
        let Endpoint { events, bounce, .. } = ep;
        let Some(buf) = bounce.as_ref() else { self.state = State::Dead; return };
        buf.sync_for_cpu();
        let mut failed: Option<u8> = None;
        for ev in events.iter() {
            let Some(k) = self.pending.iter().position(|p| p.0 == ev.ptr) else { continue };
            let (_, idx) = self.pending.remove(k);
            match ev.cc {
                CC_SUCCESS | CC_SHORT => {
                    let len = chunk.saturating_sub(ev.residual as usize).min(REPORT_MAX);
                    if len > 0 && out.n < NTDS {
                        buf.copy_out(idx as usize * chunk, &mut out.data[out.n][..len]);
                        out.len[out.n] = len as u8;
                        out.n += 1;
                    }
                }
                cc => { failed = Some(cc); break; }
            }
        }
        // `clear`, not `take`: the vector keeps its capacity, so filing
        // events from IRQ context never allocates.
        events.clear();
        if let Some(cc) = failed {
            self.recoveries += 1;
            if self.recoveries > MAX_RECOVERIES {
                usb_log!("[USB-HID] slot {} ep {:02x}: {} endpoint errors, giving up\n",
                         slot, self.ep_addr, self.recoveries);
                self.state = State::Dead;
            } else {
                usb_dbg!("[USB-HID] slot {} ep {:02x}: transfer error cc {}, resetting endpoint\n",
                         slot, self.ep_addr, cc);
                self.state = State::NeedReset { stalled: cc == CC_STALL };
            }
            return;
        }
        self.prime(x);
    }

    /// Send an output report (SET_REPORT, report type Output, id 0) without
    /// waiting. Returns false when it could not be started now (lock busy,
    /// a request still in flight, or EP0 broken) — call again later.
    pub fn set_report_nowait(&mut self, data: &[u8]) -> bool {
        if self.ctrl.is_some() || self.ctrl_broken || self.state == State::Dead { return false; }
        let hc = self.hc.clone();
        let Some(mut x) = hc.try_lock() else { return false };
        if x.dead { return false; }
        self.ctrl = start_ctrl(&mut x, self.slot, 0x21, HID_SET_REPORT, 0x0200, self.iface as u16, Some(data));
        self.ctrl.is_some()
    }

    pub fn leds_usable(&self) -> bool { !self.ctrl_broken && self.state != State::Dead }
}

/// Start an EP0 request without waiting for it. `data` is an OUT stage.
fn start_ctrl(x: &mut Xhci, slot: u8, req_type: u8, req: u8, value: u16, index: u16,
              data: Option<&[u8]>) -> Option<Td> {
    let len = data.map_or(0, |d| d.len());
    if len > CTRL_BUF_BYTES { return None; }
    let buf_phys = {
        let s = x.slot(slot)?;
        if let Some(d) = data {
            s.ctrl_buf.copy_in(0, d);
            s.ctrl_buf.sync_for_device();
        }
        s.ctrl_buf.phys()
    };
    let setup = req_type as u64 | (req as u64) << 8 | (value as u64) << 16
        | (index as u64) << 32 | (len as u64) << 48;
    let mut trbs = [Trb::default(); 3];
    let mut n = 0;
    trbs[n] = Trb::new(TRB_SETUP, setup, 8, TRB_IDT | if len == 0 { 0 } else { 2 << 16 });
    n += 1;
    if len > 0 {
        trbs[n] = Trb::new(TRB_DATA, buf_phys, len as u32, TRB_ISP);
        n += 1;
    }
    trbs[n] = Trb::new(TRB_STATUS, 0, 0, TRB_IOC | TRB_DIR_IN);
    n += 1;
    x.start_td(slot, 1, &trbs[..n], true).ok()
}

/// A configured HID interface: its descriptor and its pipe.
pub struct HidPipe {
    pub desc: HidIfaceDesc,
    pub pipe: IntrPipe,
}

/// Configure every boot HID interface in `ifaces` on `slot`: endpoint
/// contexts (one Configure Endpoint), SET_CONFIGURATION, then per interface
/// SET_INTERFACE (alt ≠ 0), SET_PROTOCOL(boot), SET_IDLE(0) and, for a
/// keyboard, an initial SET_REPORT with `leds`. Task context (blocking).
pub(super) fn setup_hid(hc: &Arc<Hc>, slot: u8, speed: u8, config_value: u8,
                        ifaces: &[HidIfaceDesc], leds: u8) -> Result<Vec<HidPipe>, &'static str> {
    let dma32 = hc.lock().dma32;
    let mut add = 0u32;
    {
        let mut x = hc.lock();
        let s = x.slot_mut(slot).ok_or("slot vanished")?;
        for d in ifaces {
            let dci = dci_of(d.ep_addr);
            let mps = d.mps & 0x7FF;
            // HS periodic: bits 12:11 are additional transactions per µframe.
            let (burst, mult) = if speed >= SPEED_SS {
                (d.ss_burst, 0u32)
            } else if speed == SPEED_HS {
                (((d.mps >> 11) & 3) as u8, 0u32)
            } else {
                (0, 0)
            };
            let esit = if speed >= SPEED_SS && d.ss_bytes_per_interval != 0 {
                d.ss_bytes_per_interval as u32
            } else {
                mps as u32 * (burst as u32 + 1) * (mult + 1)
            };
            let chunk = (mps as usize).clamp(8, 1024);
            let ring = ProducerRing::new(dma32).ok_or("oom: interrupt ring")?;
            let bounce = DmaBuf::new((NTDS * chunk).next_power_of_two().max(4096), dma32)
                .ok_or("oom: report buffers")?;
            let mut ep = Endpoint::new(ring, EP_TYPE_INTR_IN, mps, burst,
                                       interval_field(speed, false, d.interval), mps.max(8));
            ep.esit = esit;
            ep.bounce = Some(bounce);
            s.eps[dci] = Some(ep);
            add |= 1 << dci;
        }
    }
    let in_phys = hc.lock().build_configure_input(slot, 0, add).ok_or("slot vanished")?;
    let (cc, _) = run_cmd(hc, Trb::new(TRB_CONFIGURE_EP, in_phys, 0, (slot as u32) << 24))
        .map_err(|_| "Configure Endpoint timed out")?;
    if cc != CC_SUCCESS {
        usb_dbg!("[XHCI] Configure Endpoint (HID) cc={}\n", cc);
        return Err("Configure Endpoint failed");
    }
    control(hc, slot, 0x00, 9, config_value as u16, 0, CtrlData::None, ENUM_CTRL_TIMEOUT_MS)
        .map_err(|_| "SET_CONFIGURATION failed")?;

    let mut pipes = Vec::new();
    for d in ifaces {
        let i = d.iface as u16;
        if d.alt != 0 {
            control(hc, slot, 0x01, 11, d.alt as u16, i, CtrlData::None, ENUM_CTRL_TIMEOUT_MS)
                .map_err(|_| "SET_INTERFACE failed")?;
        }
        // SET_PROTOCOL(boot). Mandatory for a boot-subclass interface; a
        // failure means its reports may not be in boot format, so skip it.
        if control(hc, slot, 0x21, HID_SET_PROTOCOL, 0, i, CtrlData::None, ENUM_CTRL_TIMEOUT_MS).is_err() {
            usb_log!("[USB-HID] slot {} interface {}: SET_PROTOCOL(boot) failed, interface skipped\n", slot, d.iface);
            continue;
        }
        // SET_IDLE(0): report only on change. Optional for mice — a STALL
        // here is normal and harmless.
        let _ = control(hc, slot, 0x21, HID_SET_IDLE, 0, i, CtrlData::None, ENUM_CTRL_TIMEOUT_MS);
        if d.protocol == HID_PROTO_KEYBOARD {
            let _ = control(hc, slot, 0x21, HID_SET_REPORT, 0x0200, i, CtrlData::Out(&[leds]), ENUM_CTRL_TIMEOUT_MS);
        }
        let dci = dci_of(d.ep_addr);
        let chunk = ((d.mps & 0x7FF) as usize).clamp(8, 1024);
        let mut pipe = IntrPipe {
            hc: hc.clone(), slot, dci, ep_addr: d.ep_addr, iface: d.iface, chunk,
            pending: Vec::with_capacity(NTDS), state: State::Running, recoveries: 0, cmd_deadline: 0,
            ctrl: None, ctrl_broken: false, reported_gone: false,
        };
        {
            let mut x = hc.lock();
            pipe.prime(&mut x);
        }
        pipes.push(HidPipe { desc: *d, pipe });
    }
    Ok(pipes)
}
