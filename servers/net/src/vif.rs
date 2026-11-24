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
    let n = NETNS.lock();
    let Some(s) = n.get(&ns) else { return Vec::new() };
    let v = VIFS.lock();
    // Bridge ports own no stack.
    s.sids.iter().copied().filter(|&q| v[q as usize].as_ref().map_or(true, |x| x.master == NO_PEER)).collect()
}

/// Every live dynamic sid, any namespace (for lookups by listener id, /proc).
pub(crate) fn all_dyn_sids() -> Vec<Sid> {
    let v = VIFS.lock();
    (FIRST_DYN as usize..MAX_STACKS).filter(|&i| v[i].as_ref().map_or(false, |x| x.master == NO_PEER)).map(|i| i as Sid).collect()
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
pub(crate) fn init() {
    sched::ns::set_ns_gone_hook(on_ns_gone);
    sched::ns::set_lo_hook(set_lo);
    sched::ns::set_ifnames_hook(ifnames);
}

/// `sched::ns` hook: the `idx`-th non-loopback link of `ns` (0 = none), for /proc/sys/net/*/conf/<if>.
fn ifnames(ns: u16, idx: usize, out: &mut [u8; 16]) -> usize {
    // The initial namespace's links are the NICs, then its dynamic ones.
    let mut idx = idx;
    if ns == 0 {
        let nics = drivers::netdev::count().min(drivers::netdev::MAX_NETIF);
        if idx < nics {
            let Some(n) = drivers::netdev::iter().nth(idx) else { return 0 };
            let name = n.name();
            let l = name.len().min(15);
            out[..l].copy_from_slice(&name.as_bytes()[..l]);
            return l;
        }
        idx -= nics;
    }
    let v = VIFS.lock();
    let mut n = 0;
    for x in v.iter().flatten() {
        if x.netns != ns || x.kind == VifKind::Lo { continue; }
        let l = x.name.iter().position(|&c| c == 0).unwrap_or(16);
        if l == 0 { continue; }
        if n == idx { out[..l].copy_from_slice(&x.name[..l]); return l; }
        n += 1;
    }
    0
}

/// The sid of `ns`'s loopback stack when its lo is up (never for ns 0).
pub(crate) fn lo_up_sid(ns: u16) -> Option<Sid> {
    if ns == 0 { return None; }
    let n = NETNS.lock();
    let l = n.get(&ns)?.lo?;
    let v = VIFS.lock();
    v[l as usize].as_ref().filter(|x| x.admin_up).map(|_| l)
}

/// A previous owner of namespace id `ns` may not have been reaped yet: do it
/// now, before the new owner creates or looks up any state under the id.
pub(crate) fn claim(ns: u16) {
    let (w, b) = (ns as usize / 64 & 3, ns % 64);
    if GONE[w].fetch_and(!(1 << b), Ordering::AcqRel) & (1 << b) != 0 { reap(ns); }
}

/// Bring `ns`'s `lo` up or down. The first up allocates its stack (127.0.0.1/8,
/// the same `LoopDev` as sid 0); later ups and downs only flip the admin bit,
/// the stack and its sockets stay.
pub(crate) fn set_lo(ns: u16, up: bool) {
    if ns == 0 { return; }
    claim(ns);
    // Existing lo: flip the bit.
    {
        let n = NETNS.lock();
        if let Some(l) = n.get(&ns).and_then(|s| s.lo) {
            let mut v = VIFS.lock();
            if let Some(x) = v[l as usize].as_mut() { x.admin_up = up; }
            return;
        }
    }
    if !up { return; }
    // Slots exhausted: namespaces that died since the daemon's last pass.
    if !VIFS.lock()[FIRST_DYN as usize..].iter().any(|x| x.is_none()) { daemon_pass(); }
    // Still full: dead namespaces in their drain grace give their sids up now.
    if !VIFS.lock()[FIRST_DYN as usize..].iter().any(|x| x.is_none()) { finish_dying(true); }
    let sid = {
        let mut n = NETNS.lock();
        let mut v = VIFS.lock();
        let Some(sid) = (FIRST_DYN as usize..MAX_STACKS).find(|&i| v[i].is_none()) else { return };
        let st = n.entry(ns).or_insert_with(|| NetNsState::new(ns));
        let mut name = [0u8; 16];
        name[..2].copy_from_slice(b"lo");
        v[sid] = Some(Vif {
            netns: ns, ifindex: 1, name, kind: VifKind::Lo, admin_up: true, mtu: 65536, mac: [0; 6],
            addrs: alloc::vec![([127, 0, 0, 1], 8)], addrs6: Vec::new(), routes6: Vec::new(), rxq: Default::default(), rxsrc: Default::default(), tun: NO_TUN, pending: false, peer: NO_PEER, master: NO_PEER,
        });
        st.lo = Some(sid as Sid);
        st.sids.insert(0, sid as Sid);
        sid as Sid
    };
    let mut st = crate::make_loopback_stack();
    st.interface.set_pmtu_scope(sid as u16);
    smoltcp::pmtu::clear_scope(sid as u16);
    *crate::STACKS[sid as usize].lock() = Some(st);
}

/// Daemon pass: free the vifs and stacks of every namespace that died. A
/// reused id cannot race: the bit is cleared before the state is looked up,
/// and a new owner of the id creates its state afresh.
pub(crate) fn daemon_pass() {
    // A closed or exited process's netlink socket still holds its netns.
    crate::nlsock::prune();
    for w in 0..4 {
        if GONE[w].load(Ordering::Acquire) == 0 { continue; }
        let bits = GONE[w].swap(0, Ordering::AcqRel);
        for b in 0..64 {
            if bits & (1 << b) != 0 { reap((w * 64 + b) as u16); }
        }
    }
    finish_dying(false);
}

/// Free the given (netns, sid) vifs wherever they live: drop them from their
/// namespace's sid list and routes, then the table slot, then the stack.
pub(crate) fn free_vifs(list: &[(u16, Sid)]) {
    {
        let mut n = NETNS.lock();
        for &(ns, sid) in list {
            if let Some(st) = n.get_mut(&ns) {
                st.sids.retain(|&s| s != sid);
                st.routes.retain(|r| r.oif != sid);
            }
        }
    }
    for &(_, sid) in list { crate::l2::forget(sid); }
    // No table lock is held: the stack locks are taken alone.
    for &(_, sid) in list { *crate::STACKS[sid as usize].lock() = None; }
    // The VIFS slot (the sid allocator's "taken" mark) is released only after
    // the stack is gone: freeing it first let a new veth reuse the sid and have
    // its fresh stack wiped by this late `= None`.
    let mut gone: Vec<(u16, [u8; 16])> = Vec::new();
    {
        let mut v = VIFS.lock();
        for &(_, sid) in list { if let Some(x) = v[sid as usize].take() { gone.push((x.netns, x.name)); } }
    }
    for (ns, name) in gone { forget_ifctl(ns, &name); }
}

/// Drop the per-interface sysctl state of a deleted link (no table lock may be held: IFCTL takes VIFS).
pub(crate) fn forget_ifctl(ns: u16, name: &[u8; 16]) {
    let n = name.iter().position(|&c| c == 0).unwrap_or(16);
    sched::ns::ifctl_forget(ns, &name[..n]);
}

fn reap(ns: u16) {
    let Some(st) = NETNS.lock().remove(&ns) else { return };
    // Veth ends in this namespace take their peers (any namespace) with them.
    let mut peers: Vec<(u16, Sid)> = Vec::new();
    {
        let mut v = VIFS.lock();
        for &sid in st.sids.iter() {
            if let Some(x) = v[sid as usize].as_ref() {
                if x.kind == VifKind::Veth && x.peer != NO_PEER {
                    if let Some(p) = v[x.peer as usize].as_ref() {
                        if p.netns != ns { peers.push((p.netns, x.peer)); }
                    }
                }
            }
        }
    }
    // The namespace id is free for reuse from here on (its NETNS entry is
    // gone); its stacks and those of the veth peers it takes along live on
    // in DYING until their TCP traffic has drained (`finish_dying`).
    DYING.lock().push(Dying { sids: st.sids, peers, deadline: sched::ticks() + REAP_GRACE_TICKS, hard: sched::ticks() + REAP_HARD_TICKS, mark: usize::MAX });
    finish_dying(false);
}

/// A dead namespace whose stacks are not freed yet.
struct Dying { sids: Vec<Sid>, peers: Vec<(u16, Sid)>, deadline: u64, hard: u64, mark: usize }
static DYING: Mutex<Vec<Dying>> = Mutex::new(Vec::new());

/// Longest a dead namespace's stacks wait for their TCP traffic (2 s at
/// 100 ticks/s): bounded, so a veth end still goes within seconds.
const REAP_GRACE_TICKS: u64 = 200;

/// Hard cap on a dead namespace's life while its TCP data keeps draining:
/// the 2 s grace restarts whenever the queued bytes change (a loaded host can
/// stretch a 192 KiB stream past a flat 2 s and the tail was dropped), but a
/// stalled peer still frees the stacks 2 s after the last progress.
const REAP_HARD_TICKS: u64 = 3000;

/// Whether stack `sid` still carries TCP data in flight: an orphaned
/// connection of the exited process whose reply or FIN is not yet acked, or a
/// live socket (on a veth peer in another namespace) holding bytes its owner
/// has not read yet. Freeing the stack then would drop them: the process's
/// last write() before exit never reached the peer, and the peer's read()
/// lost bytes already delivered. Linux keeps such sockets (and their netns)
/// alive past the owner's exit; this keeps the stacks for a bounded grace.
/// `Some(queued bytes)` while stack `sid` is busy (a live connection holding unsent, unacked or unread bytes), else `None`.
fn tcp_progress(sid: Sid) -> Option<usize> {
    use smoltcp::socket::{tcp, udp, AnySocket};
    let st = crate::STACKS[sid as usize].lock();
    let s = st.as_ref()?;
    let mut busy = false;
    let mut bytes = 0usize;
    for (_, sock) in s.socket_set.iter() {
        let Some(t) = tcp::Socket::downcast(sock) else {
            // A datagram the owner sent just before exiting (a UDP server's last reply) is still queued
            // for the daemon's next pass: Linux transmits at sendto time, so it must not be lost.
            if udp::Socket::downcast(sock).map_or(false, |u| u.send_pending()) { busy = true; bytes += 1; }
            continue
        };
        match t.state() {
            tcp::State::Closed | tcp::State::Listen | tcp::State::TimeWait => {}
            tcp::State::FinWait1 | tcp::State::Closing | tcp::State::LastAck => {
                busy = true;
                bytes += t.send_queue() + t.recv_queue();
            }
            _ => {
                if t.send_queue() > 0 || t.recv_queue() > 0 {
                    busy = true;
                    bytes += t.send_queue() + t.recv_queue();
                }
            }
        }
    }
    if busy { Some(bytes) } else { None }
}

fn x_sids_peers(x: &Dying) -> Vec<Sid> {
    x.sids.iter().copied().chain(x.peers.iter().map(|&(_, s)| s)).collect()
}

/// Frames queued for stack-less port `sid` (a bridge port whose container end just sent them) that
/// the daemon has not switched yet: freeing the port first would drop the exited process's last
/// datagrams.
fn port_rx_busy(sid: Sid) -> bool {
    VIFS.lock()[sid as usize].as_ref().map_or(false, |x| !x.rxq.is_empty())
}

/// Free every dead namespace whose stacks have drained or whose grace ran
/// out (`force`: all of them, for an exhausted sid table).
fn finish_dying(force: bool) {
    let now = sched::ticks();
    let ready: Vec<Dying> = {
        let mut d = DYING.lock();
        if d.is_empty() { return; }
        let mut out = Vec::new();
        let mut i = 0;
        while i < d.len() {
            // Stack locks are leaves: taking them under DYING is safe.
            let mut busy = false;
            if !force {
                let mut sum = 0usize;
                let mut any = false;
                for p in x_sids_peers(&d[i]) {
                    if let Some(b) = tcp_progress(p) { any = true; sum = sum.wrapping_add(b).wrapping_add(1); }
                }
                // A bridge peer port with received frames still queued for the daemon keeps the stack alive too, but queued
                // frames are not progress (a flow with no conntrack retransmits FINs forever): they stay out of `sum`.
                for &(_, s) in d[i].peers.iter() {
                    if port_rx_busy(s) { any = true; }
                }
                // Progress (the queued bytes moved) restarts the grace, up to the hard cap.
                if any && sum != d[i].mark && now < d[i].hard {
                    d[i].mark = sum;
                    d[i].deadline = (now + REAP_GRACE_TICKS).min(d[i].hard);
                }
                busy = any && now < d[i].deadline;
            }
            if busy { i += 1; } else { out.push(d.swap_remove(i)); }
        }
        out
    };
    for x in ready {
        free_vifs(&x.peers);
        for &sid in x.sids.iter() { crate::l2::forget(sid); }
        // No stack lock is held here (VIFS/NETNS are released).
        for &sid in x.sids.iter() { *crate::STACKS[sid as usize].lock() = None; }
        // Release the sids only now (see free_vifs): a reused sid must not lose its new stack.
        let mut gone: Vec<(u16, [u8; 16])> = Vec::new();
        {
            let mut v = VIFS.lock();
            for &sid in x.sids.iter() { if let Some(g) = v[sid as usize].take() { gone.push((g.netns, g.name)); } }
        }
        for (gns, name) in gone { forget_ifctl(gns, &name); }
    }
}
