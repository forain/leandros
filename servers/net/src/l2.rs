//! The Ethernet switch behind a `bridge` vif (design-podnet3 L2).
//!
//! A bridge owns one smoltcp stack (its own L3 address, e.g. 10.88.0.1/16).
//! An enslaved veth end (a "port") owns no stack: frames the other end of the
//! pair sends arrive in the port's `rxq` and `tunbe::poll_all` hands them to
//! [`forward`]. Frames leaving the switch for a port are queued on that
//! port's peer (the container side); frames for the bridge itself go to the
//! bridge's `rxq`.
//!
//! The FDB learns the source MAC of every frame that enters on a port, ages
//! entries out after the bridge's AGEING_TIME, and unknown or broadcast
//! destinations are flooded to every other forwarding port (and, for
//! broadcast/multicast, the bridge itself).
//!
//! Locks: `VIFS` (leaf) first, then `L2`. `L2` is never held across anything.
#![allow(dead_code)]

use alloc::vec::Vec;
use spin::Mutex;

use crate::vifsel::{Sid, VifKind, NO_PEER, RXQ_CAP};
use crate::vif::VIFS;

/// Bridge parameters (netlink IFLA_BR_*; times in USER_HZ = 1/100 s).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct BrCfg { pub ageing: u32, pub fwd_delay: u32, pub stp: u32, pub vlan: u8 }
pub(crate) const DEFAULT_CFG: BrCfg = BrCfg { ageing: 30000, fwd_delay: 1500, stp: 0, vlan: 0 };

struct Ent { br: Sid, mac: [u8; 6], port: Sid, t: u64 }
struct State { cfg: Vec<(Sid, BrCfg)>, fdb: Vec<Ent> }
static L2: Mutex<State> = Mutex::new(State { cfg: Vec::new(), fdb: Vec::new() });
const FDB_CAP: usize = 1024;

pub(crate) fn cfg_set(br: Sid, c: BrCfg) {
    let mut s = L2.lock();
    if let Some(e) = s.cfg.iter_mut().find(|e| e.0 == br) { e.1 = c; } else { s.cfg.push((br, c)); }
}
pub(crate) fn cfg_get(br: Sid) -> BrCfg {
    L2.lock().cfg.iter().find(|e| e.0 == br).map_or(DEFAULT_CFG, |e| e.1)
}
/// Drop everything the switch knows about `sid` (a bridge or a port).
pub(crate) fn forget(sid: Sid) {
    let mut s = L2.lock();
    s.cfg.retain(|e| e.0 != sid);
    s.fdb.retain(|e| e.br != sid && e.port != sid);
}
/// Number of learned entries on `br` (tests / diagnostics).
pub(crate) fn fdb_len(br: Sid) -> usize { L2.lock().fdb.iter().filter(|e| e.br == br).count() }

fn now_cs() -> u64 { sched::monotonic_ns() / 10_000_000 }

/// Queue `f` on vif `to`'s rxq if it is up and the frame fits; true if queued.
fn push(v: &mut crate::vifsel::VifTable, to: Sid, f: &[u8]) -> bool {
    let Some(x) = v[to as usize].as_mut() else { return false };
    if !x.admin_up || f.len() > x.mtu as usize + 14 || x.rxq.len() >= RXQ_CAP { return false; }
    x.rxq.push_back(f.to_vec());
    x.pending = true;
    true
}

/// A frame leaves the switch through port `p`: it arrives at the port's peer.
fn out_port(v: &mut crate::vifsel::VifTable, p: Sid, f: &[u8]) {
    let Some(x) = v[p as usize].as_ref() else { return };
    if !x.admin_up { return; }
    let peer = x.peer;
    if peer != NO_PEER { push(v, peer, f); }
}

/// Switch `frames` that entered bridge `br` at `in_port` (a port sid, or `br`
/// itself for frames the bridge's own stack transmitted).
pub(crate) fn forward(br: Sid, in_port: Sid, frames: Vec<Vec<u8>>) {
    let mut any = false;
    {
        let mut v = VIFS.lock();
        let Some(b) = v[br as usize].as_ref() else { return };
        if b.kind != VifKind::Bridge || !b.admin_up { return; }
        let bmac = b.mac;
        let ports: Vec<Sid> = (0..v.len()).filter(|&i| v[i].as_ref()
            .map_or(false, |x| x.master == br && x.admin_up && x.kind == VifKind::Veth)).map(|i| i as Sid).collect();
        let mut s = L2.lock();
        let ageing = s.cfg.iter().find(|e| e.0 == br).map_or(DEFAULT_CFG.ageing, |e| e.1.ageing) as u64;
        let now = now_cs();
        if s.fdb.len() >= FDB_CAP / 2 { s.fdb.retain(|e| now.saturating_sub(e.t) < ageing); }
        for f in frames {
            if f.len() < 14 { continue; }
            let dst: [u8; 6] = f[0..6].try_into().unwrap();
            let src: [u8; 6] = f[6..12].try_into().unwrap();
            // Learn.
            if in_port != br && src[0] & 1 == 0 && src != [0; 6] {
                if let Some(e) = s.fdb.iter_mut().find(|e| e.br == br && e.mac == src) { e.port = in_port; e.t = now; }
                else {
                    if s.fdb.len() >= FDB_CAP { s.fdb.remove(0); }
                    s.fdb.push(Ent { br, mac: src, port: in_port, t: now });
                }
            }
            let flood = |v: &mut crate::vifsel::VifTable, to_bridge: bool| {
                for &p in ports.iter().filter(|&&p| p != in_port) { out_port(v, p, &f); }
                if to_bridge && in_port != br { push(v, br, &f); }
            };
            if dst[0] & 1 != 0 {
                flood(&mut v, true);
            } else if dst == bmac {
                if in_port != br { push(&mut v, br, &f); }
            } else {
                let hit = s.fdb.iter().find(|e| e.br == br && e.mac == dst && now.saturating_sub(e.t) < ageing)
                    .map(|e| e.port).filter(|p| ports.contains(p));
                match hit {
                    Some(p) if p == in_port => {}
                    Some(p) => out_port(&mut v, p, &f),
                    None => flood(&mut v, false),
                }
            }
            any = true;
        }
    }
    if any { drivers::netdev::kick(); }
}
