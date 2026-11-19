//! MLDv2 host side (RFC 3810), the IPv6 twin of `igmp3.rs` (whose record
//! arithmetic, query-reply selection and checksum it shares): listener
//! reports (ICMPv6 type 143) built from the union of the per-socket source
//! filters, sent as raw frames through the stack's own device to ff02::16
//! with hop limit 1 and a Router Alert hop-by-hop option. Besides the
//! groups sockets joined, every unicast address (link-local included) makes
//! its interface a listener of the solicited-node group ff02::1:ffXX:XXXX
//! (`solicited`), found by `tick` from the interface's address list, so no
//! address-change site has to call in.
//!
//! A v1 query (24 bytes) starts the older-version querier timer (RFC 3810
//! 9.12): while it runs the interface answers with MLDv1 reports (type
//! 131, to the group) and sends MLDv1 Done (type 132, to ff02::2) on leave,
//! as Linux does. Groups of scope below link-local and ff02::1 are never
//! reported. One leaf lock (`V6`), taken as `igmp3`'s `V3` is.

use super::*;
use crate::igmp3::{self, Rec, State, ROBUSTNESS, TO_EX, TO_IN};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use smoltcp::phy::{Device, TxToken};

pub(crate) type A6 = [u8; 16];

/// Keep a report under the Ethernet MTU: 14 + 40 + 8 + 8 + records.
const MAX_REPORT: usize = 1300;
/// RFC 3810 9.12 older-version-querier-present timeout (robustness 2,
/// query interval 125 s, response interval 10 s).
const COMPAT_NS: u64 = 260_000_000_000;

struct Change { rec: Rec<A6>, left: u8 }
struct Reply { due: u64, group: Option<A6>, srcs: Vec<A6> }

#[derive(Default)]
struct IfState {
    /// What sockets want (from `mcast.rs`).
    user: BTreeMap<A6, State<A6>>,
    /// The solicited-node groups of the interface's addresses.
    sol: Vec<A6>,
    /// The reported state of every group.
    cur: BTreeMap<A6, State<A6>>,
    changes: Vec<Change>,
    next_change: u64,
    replies: Vec<Reply>,
    /// Until when an MLDv1 querier is considered present (ns).
    compat_until: u64,
}

static V6: Mutex<BTreeMap<Sid, IfState>> = Mutex::new(BTreeMap::new());

/// ff02::2 (all routers): where an MLDv1 Done goes.
const ALL_ROUTERS: A6 = [0xff, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2];

/// Linux reports no group of node-local scope and not ff02::1.
pub(crate) fn reportable(g: &A6) -> bool {
    g[0] == 0xff && g[1] & 0x0f >= 2 && !(g[1] == 2 && g[2..15] == [0; 13] && g[15] == 1)
}

/// The solicited-node multicast group of `a`.
fn sol_of(a: &A6) -> A6 {
    let mut g = [0u8; 16];
    g[0] = 0xff; g[1] = 2; g[11] = 1; g[12] = 0xff;
    g[13..].copy_from_slice(&a[13..]);
    g
}

/// The solicited-node groups of the unicast addresses `s` holds (none on
/// the loopback stacks, whose only address is ::1).
pub(crate) fn solicited(s: &NetStack) -> Vec<A6> {
    let mut v: Vec<A6> = Vec::new();
    for c in s.interface.ip_addrs() {
        if let IpAddress::Ipv6(a) = c.address() {
            if a.is_unspecified() || a.is_loopback() || a.is_multicast() { continue; }
            let g = sol_of(&a.0);
            if !v.contains(&g) { v.push(g); }
        }
    }
    v
}

