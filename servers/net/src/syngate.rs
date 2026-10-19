//! At most one connection-opening SYN per `Interface::poll`.
//!
//! A listener has one LISTEN socket per stack (`listen.rs`, the "arm"), and
//! `listen::maintain` re-arms it only between polls. smoltcp's poll takes
//! every queued frame before returning, so when two SYNs for one listener
//! arrived in the same poll the first turned the arm into a handshake and
//! the second found no LISTEN socket and was answered with an RST
//! ("Connection refused"). On loopback that is every pair of connects issued
//! before the daemon's next pass: both SYNs leave in one egress and come back
//! in the same ingress.
//!
//! [`SynGate`] wraps a stack's device for one daemon pass. It hands smoltcp
//! the first pure SYN (SYN without ACK or RST) of a poll and holds the next
//! one back, ending that poll's ingress; the daemon then runs `maintain`
//! (which re-arms) and polls again with the held frame first. A SYN for a
//! port nobody listens on is held the same way; it costs one more poll and
//! changes nothing else.

use super::*;

/// A device whose received frames can be taken out as an owned buffer.
pub(crate) trait Frames: smoltcp::phy::Device {
    fn frame<'a>(rx: Self::RxToken<'a>) -> alloc::vec::Vec<u8> where Self: 'a;
}

impl Frames for NicDevice {
    fn frame<'a>(rx: RxToken) -> alloc::vec::Vec<u8> where Self: 'a { rx.buf }
}

/// The loopback stack's device: smoltcp's `phy::Loopback` (every frame
/// transmitted is received back, in order, Ethernet medium, MTU 65535), with
/// its frames reachable as owned buffers, so the gate needs no copy.
pub struct LoopDev {
    queue: alloc::collections::VecDeque<alloc::vec::Vec<u8>>,
}

impl LoopDev {
    pub(crate) fn new() -> Self { Self { queue: alloc::collections::VecDeque::new() } }
}

pub struct LoopTx<'a> {
    queue: &'a mut alloc::collections::VecDeque<alloc::vec::Vec<u8>>,
}

impl<'a> smoltcp::phy::TxToken for LoopTx<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R where F: FnOnce(&mut [u8]) -> R {
        let mut buf = alloc::vec![0u8; len];
        let r = f(&mut buf);
        self.queue.push_back(buf);
        r
    }
}

impl smoltcp::phy::Device for LoopDev {
    type RxToken<'a> = GateRx;
    type TxToken<'a> = LoopTx<'a>;

    fn receive(&mut self, _ts: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let f = self.queue.pop_front()?;
        Some((GateRx(f), LoopTx { queue: &mut self.queue }))
    }

    fn transmit(&mut self, _ts: Instant) -> Option<Self::TxToken<'_>> { Some(LoopTx { queue: &mut self.queue }) }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        let mut caps = smoltcp::phy::DeviceCapabilities::default();
        caps.max_transmission_unit = 65535;
        caps.medium = smoltcp::phy::Medium::Ethernet;
        caps
    }
}

impl Frames for LoopDev {
    fn frame<'a>(rx: GateRx) -> alloc::vec::Vec<u8> where Self: 'a { rx.0 }
}

/// Re-polls one stack may take in a daemon pass for held SYNs. A SYN still
/// held after that is dropped, which the peer sees as a lost segment and
/// retransmits — never an RST.
pub(crate) const MAX_REPOLLS: u32 = 64;

pub(crate) struct SynGate<'d, D: Frames> {
    dev: &'d mut D,
    /// A pure SYN went to smoltcp during the current poll.
    syn_taken: bool,
    /// The SYN held back for the next poll.
    held: Option<alloc::vec::Vec<u8>>,
}

impl<'d, D: Frames> SynGate<'d, D> {
    pub(crate) fn new(dev: &'d mut D) -> Self { Self { dev, syn_taken: false, held: None } }

    /// Whether a SYN is waiting for another poll; readies the gate for it.
    pub(crate) fn again(&mut self) -> bool {
        self.syn_taken = false;
        self.held.is_some()
    }
}

/// An Ethernet frame carrying an IPv4 TCP segment with SYN set and neither
/// ACK nor RST: the first segment of a connection.
fn pure_syn(f: &[u8]) -> bool {
    const ETH: usize = 14;
    if f.len() < ETH + 20 || f[12] != 0x08 || f[13] != 0x00 { return false; }
    if f[ETH] >> 4 != 4 || f[ETH + 9] != 6 { return false; }
    // A later fragment carries no TCP header.
    if u16::from_be_bytes([f[ETH + 6], f[ETH + 7]]) & 0x1fff != 0 { return false; }
    let tcp = ETH + ((f[ETH] & 0x0f) as usize) * 4;
    let Some(&flags) = f.get(tcp + 13) else { return false };
    flags & 0x16 == 0x02
}

pub struct GateRx(alloc::vec::Vec<u8>);

impl smoltcp::phy::RxToken for GateRx {
    fn consume<R, F>(mut self, f: F) -> R where F: FnOnce(&mut [u8]) -> R { f(&mut self.0) }
}

impl<'d, D: Frames> smoltcp::phy::Device for SynGate<'d, D> {
    type RxToken<'b> = GateRx where Self: 'b;
    type TxToken<'b> = D::TxToken<'b> where Self: 'b;

    fn receive(&mut self, ts: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if self.held.is_some() {
            if self.syn_taken { return None; }
            let tx = self.dev.transmit(ts)?;
            self.syn_taken = true;
            return self.held.take().map(|f| (GateRx(f), tx));
        }
        let (rx, tx) = self.dev.receive(ts)?;
        let f = D::frame(rx);
        if pure_syn(&f) {
            if self.syn_taken {
                self.held = Some(f);
                return None;
            }
            self.syn_taken = true;
        }
        Some((GateRx(f), tx))
    }

    fn transmit(&mut self, ts: Instant) -> Option<Self::TxToken<'_>> { self.dev.transmit(ts) }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities { self.dev.capabilities() }
}
