//! IGMPv3 host side (RFC 3376): membership reports built from the union of
//! the per-socket source filters (`mcast.rs` hands over the interface state
//! of a group whenever it changes), sent as raw frames through the stack's
//! own device. smoltcp (vendored, IGMP v1/v2 only) stays quiet for these
//! interfaces: its v2 report/leave are off while `igmp_v3` is active, and a
//! v3 query goes to `query_hook`. A v1/v2 querier puts the interface into
//! that older version's compatibility mode (smoltcp answers it itself).
//!
//! State per interface: the reported state of every group (`cur`), the
//! state-change records still to be (re)sent (`robustness` = 2 times), and
//! the query replies waiting for their random delay. One leaf lock (`V3`):
//! the query hook takes it under a stack lock, `changed` under `GROUPS`.

use super::*;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::AtomicU8;
use smoltcp::phy::{Device, TxToken};

pub(crate) type A4 = [u8; 4];
/// The interface state of one group: (include?, sources).
pub(crate) type State<A = A4> = (bool, Vec<A>);

pub(crate) const ROBUSTNESS: u8 = 2;
pub(crate) const IS_IN: u8 = 1; pub(crate) const IS_EX: u8 = 2; pub(crate) const TO_IN: u8 = 3; pub(crate) const TO_EX: u8 = 4;
pub(crate) const ALLOW: u8 = 5; pub(crate) const BLOCK: u8 = 6;
/// Unsolicited report interval (RFC 3376 8.14.2): retransmissions of a
/// state change are spread over 1 s.
pub(crate) const UNSOLICITED_NS: u64 = 1_000_000_000;
/// Keep a report under the Ethernet MTU: 14 + 24 + 8 + records.
const MAX_REPORT: usize = 1400;

#[derive(Clone)]
pub(crate) struct Rec<A = A4> { pub group: A, pub ty: u8, pub srcs: Vec<A> }

struct Change { rec: Rec, left: u8 }

/// A query reply waiting for its delay: all groups (`group` None) or one,
/// with the sources the query listed (none = the whole group).
struct Reply { due: u64, group: Option<A4>, srcs: Vec<A4> }

#[derive(Default)]
struct IfState {
    cur: BTreeMap<A4, State>,
    changes: Vec<Change>,
    next_change: u64,
    replies: Vec<Reply>,
}

static V3: Mutex<BTreeMap<Sid, IfState>> = Mutex::new(BTreeMap::new());
/// The stack being polled (set by `poll_stack`), so the query hook knows
/// which interface heard the query.
static POLL_SID: AtomicU8 = AtomicU8::new(0);

/// Stack `sid` was removed.
pub(crate) fn forget(sid: Sid) { V3.lock().remove(&sid); }

pub(crate) fn set_poll_sid(sid: Sid) { POLL_SID.store(sid, Ordering::Relaxed); }
pub(crate) fn poll_sid() -> Sid { POLL_SID.load(Ordering::Relaxed) as Sid }

pub(crate) fn rnd(max: u64) -> u64 {
    if max == 0 { return 0; }
    let mut x = sched::monotonic_ns() ^ 0x9e37_79b9_7f4a_7c15;
    x ^= x << 13; x ^= x >> 7; x ^= x << 17;
    x % max
}

pub(crate) fn minus<A: Copy + PartialEq>(a: &[A], b: &[A]) -> Vec<A> { a.iter().filter(|x| !b.contains(x)).copied().collect() }

/// RFC 3376 table 3: the records that take the interface from `old` to
/// `new` (a missing state is INCLUDE(empty): not a member).
pub(crate) fn diff<A: Copy + PartialEq>(old: Option<&State<A>>, new: Option<&State<A>>, group: A) -> Vec<Rec<A>> {
    let empty: State<A> = (true, Vec::new());
    let (oi, oa) = old.unwrap_or(&empty);
    let (ni, nb) = new.unwrap_or(&empty);
    let mut v = Vec::new();
    let mut push = |ty: u8, srcs: Vec<A>| v.push(Rec { group, ty, srcs });
    match (*oi, *ni) {
        (true, true) => {
            let (a, b) = (minus(nb, oa), minus(oa, nb));
            if !a.is_empty() { push(ALLOW, a); }
            if !b.is_empty() { push(BLOCK, b); }
        }
        (false, false) => {
            let (a, b) = (minus(oa, nb), minus(nb, oa));
            if !a.is_empty() { push(ALLOW, a); }
            if !b.is_empty() { push(BLOCK, b); }
        }
        (true, false) => push(TO_EX, nb.clone()),
        (false, true) => push(TO_IN, nb.clone()),
    }
    v
}

