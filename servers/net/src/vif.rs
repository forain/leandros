//! Per-netns interface table: the dynamic stacks (sids >= `FIRST_DYN`) and
//! the per-netns routing state that goes with them.
//!
//! netns 0 keeps its fixed layout (sid 0 = lo, sids 1..=MAX_NETIF = NICs, see
//! `iface`). Every other stack is a dynamic sid owned by one netns: its `lo`
//! and, later, tun/tap and veth ends. Nothing creates a dynamic sid yet; this
//! is the bookkeeping the next lanes build on.
//!
//! Locks: `VIFS` and `NETNS` are leaves. Neither is held while a stack lock
//! is taken, and a stack lock is never held while taking either (stack lock
//! -> nothing). The namespace-death hook only sets a bit (it can run under
//! the run queue); the daemon reaps in `daemon_pass`.
#![allow(dead_code)]

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

use crate::iface::{Sid, FIRST_DYN};
pub(crate) use crate::vifsel::*;

/// `route_select` for a namespace other than 0: its own dynamic stacks only.
pub(crate) fn route_in_ns(ns: u16, dst: [u8; 4]) -> Option<Sid> {
    let n = NETNS.lock();
    let st = n.get(&ns)?;
    let v = VIFS.lock();
    select(st, &v, dst)
}

/// The dynamic sids of `ns` (empty until something creates one).
pub(crate) fn ns_sids_dyn(ns: u16) -> Vec<Sid> {
    NETNS.lock().get(&ns).map_or_else(Vec::new, |s| s.sids.clone())
}

/// Every live dynamic sid, any namespace (for lookups by listener id, /proc).
pub(crate) fn all_dyn_sids() -> Vec<Sid> {
    let v = VIFS.lock();
    (FIRST_DYN as usize..MAX_STACKS).filter(|&i| v[i].is_some()).map(|i| i as Sid).collect()
}

/// The user namespace that owns `ns`, for the CAP_NET_ADMIN check
/// (`ns_capable(CAP_NET_ADMIN, owner_userns(netns))`). 0 for the initial one.
pub(crate) fn owner_userns(ns: u16) -> u16 { sched::ns::owner_userns(1, ns) }

// ---- namespace death ------------------------------------------------------

pub(crate) static VIFS: Mutex<VifTable> = Mutex::new([const { None }; MAX_STACKS]);
pub(crate) static NETNS: Mutex<BTreeMap<u16, NetNsState>> = Mutex::new(BTreeMap::new());

static GONE: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// `sched::ns` hook: the last reference to flat namespace `id` dropped. Runs
/// in any context, so it only records the id.
fn on_ns_gone(id: sched::ns::NsId) {
    GONE[(id as usize / 64) & 3].fetch_or(1 << (id % 64), Ordering::AcqRel);
}

/// Install the hook (once, from the daemon's start).
pub(crate) fn init() { sched::ns::set_ns_gone_hook(on_ns_gone); }

/// Daemon pass: free the vifs and stacks of every namespace that died. A
/// reused id cannot race: the bit is cleared before the state is looked up,
/// and a new owner of the id creates its state afresh.
pub(crate) fn daemon_pass() {
    for w in 0..4 {
        if GONE[w].load(Ordering::Acquire) == 0 { continue; }
        let bits = GONE[w].swap(0, Ordering::AcqRel);
        for b in 0..64 {
            if bits & (1 << b) != 0 { reap((w * 64 + b) as u16); }
        }
    }
}

fn reap(ns: u16) {
    let Some(st) = NETNS.lock().remove(&ns) else { return };
    {
        let mut v = VIFS.lock();
        for &sid in st.sids.iter() { v[sid as usize] = None; }
    }
    // No stack lock is held here (VIFS/NETNS are released).
    for &sid in st.sids.iter() { *crate::STACKS[sid as usize].lock() = None; }
}
