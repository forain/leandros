//! Broadcom UART controller bring-up (btbcm.c + hci_bcm.c order), generic over
//! the byte transport so the whole sequence runs on the host against a
//! scripted controller:
//!
//! HCI Reset → Read Local Version (unanswered: driver-driven RTS/CTS, Reset,
//! again) → [Download Minidriver (0xfc2e), 50 ms, every
//! `.hcd` record, 250 ms, HCI Reset] → [Update UART Baud Rate (0xfc18), switch
//! our UART, Read Local Version at the new rate] → Read Local Version →
//! Read BD_ADDR → [Write BD_ADDR (0xfc01) if the controller still has its
//! placeholder and the device tree supplies one].
//!
//! Every wait is bounded; the first failure aborts with the stage name.
use alloc::string::String;
use alloc::vec::Vec;

use super::h4::{self, BdAddr, Event, H4Parser, LocalVersion};
use super::hcd::{self, HcdError, Summary};

pub trait Transport {
    /// Writes all bytes (bounded wait; `false` if the controller never took them).
    fn send(&mut self, bytes: &[u8]) -> bool;
    /// One received byte, or `None` after `timeout_us` without one.
    fn recv(&mut self, timeout_us: u32) -> Option<u8>;
    /// Switches the host side to a new rate (after the controller acknowledged it).
    fn set_baud(&mut self, baud: u32) -> bool;
    fn sleep_ms(&mut self, ms: u32);
    /// SError check after a hardware phase: `Some(syndrome)` aborts the bring-up.
    fn checkpoint(&mut self) -> Option<u64> { None }
    /// Switches the host UART's flow control; `false` = not supported (kept as is).
    fn set_flow(&mut self, _f: Flow) -> bool { false }
    /// Modem-line snapshot for the diagnostics.
    fn lines(&mut self) -> Lines { Lines::default() }
}

/// Host-side RTS/CTS handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Flow {
    /// The UART's own auto RTS/CTS (16550 MCR.AFE; RTS drops at the RX trigger level).
    #[default]
    Auto,
    /// RTS/CTS driven by the driver: RTS asserted only while it polls RX, TX gated
    /// on MSR.CTS. Fallback when the controller goes silent in `Auto` (pibt-rts:
    /// Reset's 7-byte answer is below the 8-byte RX trigger, Read Local Version's
    /// 15 bytes are not).
    Soft,
}

/// Modem lines: MCR/MSR as read, the RTS (GPIO 24, ours) / CTS (GPIO 25, the
/// controller's) pad levels (`None` = unreadable; low = asserted), and how many
/// CTS changes (MSR.DCTS) the transport has seen so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Lines {
    pub mcr: u8,
    pub msr: u8,
    pub rts_pad: Option<bool>,
    pub cts_pad: Option<bool>,
    pub dcts: u32,
}

impl core::fmt::Display for Lines {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let pad = |v: Option<bool>| match v { Some(true) => '1', Some(false) => '0', None => '?' };
        write!(f, "mcr={:#x} msr={:#x} rts_pad={} cts_pad={} dcts={}", self.mcr, self.msr, pad(self.rts_pad), pad(self.cts_pad), self.dcts)
    }
}

/// Bytes one command's receive window got (first [`DIAG_RX`] kept).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub len: u32,
    pub b: [u8; DIAG_RX],
}

impl Default for Window { fn default() -> Self { Window::NONE } }

impl Window {
    pub const NONE: Window = Window { len: 0, b: [0; DIAG_RX] };
    /// The kept bytes as hex, `-` when nothing arrived.
    pub fn hex(&self) -> String { hex(&self.b, self.len) }
}

