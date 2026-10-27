//! `/dev/hci0` queue logic (docs/servers/bluetooth.md section A), pure and
//! host-tested (drivers/bt-tests). The rpi5 glue (`hcidev`) wraps one
//! `HciQueues` in a lock, answers the VFS from it and runs `pump_once` on the
//! bt thread against the UART.
//!
//! Contract: `write` takes exactly one whole H4 packet (type byte first);
//! `read` returns exactly one whole H4 packet or `EMSGSIZE` (packet kept);
//! RX holds at least 64 packets and drops the NEWEST on overflow.
use alloc::collections::VecDeque;
use alloc::vec::Vec;

use super::bringup::Transport;
use super::h4::H4Parser;

pub const EIO: i32 = -5;
pub const EAGAIN: i32 = -11;
pub const EINVAL: i32 = -22;
pub const EMSGSIZE: i32 = -90;

pub const POLLIN: u64 = 0x1;
pub const POLLOUT: u64 = 0x4;
pub const POLLERR: u64 = 0x8;
pub const POLLHUP: u64 = 0x10;

pub const RX_CAP: usize = 64;
pub const TX_CAP: usize = 16;
/// Largest packet a write may carry: an ACL header plus 64 KiB is the H4
/// maximum; anything bigger is not one packet.
pub const MAX_PKT: usize = 5 + 0xffff;

/// One whole H4 packet from the host: `01` command (3-byte header, len at
/// [3]), `02` ACL (4-byte header, LE16 len at [3..5]), `03` SCO (3-byte
/// header, len at [3]). The length field must equal the remaining bytes.
pub fn validate_h4_write(b: &[u8]) -> Result<(), i32> {
    let (hdr, plen) = match b.first() {
        Some(0x01) | Some(0x03) if b.len() >= 4 => (4, b[3] as usize),
        Some(0x02) if b.len() >= 5 => (5, u16::from_le_bytes([b[3], b[4]]) as usize),
        _ => return Err(EINVAL),
    };
    if b.len() != hdr + plen { return Err(EINVAL); }
    Ok(())
}

#[derive(Default)]
pub struct HciQueues {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
    /// An fd is open: packets from the controller are queued only then, so a
    /// later opener never sees a previous session's stale events.
    pub open: bool,
    /// The controller stopped taking bytes: reads drain what is left, then EIO.
    pub dead: bool,
    pub rx_drops: u64,
}

impl HciQueues {
    pub const fn new() -> Self {
        Self { rx: VecDeque::new(), tx: VecDeque::new(), open: false, dead: false, rx_drops: 0 }
    }

    pub fn open(&mut self) {
        self.rx.clear();
        self.tx.clear();
        self.open = true;
    }

    pub fn close(&mut self) {
        self.rx.clear();
        self.tx.clear();
        self.open = false;
    }

    /// A packet from the controller (`[kind] + data`). `false` if dropped
    /// (queue full: the newest is dropped and counted; nobody open: discarded).
    pub fn push_rx(&mut self, pkt: Vec<u8>) -> bool {
        if !self.open { return false; }
        if self.rx.len() >= RX_CAP { self.rx_drops += 1; return false; }
        self.rx.push_back(pkt);
        true
    }

    /// The next packet if it fits in `count` bytes.
    pub fn read(&mut self, count: usize) -> Result<Vec<u8>, i32> {
        match self.rx.front() {
            None if self.dead => Err(EIO),
            None => Err(EAGAIN),
            Some(p) if p.len() > count => Err(EMSGSIZE),
            Some(_) => Ok(self.rx.pop_front().unwrap_or_default()),
        }
    }

    pub fn write(&mut self, b: &[u8]) -> Result<usize, i32> {
        if self.dead { return Err(EIO); }
        validate_h4_write(b)?;
        if self.tx.len() >= TX_CAP { return Err(EAGAIN); }
        self.tx.push_back(b.to_vec());
        Ok(b.len())
    }

    pub fn pop_tx(&mut self) -> Option<Vec<u8>> { self.tx.pop_front() }

    pub fn poll_mask(&self) -> u64 {
        let mut m = 0;
        if !self.rx.is_empty() { m |= POLLIN; }
        if self.dead { m |= POLLHUP | POLLERR; } else if self.tx.len() < TX_CAP { m |= POLLOUT; }
        m
    }

    pub fn rx_len(&self) -> usize { self.rx.len() }
}

/// Access to the shared queues (the kernel takes its lock with interrupts off).
pub trait QueueLock {
    fn with<R>(&self, f: impl FnOnce(&mut HciQueues) -> R) -> R;
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct PumpOut {
    pub sent: usize,
    pub received: usize,
    pub dropped: usize,
    /// The send failed this round: the queues are now marked dead.
    pub died: bool,
}

impl PumpOut {
    /// Readiness changed: the caller wakes the poll key.
    pub fn changed(&self) -> bool { self.sent + self.received + self.dropped > 0 || self.died }
}

/// One pump round: drain TX to the controller, then wait up to `wait_us` for
/// the first received byte and take what else is already there (at most
/// `max_bytes`), framing whole packets into RX. The lock is never held across
/// transport I/O.
pub fn pump_once<T: Transport, L: QueueLock>(q: &L, t: &mut T, parser: &mut H4Parser, wait_us: u32, max_bytes: usize) -> PumpOut {
    let mut out = PumpOut::default();
    while let Some(pkt) = q.with(|q| if q.dead { None } else { q.pop_tx() }) {
        if !t.send(&pkt) {
            q.with(|q| { q.dead = true; q.tx.clear(); });
            out.died = true;
            return out;
        }
        out.sent += 1;
    }
    let mut timeout = wait_us;
    for _ in 0..max_bytes {
        let Some(b) = t.recv(timeout) else { break };
        timeout = 0;
        if let Some(p) = parser.feed(b) {
            let mut v = Vec::with_capacity(1 + p.data.len());
            v.push(p.kind);
            v.extend_from_slice(&p.data);
            let open = q.with(|q| { let o = q.open; (o, q.push_rx(v)) });
            match open {
                (true, true) => out.received += 1,
                (true, false) => out.dropped += 1,
                (false, _) => {}
            }
        }
    }
    out
}