/// Recompute the reported state of `g` from the socket state and the
/// solicited-node set, and queue the records that take the old to the new.
fn update(st: &mut IfState, g: A6) {
    let eff: Option<State<A6>> = st.user.get(&g).cloned()
        .or_else(|| if st.sol.contains(&g) { Some((false, Vec::new())) } else { None });
    let old = st.cur.get(&g).cloned();
    if eff == old { return; }
    let pending = st.changes.iter().any(|c| c.rec.group == g);
    match &eff { Some(n) => { st.cur.insert(g, n.clone()); } None => { st.cur.remove(&g); } }
    st.changes.retain(|c| c.rec.group != g);
    // A change while an older one is still being retransmitted: one
    // filter-mode record with the whole new state supersedes both.
    let recs = if pending {
        let (inc, srcs) = eff.clone().unwrap_or((true, Vec::new()));
        alloc::vec![Rec { group: g, ty: if inc { TO_IN } else { TO_EX }, srcs }]
    } else { igmp3::diff(old.as_ref(), eff.as_ref(), g) };
    if !recs.is_empty() { st.next_change = 0; }
    for r in recs { st.changes.push(Change { rec: r, left: ROBUSTNESS }); }
}

/// `mcast.rs`: the socket state of `group` on `sid` is now `new` (None: no
/// socket is a member any more).
pub(crate) fn changed(sid: Sid, group: A6, new: Option<State<A6>>) {
    if sid == LO || !reportable(&group) { return; }
    {
        let mut v = V6.lock();
        let st = v.entry(sid).or_default();
        match new { Some(n) => { st.user.insert(group, n); } None => { st.user.remove(&group); } }
        update(st, group);
        if st.changes.is_empty() { return; }
    }
    drivers::netdev::kick();
}

/// ICMPv6 checksum of `m` between `src` and `dst`.
fn icmp_csum(src: &A6, dst: &A6, m: &[u8]) -> u16 {
    let mut b = Vec::with_capacity(40 + m.len());
    b.extend_from_slice(src); b.extend_from_slice(dst);
    b.extend_from_slice(&(m.len() as u32).to_be_bytes());
    b.extend_from_slice(&[0, 0, 0, 58]);
    b.extend_from_slice(m);
    igmp3::csum(&b)
}

/// RFC 3810 5.1.3 maximum response code to ns.
fn max_resp_ns(code: u16, v2: bool) -> u64 {
    let ms = if !v2 || code < 0x8000 { code as u64 } else { (((code & 0xfff) as u64) | 0x1000) << (((code >> 12) & 7) + 3) };
    ms * 1_000_000
}

/// smoltcp's hook for an ICMPv6 type 130 message heard by the interface
/// being polled. True: consumed.
pub(crate) fn query_hook(src: A6, dst: A6, hop: u8, p: &[u8]) -> bool {
    // Hop limit 1 and a link-local source (RFC 3810 5.1.14, 5.1.15).
    if hop != 1 || src[0] != 0xfe || src[1] & 0xc0 != 0x80 { return true; }
    if p.len() < 24 || icmp_csum(&src, &dst, p) != 0 { return true; }
    let v2 = p.len() >= 28;
    if !v2 && p.len() != 24 { return true; }
    let mut group: A6 = [0; 16]; group.copy_from_slice(&p[8..24]);
    let general = group == [0; 16];
    if general { if dst != [0xff, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1] { return true; } }
    else if dst != group || group[0] != 0xff { return true; }
    let n = if v2 { u16::from_be_bytes([p[26], p[27]]) as usize } else { 0 };
    if p.len() < 28 + 16 * n && v2 { return true; }
    let srcs: Vec<A6> = (0..n).map(|i| { let mut a = [0u8; 16]; a.copy_from_slice(&p[28 + 16 * i..44 + 16 * i]); a }).collect();
    let now = sched::monotonic_ns();
    let due = now + igmp3::rnd(max_resp_ns(u16::from_be_bytes([p[4], p[5]]), v2));
    let sid = igmp3::poll_sid();
    let mut v = V6.lock();
    let st = v.entry(sid).or_default();
    if !v2 { st.compat_until = now + COMPAT_NS; }
    // A reply to an earlier general query that is due sooner covers this one.
    let earlier_general = st.replies.iter().any(|r| r.group.is_none() && r.due <= due);
    if general {
        if !earlier_general {
            st.replies.retain(|r| r.group.is_some());
            st.replies.push(Reply { due, group: None, srcs: Vec::new() });
        }
    } else if !earlier_general {
        match st.replies.iter_mut().find(|r| r.group == Some(group)) {
            Some(r) => {
                r.due = r.due.min(due);
                if r.srcs.is_empty() || srcs.is_empty() { r.srcs.clear(); }
                else { for s in srcs { if !r.srcs.contains(&s) { r.srcs.push(s); } } }
            }
            None => st.replies.push(Reply { due, group: Some(group), srcs }),
        }
    }
    true
}