/// `mcast.rs`: the interface state of `group` on `sid` is now `new`
/// (None: no socket is a member any more).
pub(crate) fn changed(sid: Sid, group: A4, new: Option<State>) {
    let recs = {
        let mut v = V3.lock();
        let st = v.entry(sid).or_default();
        let old = st.cur.get(&group).cloned();
        let pending = st.changes.iter().any(|c| c.rec.group == group);
        match &new { Some(n) => { st.cur.insert(group, n.clone()); } None => { st.cur.remove(&group); } }
        st.changes.retain(|c| c.rec.group != group);
        // A change while an older one is still being retransmitted: one
        // filter-mode record with the whole new state supersedes both.
        let recs = if pending {
            let (inc, srcs) = new.clone().unwrap_or((true, Vec::new()));
            alloc::vec![Rec { group, ty: if inc { TO_IN } else { TO_EX }, srcs }]
        } else { diff(old.as_ref(), new.as_ref(), group) };
        let n = recs.len();
        for r in recs { st.changes.push(Change { rec: r, left: ROBUSTNESS }); }
        if n != 0 { st.next_change = 0; }
        n
    };
    if recs != 0 { drivers::netdev::kick(); }
}

/// Internet checksum of `b` folded to 16 bits (0xffff-complement).
pub(crate) fn csum(b: &[u8]) -> u16 {
    let mut s: u32 = 0;
    let mut i = 0;
    while i + 1 < b.len() { s += u16::from_be_bytes([b[i], b[i + 1]]) as u32; i += 2; }
    if i < b.len() { s += (b[i] as u32) << 8; }
    while s >> 16 != 0 { s = (s & 0xffff) + (s >> 16); }
    !(s as u16)
}

/// RFC 3376 4.1.1: max resp code to tenths of a second.
fn max_resp_tenths(c: u8) -> u64 {
    if c < 128 { c as u64 } else { (((c & 0x0f) as u64) | 0x10) << (((c >> 4) & 7) + 3) }
}

