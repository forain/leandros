//! The pure part of the per-netns interface table (see `vif`): types and the
//! route lookup over a fake-able table, so the host tests can include it.
#![allow(dead_code)]

use alloc::collections::VecDeque;
use alloc::vec::Vec;

pub(crate) type Sid = u8;
/// Size of the stack table: lo + the NICs + dynamic per-netns stacks.
pub(crate) const MAX_STACKS: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum VifKind { Lo, Tap, Tun, Veth, Bridge }

pub(crate) struct Vif {
    pub netns: u16,
    pub ifindex: u32,
    pub name: [u8; 16],
    pub kind: VifKind,
    pub admin_up: bool,
    pub mtu: u32,
    pub mac: [u8; 6],
    /// Addresses mirrored from the smoltcp Interface: (addr, prefix).
    pub addrs: Vec<([u8; 4], u8)>,
    /// Frames waiting to enter the stack (cap `RXQ_CAP`).
    pub rxq: VecDeque<Vec<u8>>,
    /// Bridge only: the port sid each `rxq` frame entered on, parallel to `rxq` (`NO_PEER` = unknown).
    pub rxsrc: VecDeque<Sid>,
    /// Tap/Tun: the tundev handle of the attached fd (`NO_TUN` = detached persistent).
    pub tun: u32,
    /// rxq non-empty or a timer is due: poll this pass.
    pub pending: bool,
    /// Veth: the other end's sid (`NO_PEER` = none). The ends may live in different netns.
    pub peer: Sid,
    /// Veth: the bridge this end is enslaved to (`NO_PEER` = none). A port owns no stack.
    pub master: Sid,
}

pub(crate) const NO_PEER: Sid = 255;
pub(crate) const NO_TUN: u32 = u32::MAX;
pub(crate) const RXQ_CAP: usize = 512;
pub(crate) const MAX_NS_SIDS: usize = 8;
/// netns 0 also hosts the bridge-side ends of every container's veth.
pub(crate) const MAX_NS0_SIDS: usize = 24;
pub(crate) fn ns_sid_cap(ns: u16) -> usize { if ns == 0 { MAX_NS0_SIDS } else { MAX_NS_SIDS } }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Route { pub dst: [u8; 4], pub prefix: u8, pub gw: Option<[u8; 4]>, pub oif: Sid, pub metric: u32 }

pub(crate) struct NetNsState {
    /// This namespace's loopback stack, created lazily (netns 0 uses `LO`).
    pub lo: Option<Sid>,
    /// Every dynamic sid of the namespace (lo first).
    pub sids: Vec<Sid>,
    pub routes: Vec<Route>,
    /// Next interface index: ns != 0 starts at 2 (lo is 1); ns 0 at 16.
    pub next_ifindex: u32,
}

impl NetNsState {
    pub(crate) fn new(ns: u16) -> Self {
        NetNsState { lo: None, sids: Vec::new(), routes: Vec::new(), next_ifindex: if ns == 0 { 16 } else { 2 } }
    }
}

pub(crate) type VifTable = [Option<Vif>; MAX_STACKS];

#[inline]
fn mask(a: [u8; 4], prefix: u8) -> u32 {
    let m = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix.min(32) as u32) };
    u32::from_be_bytes(a) & m
}

/// Route lookup in one namespace's own table: 127/8 to its lo when that is
/// up, else longest prefix over the explicit routes and the connected routes
/// of up interfaces. `None` = no route. Pure over its inputs.
pub(crate) fn select(st: &NetNsState, vifs: &VifTable, dst: [u8; 4]) -> Option<Sid> {
    let up = |sid: Sid| vifs.get(sid as usize).and_then(|v| v.as_ref()).map_or(false, |v| v.admin_up);
    if dst[0] == 127 {
        return st.lo.filter(|&l| up(l));
    }
    let mut best: Option<(u8, u32, Sid)> = None;
    let mut consider = |prefix: u8, metric: u32, sid: Sid| {
        let better = match best { None => true, Some((p, m, _)) => prefix > p || (prefix == p && metric < m) };
        if better { best = Some((prefix, metric, sid)); }
    };
    for &sid in st.sids.iter().filter(|&&s| Some(s) != st.lo) {
        let Some(v) = vifs[sid as usize].as_ref() else { continue };
        if !v.admin_up { continue; }
        for &(a, p) in v.addrs.iter() {
            if mask(a, p) == mask(dst, p) { consider(p, 0, sid); }
        }
    }
    for r in st.routes.iter() {
        if up(r.oif) && mask(r.dst, r.prefix) == mask(dst, r.prefix) { consider(r.prefix, r.metric, r.oif); }
    }
    best.map(|(_, _, s)| s)
}

#[cfg(test)]
mod tests {
    use super::*;
    const FIRST: Sid = 5;

    fn vif(up: bool, addrs: &[([u8; 4], u8)]) -> Vif {
        Vif { netns: 1, ifindex: 2, name: [0; 16], kind: VifKind::Lo, admin_up: up, mtu: 1500, mac: [0; 6],
              addrs: addrs.to_vec(), rxq: VecDeque::new(), rxsrc: VecDeque::new(), tun: NO_TUN, pending: false, peer: NO_PEER, master: NO_PEER }
    }

    #[test]
    fn route_select_fake_table() {
        let mut vifs: VifTable = [const { None }; MAX_STACKS];
        let (lo, tap) = (FIRST, FIRST + 1);
        vifs[lo as usize] = Some(vif(false, &[([127, 0, 0, 1], 8)]));
        vifs[tap as usize] = Some(vif(true, &[([10, 0, 2, 15], 24)]));
        let mut st = NetNsState::new(1);
        st.lo = Some(lo);
        st.sids = alloc::vec![lo, tap];
        // lo down: void, as today.
        assert_eq!(select(&st, &vifs, [127, 0, 0, 1]), None);
        assert_eq!(select(&st, &vifs, [10, 0, 2, 9]), Some(tap));
        assert_eq!(select(&st, &vifs, [8, 8, 8, 8]), None);
        st.routes.push(Route { dst: [0; 4], prefix: 0, gw: Some([10, 0, 2, 2]), oif: tap, metric: 0 });
        assert_eq!(select(&st, &vifs, [8, 8, 8, 8]), Some(tap));
        vifs[lo as usize].as_mut().unwrap().admin_up = true;
        assert_eq!(select(&st, &vifs, [127, 0, 0, 1]), Some(lo));
        vifs[tap as usize].as_mut().unwrap().admin_up = false;
        assert_eq!(select(&st, &vifs, [8, 8, 8, 8]), None);
    }
}
