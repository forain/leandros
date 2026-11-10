//! The Ethernet frame device behind a dynamic vif's smoltcp stack (tap, tun
//! and veth ends alike).
//!
//! `rx` was drained from the vif's queue before the stack lock was taken;
//! `tx` is handed on after it is dropped: to the tundev reader for a tap/tun,
//! to the peer end's `rxq` for a veth (see `tunbe::poll_all`).

use super::*;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use syngate::{Frames, GateRx};

/// The vif as a smoltcp device for one poll: `rx` was drained from the vif's
/// queue before the stack lock was taken, `tx` is delivered after it is dropped.
pub(crate) struct FrameDev { pub rx: VecDeque<Vec<u8>>, pub tx: Vec<Vec<u8>>, pub mtu: usize }
pub struct FrameTx<'a>(&'a mut Vec<Vec<u8>>);

impl<'a> smoltcp::phy::TxToken for FrameTx<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R where F: FnOnce(&mut [u8]) -> R {
        let mut buf = alloc::vec![0u8; len];
        let r = f(&mut buf);
        self.0.push(buf);
        r
    }
}

impl smoltcp::phy::Device for FrameDev {
    type RxToken<'a> = GateRx;
    type TxToken<'a> = FrameTx<'a>;

    fn receive(&mut self, _ts: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let f = self.rx.pop_front()?;
        Some((GateRx(f), FrameTx(&mut self.tx)))
    }
    fn transmit(&mut self, _ts: Instant) -> Option<Self::TxToken<'_>> {
        if self.tx.len() >= 256 { None } else { Some(FrameTx(&mut self.tx)) }
    }
    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        let mut caps = smoltcp::phy::DeviceCapabilities::default();
        caps.max_transmission_unit = self.mtu + 14;
        caps.medium = smoltcp::phy::Medium::Ethernet;
        caps
    }
}

impl Frames for FrameDev {
    fn frame<'a>(rx: GateRx) -> Vec<u8> where Self: 'a { rx.0 }
}