/// smoltcp's hook for an IGMP message of 12 bytes or more of type 0x11 (a
/// v3 query) heard by the interface being polled. True: consumed.
pub(crate) fn query_hook(dst: A4, p: &[u8]) -> bool {
    let n = u16::from_be_bytes([p[10], p[11]]) as usize;
    if p.len() < 12 + 4 * n || csum(p) != 0 { return true; } // malformed: dropped
    let group: A4 = [p[4], p[5], p[6], p[7]];
    let general = group == [0; 4];
    if general { if dst != [224, 0, 0, 1] { return true; } }
    else if dst != group || group[0] & 0xf0 != 0xe0 { return true; }
    let srcs: Vec<A4> = (0..n).map(|i| { let o = 12 + 4 * i; [p[o], p[o + 1], p[o + 2], p[o + 3]] }).collect();
    let max_ns = max_resp_tenths(p[1]) * 100_000_000;
    let due = sched::monotonic_ns() + rnd(max_ns);
    let sid = POLL_SID.load(Ordering::Relaxed) as Sid;
    let mut v = V3.lock();
    let st = v.entry(sid).or_default();
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

/// MODE_IS records for `group` (or all groups) as the query asked.
fn reply_recs(st: &IfState, r: &Reply, out: &mut Vec<Rec>) { reply_recs_g(&st.cur, r.group, &r.srcs, out); }

/// The same for any address width (MLDv2 reuses it): `cur` is the reported
/// state per group, `group` the query's group (None = general), `qsrcs` the
/// sources it listed.
pub(crate) fn reply_recs_g<A: Copy + PartialEq + Ord>(cur: &BTreeMap<A, State<A>>, group: Option<A>, qsrcs: &[A], out: &mut Vec<Rec<A>>) {
    let mut one = |group: A, state: &State<A>| {
        let (inc, a) = state;
        if qsrcs.is_empty() {
            if *inc && a.is_empty() { return; }
            out.push(Rec { group, ty: if *inc { IS_IN } else { IS_EX }, srcs: a.clone() });
        } else {
            // Group-and-source-specific (RFC 3376 5.2): INCLUDE answers the
            // sources it forwards, EXCLUDE those it does not block.
            let s: Vec<A> = if *inc { qsrcs.iter().filter(|x| a.contains(x)).copied().collect() }
                            else { minus(qsrcs, a) };
            if !s.is_empty() { out.push(Rec { group, ty: IS_IN, srcs: s }); }
        }
    };
    match group {
        None => for (g, s) in cur.iter() { one(*g, s); },
        Some(g) => if let Some(s) = cur.get(&g) { one(g, s); },
    }
}

/// One v3 report frame carrying `recs`: Ethernet to 01:00:5e:00:00:16, IPv4
/// to 224.0.0.22, TTL 1, TOS 0xc0, router-alert option.
fn frame(src_mac: [u8; 6], src_ip: A4, recs: &[Rec], id: u16) -> Vec<u8> {
    let mut m = alloc::vec![0x22u8, 0, 0, 0, 0, 0, 0, 0];
    m[6..8].copy_from_slice(&(recs.len() as u16).to_be_bytes());
    for r in recs {
        m.push(r.ty); m.push(0);
        m.extend_from_slice(&(r.srcs.len() as u16).to_be_bytes());
        m.extend_from_slice(&r.group);
        for s in &r.srcs { m.extend_from_slice(s); }
    }
    let c = csum(&m); m[2..4].copy_from_slice(&c.to_be_bytes());
    let mut ip = alloc::vec![0x46u8, 0xc0, 0, 0, (id >> 8) as u8, id as u8, 0x40, 0, 1, 2, 0, 0];
    ip.extend_from_slice(&src_ip);
    ip.extend_from_slice(&[224, 0, 0, 22]);
    ip.extend_from_slice(&[0x94, 0x04, 0, 0]);
    let total = (ip.len() + m.len()) as u16;
    ip[2..4].copy_from_slice(&total.to_be_bytes());
    let c = csum(&ip); ip[10..12].copy_from_slice(&c.to_be_bytes());
    let mut f = Vec::with_capacity(14 + total as usize);
    f.extend_from_slice(&[0x01, 0x00, 0x5e, 0x00, 0x00, 0x16]);
    f.extend_from_slice(&src_mac);
    f.extend_from_slice(&[0x08, 0x00]);
    f.extend_from_slice(&ip);
    f.extend_from_slice(&m);
    f
}

fn rec_len(r: &Rec) -> usize { 8 + 4 * r.srcs.len() }

static IP_ID: AtomicU32 = AtomicU32::new(1);

/// Send `recs` in as many reports as the size limit needs; false when the
/// device took none of them (try again later).
fn send<D: Device>(s: &mut NetStack, dev: &mut D, ts: Instant, recs: &[Rec]) -> bool {
    let HardwareAddress::Ethernet(EthernetAddress(mac)) = s.interface.hardware_addr() else { return true };
    let ip = s.interface.ipv4_addr().map_or([0; 4], |a| a.0);
    let mut i = 0;
    while i < recs.len() {
        let (mut j, mut len) = (i, 0);
        while j < recs.len() && (j == i || len + rec_len(&recs[j]) <= MAX_REPORT) { len += rec_len(&recs[j]); j += 1; }
        let id = IP_ID.fetch_add(1, Ordering::Relaxed) as u16;
        let f = frame(mac, ip, &recs[i..j], id);
        let Some(tok) = dev.transmit(ts) else { return false };
        tok.consume(f.len(), |b| b.copy_from_slice(&f));
        i = j;
    }
    true
}

/// Called by `poll_stack` before each poll (stack lock held): send what is
/// due. Interfaces not speaking v3 (a v1/v2 querier is present) drop the
/// pending work: smoltcp's own v1/v2 handling answers there.
pub(crate) fn tick<D: Device>(s: &mut NetStack, dev: &mut D, sid: Sid, ts: Instant) {
    let now = sched::monotonic_ns();
    // Linux runs no IGMP on loopback: it reports V3 in /proc/net/igmp but
    // nothing is ever queued or sent there.
    let lo = matches!(s.interface.hardware_addr(), HardwareAddress::Ethernet(EthernetAddress([0x02, 0, 0, 0, 0, 1])));
    let v3 = s.interface.igmp_version(ts) == 3 && !lo;
    let mut recs: Vec<Rec> = Vec::new();
    let (mut chg, mut rep) = (false, 0usize);
    {
        let mut v = V3.lock();
        let Some(st) = v.get_mut(&sid) else { return };
        if !v3 { st.changes.clear(); st.replies.clear(); return; }
        if !st.changes.is_empty() && now >= st.next_change {
            chg = true;
            for c in &st.changes { recs.push(c.rec.clone()); }
        }
        let mut due: Vec<&Reply> = st.replies.iter().filter(|r| now >= r.due).collect();
        due.sort_by_key(|r| r.due);
        rep = due.len();
        for r in due { reply_recs(st, r, &mut recs); }
        if !chg && rep == 0 { return; }
    }
    // Merge records of one group and type that two causes both produced.
    let mut seen: Vec<(A4, u8)> = Vec::new();
    recs.retain(|r| { let k = (r.group, r.ty); if seen.contains(&k) { false } else { seen.push(k); true } });
    if !recs.is_empty() && !send(s, dev, ts, &recs) { return; }
    let mut v = V3.lock();
    let Some(st) = v.get_mut(&sid) else { return };
    if chg {
        for c in st.changes.iter_mut() { c.left -= 1; }
        st.changes.retain(|c| c.left > 0);
        st.next_change = now + 100_000_000 + rnd(UNSOLICITED_NS - 100_000_000);
    }
    if rep != 0 { st.replies.retain(|r| now < r.due); }
}

/// ns until the earliest pending report on any interface (0 = due now).
pub(crate) fn next_due(now: u64) -> Option<u64> {
    let v = V3.lock();
    let mut best: Option<u64> = None;
    let mut take = |t: u64| best = Some(best.map_or(t, |b| b.min(t)));
    for st in v.values() {
        if !st.changes.is_empty() { take(st.next_change); }
        for r in &st.replies { take(r.due); }
    }
    best.map(|t| t.saturating_sub(now))
}