/// One frame: Ethernet to the multicast MAC of `dst`, IPv6 with hop limit 1
/// and the Router Alert (MLD) hop-by-hop option, ICMPv6 message `m` (its
/// checksum field is filled here).
fn frame(src_mac: [u8; 6], src: &A6, dst: &A6, mut m: Vec<u8>) -> Vec<u8> {
    let c = icmp_csum(src, dst, &m); m[2..4].copy_from_slice(&c.to_be_bytes());
    let mut f = Vec::with_capacity(14 + 40 + 8 + m.len());
    f.extend_from_slice(&[0x33, 0x33, dst[12], dst[13], dst[14], dst[15]]);
    f.extend_from_slice(&src_mac);
    f.extend_from_slice(&[0x86, 0xdd]);
    f.extend_from_slice(&[0x60, 0, 0, 0]);
    f.extend_from_slice(&((8 + m.len()) as u16).to_be_bytes());
    f.extend_from_slice(&[0, 1]); // next header: hop-by-hop; hop limit 1
    f.extend_from_slice(src); f.extend_from_slice(dst);
    f.extend_from_slice(&[58, 0, 5, 2, 0, 0, 1, 0]); // -> ICMPv6; Router Alert (MLD); PadN
    f.extend_from_slice(&m);
    f
}

/// A v2 report carrying `recs`.
fn report(recs: &[Rec<A6>]) -> Vec<u8> {
    let mut m = alloc::vec![143u8, 0, 0, 0, 0, 0, 0, 0];
    m[6..8].copy_from_slice(&(recs.len() as u16).to_be_bytes());
    for r in recs {
        m.push(r.ty); m.push(0);
        m.extend_from_slice(&(r.srcs.len() as u16).to_be_bytes());
        m.extend_from_slice(&r.group);
        for s in &r.srcs { m.extend_from_slice(s); }
    }
    m
}

/// An MLDv1 Report (131) or Done (132) for `group`.
fn v1_msg(ty: u8, group: &A6) -> Vec<u8> {
    let mut m = alloc::vec![ty, 0, 0, 0, 0, 0, 0, 0];
    m.extend_from_slice(group);
    m
}

fn rec_len(r: &Rec<A6>) -> usize { 20 + 16 * r.srcs.len() }

/// The frames for `recs`, in as many reports as the size limit needs.
fn report_frames(mac: [u8; 6], src: &A6, recs: &[Rec<A6>]) -> Vec<Vec<u8>> {
    let dst: A6 = [0xff, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x16];
    let mut out = Vec::new();
    let mut i = 0;
    while i < recs.len() {
        let (mut j, mut len) = (i, 0);
        while j < recs.len() && (j == i || len + rec_len(&recs[j]) <= MAX_REPORT) { len += rec_len(&recs[j]); j += 1; }
        out.push(frame(mac, src, &dst, report(&recs[i..j])));
        i = j;
    }
    out
}

/// Send `frames`; false when the device took none of them (try again later).
fn send<D: Device>(dev: &mut D, ts: Instant, frames: &[Vec<u8>]) -> bool {
    for (i, f) in frames.iter().enumerate() {
        let Some(tok) = dev.transmit(ts) else { return i != 0 };
        tok.consume(f.len(), |b| b.copy_from_slice(f));
    }
    true
}

/// A member group in v1 terms: not INCLUDE of nothing.
fn member(st: &State<A6>) -> bool { !(st.0 && st.1.is_empty()) }