fn hex(b: &[u8; DIAG_RX], len: u32) -> String {
    use core::fmt::Write;
    let n = (len as usize).min(DIAG_RX);
    if n == 0 { return String::from("-"); }
    let mut s = String::with_capacity(n * 3);
    for (i, x) in b[..n].iter().enumerate() {
        let _ = write!(s, "{}{:02x}", if i == 0 { "" } else { " " }, x);
    }
    if len as usize > n { s.push_str(" .."); }
    s
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// No Command Complete/Status for this opcode in time.
    NoAnswer(u16),
    /// Command Complete/Status with a non-zero status.
    Status(u16, u8),
    /// Return parameters too short to decode.
    Malformed(u16),
    Tx(u16),
    BaudRejected(u32),
    Hcd(HcdError),
    SError(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error {
    pub stage: &'static str,
    pub kind: ErrorKind,
    /// What the wire looked like when the stage failed (filled by `bring_up`).
    pub diag: Diag,
}

/// Bytes kept from the failing command's receive window.
pub const DIAG_RX: usize = 24;

/// Wire state at a failure: the host rate in force, every byte the failing
/// command's wait received (first [`DIAG_RX`] kept), and how many Resets were
/// sent. `rx_len == 0` = the controller sent nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Diag {
    pub baud: u32,
    pub rx_len: u32,
    pub rx: [u8; DIAG_RX],
    pub reset_tries: u32,
    pub version_tries: u32,
    /// Flow mode in force at the failure.
    pub flow: Flow,
    /// Modem lines at the failure.
    pub lines: Lines,
    /// The first Read Local Version window (in `Auto`) and its lines, when it
    /// went unanswered and the bring-up fell back to [`Flow::Soft`].
    pub v1: Option<(Window, Lines)>,
}

impl Diag {
    pub const NONE: Diag = Diag {
        baud: 0, rx_len: 0, rx: [0; DIAG_RX], reset_tries: 0, version_tries: 0,
        flow: Flow::Auto, lines: Lines { mcr: 0, msr: 0, rts_pad: None, cts_pad: None, dcts: 0 }, v1: None,
    };
    /// The kept bytes as hex, `-` when nothing arrived.
    pub fn rx_hex(&self) -> String { hex(&self.rx, self.rx_len) }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Options<'a> {
    /// `.hcd` image; `None` skips the patch download.
    pub hcd: Option<&'a [u8]>,
    /// Operational baud rate; `None` stays at the initial rate.
    pub oper_baud: Option<u32>,
    /// Address to program when the controller reports a placeholder.
    pub bdaddr: Option<BdAddr>,
    /// Per-command answer timeout (µs); 0 means the default (1 s).
    pub cmd_timeout_us: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Report {
    /// Version before the patch (ROM firmware).
    pub rom: LocalVersion,
    /// Version after everything (patched firmware when `patch` is `Some`).
    pub ver: LocalVersion,
    pub patch: Option<Summary>,
    /// The rate both sides ended at.
    pub baud: u32,
    pub bdaddr: BdAddr,
    /// `true` if we wrote the address with 0xfc01.
    pub bdaddr_set: bool,
    /// HCI Resets sent before the ROM answered (1 = first try).
    pub reset_tries: u32,
    /// Read Local Version attempts after that Reset (1 = first try).
    pub version_tries: u32,
    /// Events that were not the awaited command's answer (vendor events etc.).
    pub stray_events: u32,
    /// Bytes dropped while resynchronising the H4 stream.
    pub garbage: u32,
    /// Flow mode the bring-up finished in.
    pub flow: Flow,
    /// See [`Diag::v1`]: set when the bring-up fell back to [`Flow::Soft`].
    pub v1: Option<(Window, Lines)>,
}

pub const INIT_BAUD: u32 = 115_200;
const DEFAULT_CMD_TIMEOUT_US: u32 = 1_000_000;
/// One received-byte poll slice; the overall deadline is counted in these.
const POLL_US: u32 = 1_000;
const MAX_BYTES_PER_CMD: u32 = 64 * 1024;
/// btbcm_reset(): "100 msec delay for module to complete reset process".
/// The controller re-initialises after answering Reset and drops a command
/// sent inside this window (real Pi 5: Reset answered, the next Read Local
/// Version got no byte back).
pub const RESET_SETTLE_MS: u32 = 100;

struct Hci<'t, T: Transport> {
    t: &'t mut T,
    p: H4Parser,
    tx: Vec<u8>,
    timeout_us: u32,
    stray: u32,
    /// Host rate in force (for the failure diagnostics).
    baud: u32,
    /// Receive window of the most recent command.
    rx: [u8; DIAG_RX],
    rx_len: u32,
    reset_tries: u32,
    version_tries: u32,
    flow: Flow,
    v1: Option<(Window, Lines)>,
}

impl<'t, T: Transport> Hci<'t, T> {
    /// Sends one command and waits for its Command Complete; returns the
    /// return parameters. A non-zero status (first return byte, or the Command
    /// Status event) is an error.
    fn cmd(&mut self, stage: &'static str, op: u16, params: &[u8]) -> Result<Vec<u8>, Error> {
        let err = |kind| Error { stage, diag: Diag::NONE, kind };
        self.tx.clear();
        h4::encode_cmd(op, params, &mut self.tx);
        if !self.t.send(&self.tx) { return Err(err(ErrorKind::Tx(op))); }
        let mut waited = 0u32;
        // A chatty controller must not keep us here forever either.
        let mut bytes = 0u32;
        self.rx_len = 0;
        while waited < self.timeout_us && bytes < MAX_BYTES_PER_CMD {
            let Some(b) = self.t.recv(POLL_US) else { waited += POLL_US; continue; };
            bytes += 1;
            if (self.rx_len as usize) < DIAG_RX { self.rx[self.rx_len as usize] = b; }
            self.rx_len = self.rx_len.saturating_add(1);
            let Some(pkt) = self.p.feed(b) else { continue };
            if pkt.kind != h4::H4_EVT { self.stray += 1; continue; }
            match h4::parse_event(&pkt.data) {
                Some(Event::CommandComplete { opcode, ret, .. }) if opcode == op => {
                    return match ret.first() {
                        Some(0) => Ok(ret.to_vec()),
                        Some(&s) => Err(err(ErrorKind::Status(op, s))),
                        None => Err(err(ErrorKind::Malformed(op))),
                    };
                }
                Some(Event::CommandStatus { status, opcode, .. }) if opcode == op && status != 0 => {
                    return Err(err(ErrorKind::Status(op, status)));
                }
                _ => self.stray += 1,
            }
        }
        Err(err(ErrorKind::NoAnswer(op)))
    }

    fn check(&mut self, stage: &'static str) -> Result<(), Error> {
        match self.t.checkpoint() {
            Some(syn) => Err(Error { stage, diag: Diag::NONE, kind: ErrorKind::SError(syn) }),
            None => Ok(()),
        }
    }

    /// HCI Reset, then the settle delay btbcm_reset() keeps.
    fn reset(&mut self, stage: &'static str) -> Result<(), Error> {
        self.reset_tries += 1;
        self.cmd(stage, h4::OP_RESET, &[])?;
        self.t.sleep_ms(RESET_SETTLE_MS);
        Ok(())
    }

    fn diag(&mut self) -> Diag {
        Diag {
            baud: self.baud, rx_len: self.rx_len, rx: self.rx, reset_tries: self.reset_tries,
            version_tries: self.version_tries, flow: self.flow, lines: self.t.lines(), v1: self.v1,
        }
    }

    /// Reads and drops whatever is still arriving (a stalled answer released by
    /// a flow change) until the line has been idle for `idle_ms`.
    fn drain(&mut self, idle_ms: u32) -> u32 {
        let (mut idle, mut n) = (0u32, 0u32);
        while idle < idle_ms * 1000 && n < MAX_BYTES_PER_CMD {
            match self.t.recv(POLL_US) { Some(_) => { n += 1; idle = 0; } None => idle += POLL_US }
        }
        n
    }

    fn version(&mut self, stage: &'static str) -> Result<LocalVersion, Error> {
        let r = self.cmd(stage, h4::OP_READ_LOCAL_VERSION, &[])?;
        h4::parse_local_version(&r).ok_or(Error { stage, diag: Diag::NONE, kind: ErrorKind::Malformed(h4::OP_READ_LOCAL_VERSION) })
    }

    fn bdaddr(&mut self, stage: &'static str) -> Result<BdAddr, Error> {
        let r = self.cmd(stage, h4::OP_READ_BD_ADDR, &[])?;
        h4::parse_bdaddr(&r).ok_or(Error { stage, diag: Diag::NONE, kind: ErrorKind::Malformed(h4::OP_READ_BD_ADDR) })
    }
}

/// Runs the whole sequence. The transport must already be at [`INIT_BAUD`]
/// with the controller freshly powered.
pub fn bring_up<T: Transport>(t: &mut T, o: &Options) -> Result<Report, Error> {
    // Validate the image before the controller is touched at all.
    let patch = match o.hcd {
        Some(img) => Some((img, hcd::validate(img).map_err(|e| Error { stage: "hcd", diag: Diag::NONE, kind: ErrorKind::Hcd(e) })?)),
        None => None,
    };
    let timeout_us = if o.cmd_timeout_us == 0 { DEFAULT_CMD_TIMEOUT_US } else { o.cmd_timeout_us };
    let mut h = Hci {
        t, p: H4Parser::new(), tx: Vec::with_capacity(260), timeout_us, stray: 0,
        baud: INIT_BAUD, rx: [0; DIAG_RX], rx_len: 0, reset_tries: 0, version_tries: 0,
        flow: Flow::Auto, v1: None,
    };
    let mut rep = Report { baud: INIT_BAUD, ..Report::default() };
    match sequence(&mut h, o, patch, &mut rep) {
        Ok(()) => Ok(rep),
        Err(mut e) => { e.diag = h.diag(); Err(e) }
    }
}

fn sequence<T: Transport>(h: &mut Hci<'_, T>, o: &Options, patch: Option<(&[u8], Summary)>, rep: &mut Report) -> Result<(), Error> {
    // A controller fresh out of BT_ON can miss the first command: one retry.
    if h.reset("reset").is_err() {
        h.p.reset();
        h.reset("reset")?;
    }
    rep.reset_tries = h.reset_tries;
    h.check("reset")?;
    // Real Pi 5: Reset (7-byte answer) works, Read Local Version (15 bytes)
    // gets nothing with the UART's auto RTS. Keep that window and the line
    // state, switch to driver-driven RTS/CTS, release whatever the stalled
    // controller still holds, then Reset (resync) and ask again.
    h.version_tries = 1;
    rep.rom = match h.version("version") {
        Err(Error { kind: ErrorKind::NoAnswer(_), .. }) => {
            let w = Window { len: h.rx_len, b: h.rx };
            let lines = h.t.lines();
            h.v1 = Some((w, lines));
            if h.t.set_flow(Flow::Soft) { h.flow = Flow::Soft; }
            h.drain(20);
            h.p.reset();
            h.version_tries = 2;
            h.reset("reset-resync")?;
            h.version("version")?
        }
        r => r?,
    };
    rep.version_tries = h.version_tries;
    rep.reset_tries = h.reset_tries;
    rep.ver = rep.rom;

    if let Some((img, sum)) = patch {
        h.cmd("minidriver", h4::OP_BCM_DOWNLOAD_MINIDRV, &[])?;
        h.t.sleep_ms(50);
        for r in hcd::records(img) {
            let (_, op, p) = r.map_err(|e| Error { stage: "hcd", diag: Diag::NONE, kind: ErrorKind::Hcd(e) })?;
            h.cmd("patch", op, p)?;
        }
        h.check("patch")?;
        // Launch RAM: the patched firmware boots; btbcm waits 250 ms.
        h.t.sleep_ms(250);
        h.p.reset();
        h.reset("reset-patched")?;
        rep.patch = Some(sum);
    }

    if let Some(baud) = o.oper_baud.filter(|&b| b != INIT_BAUD) {
        h.cmd("baud", h4::OP_BCM_UPDATE_BAUD, &h4::bcm_baud_params(baud))?;
        if !h.t.set_baud(baud) { return Err(Error { stage: "baud", diag: Diag::NONE, kind: ErrorKind::BaudRejected(baud) }); }
        h.baud = baud;
        h.t.sleep_ms(10);
        h.p.reset();
        rep.baud = baud;
        h.check("baud")?;
    }

    rep.ver = h.version("version-final")?;
    rep.bdaddr = h.bdaddr("bdaddr")?;
    if rep.bdaddr.is_placeholder() {
        if let Some(a) = o.bdaddr.filter(|a| !a.is_placeholder()) {
            h.cmd("bdaddr-set", h4::OP_BCM_WRITE_BD_ADDR, &a.0)?;
            rep.bdaddr = h.bdaddr("bdaddr")?;
            rep.bdaddr_set = true;
        }
    }
    rep.stray_events = h.stray;
    rep.garbage = h.p.garbage;
    rep.flow = h.flow;
    rep.v1 = h.v1;
    Ok(())
}

/// Finds the first `local-bd-address` property (6 bytes) in a flattened device
/// tree. `None` if absent, malformed or all zeros (the firmware fills it on a
/// real boot; the stock .dtb carries zeros).
pub fn fdt_local_bd_address(dtb: &[u8]) -> Option<BdAddr> {
    let be32 = |o: usize| dtb.get(o..o + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    if be32(0)? != 0xd00d_feed { return None; }
    let st = be32(8)? as usize;
    let strs = be32(12)? as usize;
    let end = (st + be32(36).unwrap_or(0) as usize).min(dtb.len());
    let mut o = st;
    while o + 4 <= end {
        let tok = be32(o)?;
        o += 4;
        match tok {
            1 => {
                let n = dtb.get(o..end)?.iter().position(|&c| c == 0)?;
                o = (o + n + 1 + 3) & !3;
            }
            3 => {
                let len = be32(o)? as usize;
                let nameoff = be32(o + 4)? as usize;
                let val = o + 8;
                o = (val + len + 3) & !3;
                let name = dtb.get(strs + nameoff..)?;
                let nl = name.iter().position(|&c| c == 0)?;
                if &name[..nl] == b"local-bd-address" && len == 6 {
                    let mut a = [0u8; 6];
                    a.copy_from_slice(dtb.get(val..val + 6)?);
                    let a = BdAddr(a);
                    return if a.is_zero() { None } else { Some(a) };
                }
            }
            2 | 4 => {}
            9 => break,
            _ => return None,
        }
    }
    None
}