/// Called by `poll_stack` before each poll (stack lock held): follow the
/// address list, send what is due.
pub(crate) fn tick<D: Device>(s: &mut NetStack, dev: &mut D, sid: Sid, ts: Instant) {
    if sid == LO || !s.interface.igmp_v3_enabled() { return; }
    let HardwareAddress::Ethernet(EthernetAddress(mac)) = s.interface.hardware_addr() else { return };
    let now = sched::monotonic_ns();
    let sol = solicited(s);
    let mut src: A6 = [0; 16];
    for c in s.interface.ip_addrs() {
        if let IpAddress::Ipv6(a) = c.address() { if a.is_link_local() { src = a.0; break; } }
    }
    let mut frames: Vec<Vec<u8>> = Vec::new();
    let (mut chg, mut rep) = (false, 0usize);
    {
        let mut v = V6.lock();
        if !v.contains_key(&sid) && sol.is_empty() { return; }
        let st = v.entry(sid).or_default();
        if st.sol != sol {
            let old = core::mem::replace(&mut st.sol, sol);
            let all: Vec<A6> = old.iter().chain(st.sol.iter()).copied().collect();
            for g in all { update(st, g); }
        }
        let compat = now < st.compat_until;
        let mut recs: Vec<Rec<A6>> = Vec::new();
        let mut v1: Vec<(u8, A6)> = Vec::new();
        if !st.changes.is_empty() && now >= st.next_change {
            chg = true;
            if compat {
                for c in &st.changes {
                    let g = c.rec.group;
                    let m = st.cur.get(&g).map_or(false, member);
                    let e = (if m { 131 } else { 132 }, g);
                    if !v1.contains(&e) { v1.push(e); }
                }
            } else { for c in &st.changes { recs.push(c.rec.clone()); } }
        }
        let mut due: Vec<&Reply> = st.replies.iter().filter(|r| now >= r.due).collect();
        due.sort_by_key(|r| r.due);
        rep = due.len();
        for r in due {
            if compat {
                let gs: Vec<A6> = match r.group { None => st.cur.keys().copied().collect(), Some(g) => alloc::vec![g] };
                for g in gs {
                    if st.cur.get(&g).map_or(false, member) && !v1.contains(&(131, g)) { v1.push((131, g)); }
                }
            } else { igmp3::reply_recs_g(&st.cur, r.group, &r.srcs, &mut recs); }
        }
        if !chg && rep == 0 { return; }
        // Merge records of one group and type that two causes both produced.
        let mut seen: Vec<(A6, u8)> = Vec::new();
        recs.retain(|r| { let k = (r.group, r.ty); if seen.contains(&k) { false } else { seen.push(k); true } });
        if !recs.is_empty() { frames = report_frames(mac, &src, &recs); }
        for (ty, g) in v1 {
            let dst = if ty == 131 { g } else { ALL_ROUTERS };
            frames.push(frame(mac, &src, &dst, v1_msg(ty, &g)));
        }
    }
    if !frames.is_empty() && !send(dev, ts, &frames) { return; }
    let mut v = V6.lock();
    let Some(st) = v.get_mut(&sid) else { return };
    if chg {
        for c in st.changes.iter_mut() { c.left -= 1; }
        st.changes.retain(|c| c.left > 0);
        st.next_change = now + 100_000_000 + igmp3::rnd(igmp3::UNSOLICITED_NS - 100_000_000);
    }
    if rep != 0 { st.replies.retain(|r| now < r.due); }
}

/// ns until the earliest pending report on any interface (0 = due now).
pub(crate) fn next_due(now: u64) -> Option<u64> {
    let v = V6.lock();
    let mut best: Option<u64> = None;
    let mut take = |t: u64| best = Some(best.map_or(t, |b| b.min(t)));
    for st in v.values() {
        if !st.changes.is_empty() { take(st.next_change); }
        for r in &st.replies { take(r.due); }
    }
    best.map(|t| t.saturating_sub(now))
}
