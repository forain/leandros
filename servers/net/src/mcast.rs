//! Multicast group membership: IP_ADD_MEMBERSHIP / IP_DROP_MEMBERSHIP, the
//! source-filter options, IP_MSFILTER, MCAST_* (IPv4 and IPv6 groups) and
//! IP_MULTICAST_ALL. Groups are 16-byte keys; IPv4 is ::ffff:a.b.c.d.
//!
//! Membership is per socket (open file description) and per interface, as
//! on Linux; the interface itself (smoltcp's `join_multicast_group`, built
//! with `proto-igmp`) is a member while at least one socket is, and sends the
//! IGMP report when it joins and the leave when the last socket leaves. Who
//! receives a group's datagrams is smoltcp's demultiplexing: every socket
//! bound to the port on an interface that is a member (Linux's
//! IP_MULTICAST_ALL default), see the patched `process_udp`.
//!
//! Lock order: `GROUPS`, then one stack lock (the interface join or leave
//! happens under the map, so two sockets joining and leaving the same group
//! cannot interleave). Nothing takes `GROUPS` under a stack lock.

use super::*;

/// Linux net.ipv4.igmp_max_memberships: ENOBUFS past it.
const MAX_PER_SOCKET: usize = 20;

/// A group or source address: IPv4 as ::ffff:a.b.c.d, IPv6 as is.
pub(crate) type G = [u8; 16];

pub(crate) fn m4(a: [u8; 4]) -> G {
    let mut b = [0u8; 16];
    b[10] = 0xff; b[11] = 0xff; b[12..].copy_from_slice(&a);
    b
}

fn is_v4(g: &G) -> bool { g[..10] == [0u8; 10] && g[10] == 0xff && g[11] == 0xff }

pub(crate) fn is_mcast(g: &G) -> bool { if is_v4(g) { g[12] & 0xf0 == 0xe0 } else { g[0] == 0xff } }

fn ipaddr(g: &G) -> IpAddress {
    if is_v4(g) { IpAddress::Ipv4(smoltcp::wire::Ipv4Address([g[12], g[13], g[14], g[15]])) }
    else { IpAddress::Ipv6(smoltcp::wire::Ipv6Address(*g)) }
}

/// The groups each description joined: (group, stack).
static GROUPS: Mutex<alloc::collections::BTreeMap<u32, alloc::vec::Vec<(G, Sid)>>> =
    Mutex::new(alloc::collections::BTreeMap::new());

/// Leaf locks for the receive hook: the groups each description is a member
/// of (any interface), and the descriptions with IP_MULTICAST_ALL cleared
/// (they hear only the groups they joined themselves).
static MEMB: Mutex<alloc::collections::BTreeMap<u32, alloc::vec::Vec<G>>> =
    Mutex::new(alloc::collections::BTreeMap::new());
static NOALL: Mutex<alloc::collections::BTreeSet<u32>> = Mutex::new(alloc::collections::BTreeSet::new());

/// Stack `sid` was removed: the memberships on it go.
pub(crate) fn forget_sid(sid: Sid) {
    let mut g = GROUPS.lock();
    for v in g.values_mut() { v.retain(|e| e.1 != sid); }
    g.retain(|_, v| !v.is_empty());
}

pub(crate) fn set_all(ofd: u32, on: bool) {
    let mut n = NOALL.lock();
    if on { n.remove(&ofd); } else { n.insert(ofd); }
}

pub(crate) fn all(ofd: u32) -> bool { !NOALL.lock().contains(&ofd) }

fn memb_add(ofd: u32, group: G) { MEMB.lock().entry(ofd).or_default().push(group); }

fn memb_del(ofd: u32, group: G) {
    let mut m = MEMB.lock();
    if let Some(v) = m.get_mut(&ofd) {
        if let Some(i) = v.iter().position(|&x| x == group) { v.remove(i); }
        if v.is_empty() { m.remove(&ofd); }
    }
}

/// The interface a membership request names (Linux ip_mc_find_dev): by
/// index, else by address, else the one the group routes through.
fn interface_of(group: G, addr: [u8; 4], ifindex: i32) -> Result<Sid, i32> {
    const ENODEV: i32 = -19;
    if ifindex > 0 {
        let sid = (ifindex - 1) as usize;
        if sid >= MAX_STACKS || stack_for(sid as Sid).is_none() { return Err(ENODEV); }
        return Ok(sid as Sid);
    }
    if addr != [0; 4] {
        let a = IpAddress::Ipv4(smoltcp::wire::Ipv4Address(addr));
        if iface::is_loopback(a) { return Ok(LO); }
        return iface::holder_of(a).ok_or(ENODEV);
    }
    let g = ipaddr(&group);
    iface::route_select(0, g).or_else(iface::default_sid).ok_or(ENODEV)
}

/// Make stack `sid`'s interface a member of `group` (or stop being one).
/// The IGMP message goes out now if the device takes it (else the group is
/// recorded and announced at the next query, smoltcp's behaviour).
fn interface_set(sid: Sid, group: G, join: bool) -> Result<(), i32> {
    use smoltcp::iface::MulticastError;
    let mut stack = stack_for(sid);
    let s = stack.as_mut().ok_or(-19i32)?;
    let g = ipaddr(&group);
    let now = stack_now();
    let r = if sid == LO {
        match s.loopback_dev.as_mut() {
            Some(dev) => if join { s.interface.join_multicast_group(dev, g, now) }
                         else { s.interface.leave_multicast_group(dev, g, now) },
            None => return Err(-19),
        }
    } else if sid >= iface::FIRST_DYN {
        // A tap/tun/veth stack: the IGMPv3 reporter sends through the stack's
        // own device; a v1/v2-compat report from here has no wire to take.
        let mut dev = framedev::FrameDev { rx: Default::default(), tx: alloc::vec::Vec::new(), mtu: 1500 };
        if join { s.interface.join_multicast_group(&mut dev, g, now) }
        else { s.interface.leave_multicast_group(&mut dev, g, now) }
    } else {
        let Some(nif) = drivers::netdev::iter().find(|n| iface::sid_of(n.id) == sid) else { return Err(-19) };
        let mut dev = NicDevice::new(nif);
        if join { s.interface.join_multicast_group(&mut dev, g, now) }
        else { s.interface.leave_multicast_group(&mut dev, g, now) }
    };
    drop(stack);
    drivers::netdev::kick();
    match r {
        Ok(_) | Err(MulticastError::Exhausted) => Ok(()),
        Err(MulticastError::GroupTableFull) => Err(-105), // ENOBUFS
        Err(_) => Err(-22),
    }
}

/// One socket's source filter for one group on one interface (RFC 3376
/// state): `include` says the list is the only sources heard (IP_ADD_SOURCE_
/// MEMBERSHIP), else the sources listed are blocked (IP_BLOCK_SOURCE). A
/// membership without an entry is the any-source join (exclude, empty).
struct Filter { group: G, sid: Sid, include: bool, srcs: alloc::vec::Vec<G> }

/// Leaf lock (taken under a stack lock by the rx filter, and under `GROUPS`
/// here): nothing is acquired while holding it.
static FILTERS: Mutex<alloc::collections::BTreeMap<u32, alloc::vec::Vec<Filter>>> =
    Mutex::new(alloc::collections::BTreeMap::new());

/// Linux net.ipv4.igmp_max_msf.
const MAX_SOURCES: usize = 10;

fn drop_filter(ofd: u32, group: G, sid: Sid) {
    let mut f = FILTERS.lock();
    if let Some(v) = f.get_mut(&ofd) {
        v.retain(|x| !(x.group == group && x.sid == sid));
        if v.is_empty() { f.remove(&ofd); }
    }
}

pub(crate) enum SrcOp { Add, Drop, Block, Unblock }

/// The four source options (Linux ip_mc_source): the sources `ofd` hears
/// from `group`. `Add` on a group not yet joined joins it in include mode;
/// the others need a prior membership (EINVAL). Include and exclude
/// (blocked) lists cannot be mixed while non-empty (EINVAL).
pub(crate) fn source(ofd: u32, op: SrcOp, group: G, addr: [u8; 4], ifindex: i32, src: G) -> Result<(), i32> {
    const EINVAL: i32 = -22; const EADDRINUSE: i32 = -98; const EADDRNOTAVAIL: i32 = -99; const ENOBUFS: i32 = -105;
    if is_mcast(&src) { return Err(EINVAL); }
    let sid = interface_of(group, addr, ifindex)?;
    let mut g = GROUPS.lock();
    let member = g.get(&ofd).map_or(false, |m| m.contains(&(group, sid)));
    if !member {
        if !matches!(op, SrcOp::Add) { return Err(EINVAL); }
        join_locked(&mut g, ofd, group, sid)?;
        FILTERS.lock().entry(ofd).or_default().push(Filter { group, sid, include: true, srcs: alloc::vec![src] });
        note(&g, sid, group);
        return Ok(());
    }
    let mut fl = FILTERS.lock();
    let list = fl.entry(ofd).or_default();
    let pos = list.iter().position(|x| x.group == group && x.sid == sid);
    // Block/unblock work on the exclude list, add/drop on the include list.
    let want_include = matches!(op, SrcOp::Add | SrcOp::Drop);
    let mut leave = false;
    match pos {
        None => match op {
            // Any-source membership: an empty exclude list.
            SrcOp::Add => list.push(Filter { group, sid, include: true, srcs: alloc::vec![src] }),
            SrcOp::Block => list.push(Filter { group, sid, include: false, srcs: alloc::vec![src] }),
            SrcOp::Drop | SrcOp::Unblock => return Err(EADDRNOTAVAIL),
        },
        Some(i) => {
            let f = &mut list[i];
            if f.include != want_include { return Err(EINVAL); } // sources of the other kind are set
            let at = f.srcs.iter().position(|&x| x == src);
            match op {
                SrcOp::Add | SrcOp::Block => {
                    if at.is_some() { return Err(EADDRINUSE); }
                    if f.srcs.len() >= MAX_SOURCES { return Err(ENOBUFS); }
                    f.srcs.push(src);
                }
                SrcOp::Drop | SrcOp::Unblock => {
                    let Some(at) = at else { return Err(EADDRNOTAVAIL) };
                    f.srcs.remove(at);
                    if f.srcs.is_empty() {
                        list.remove(i);
                        // The last included source gone is a leave; the last
                        // unblock is back to the plain any-source join.
                        leave = matches!(op, SrcOp::Drop);
                    }
                }
            }
        }
    }
    if list.is_empty() { fl.remove(&ofd); }
    drop(fl);
    if leave {
        if let Some(mine) = g.get_mut(&ofd) {
            mine.retain(|&m| m != (group, sid));
            if mine.is_empty() { g.remove(&ofd); }
        }
        memb_del(ofd, group);
        if !g.values().any(|v| v.contains(&(group, sid))) { let _ = interface_set(sid, group, false); }
    }
    note(&g, sid, group);
    Ok(())
}

/// Receive filter (called by smoltcp's UDP demultiplexing under the stack
/// lock): does the socket `owner` hear `src` sending to `group`?
pub(crate) fn src_accepts(owner: u32, group: G, src: G) -> bool {
    if NOALL.lock().contains(&owner) && !MEMB.lock().get(&owner).map_or(false, |v| v.contains(&group)) {
        return false;
    }
    let fl = FILTERS.lock();
    let Some(v) = fl.get(&owner) else { return true };
    match v.iter().find(|x| x.group == group) {
        None => true,
        Some(f) => f.include == f.srcs.contains(&src),
    }
}

/// IP_MSFILTER set (RFC 3678 setipv4sourcefilter): replace the source list
/// of an existing membership. Include with no sources hears nothing;
/// exclude with none is the plain any-source membership.
pub(crate) fn msfilter_set(ofd: u32, group: G, addr: [u8; 4], ifindex: i32, include: bool, srcs: &[G]) -> Result<(), i32> {
    const EINVAL: i32 = -22; const EADDRNOTAVAIL: i32 = -99; const ENOBUFS: i32 = -105;
    let sid = interface_of(group, addr, ifindex).map_err(|_| EADDRNOTAVAIL)?;
    let g = GROUPS.lock();
    if !g.get(&ofd).map_or(false, |m| m.contains(&(group, sid))) { return Err(EADDRNOTAVAIL); }
    if srcs.iter().any(is_mcast) { return Err(EINVAL); }
    let mut list: alloc::vec::Vec<G> = alloc::vec::Vec::new();
    for s in srcs { if !list.contains(s) { list.push(*s); } }
    if list.len() > MAX_SOURCES { return Err(ENOBUFS); }
    let mut fl = FILTERS.lock();
    let v = fl.entry(ofd).or_default();
    v.retain(|x| !(x.group == group && x.sid == sid));
    if include || !list.is_empty() { v.push(Filter { group, sid, include, srcs: list }); }
    if v.is_empty() { fl.remove(&ofd); }
    drop(fl);
    note(&g, sid, group);
    Ok(())
}

/// IP_MSFILTER get: (include, sources) of a membership.
pub(crate) fn msfilter_get(ofd: u32, group: G, addr: [u8; 4], ifindex: i32) -> Result<(bool, alloc::vec::Vec<G>), i32> {
    const EADDRNOTAVAIL: i32 = -99;
    let sid = interface_of(group, addr, ifindex).map_err(|_| EADDRNOTAVAIL)?;
    let g = GROUPS.lock();
    if !g.get(&ofd).map_or(false, |m| m.contains(&(group, sid))) { return Err(EADDRNOTAVAIL); }
    let fl = FILTERS.lock();
    Ok(fl.get(&ofd).and_then(|v| v.iter().find(|x| x.group == group && x.sid == sid))
        .map_or((false, alloc::vec::Vec::new()), |f| (f.include, f.srcs.clone())))
}

/// Install the receive filter hook and the IGMPv3 query hook.
pub(crate) fn init() {
    use core::sync::atomic::Ordering::Relaxed;
    let f: fn(u32, G, G) -> bool = src_accepts;
    smoltcp::socket::udp::MCAST_SRC_FILTER.store(f as usize, Relaxed);
    let q: fn([u8; 4], &[u8]) -> bool = igmp3::query_hook;
    smoltcp::iface::IGMP_QUERY_HOOK.store(q as usize, Relaxed);
    let m: fn([u8; 16], [u8; 16], u8, &[u8]) -> bool = mld2::query_hook;
    smoltcp::iface::MLD_QUERY_HOOK.store(m as usize, Relaxed);
}

/// The interface's state for `group` (RFC 3376 3.2): the union of every
/// member socket's filter. All include: INCLUDE of the union of the lists;
/// any exclude: EXCLUDE of the intersection of the exclude lists minus the
/// union of the include lists. None: no socket is a member.
fn iface_state_g(g: &Groups, sid: Sid, group: G) -> Option<igmp3::State<G>> {
    let fl = FILTERS.lock();
    let (mut any, mut inc, mut exc): (bool, alloc::vec::Vec<G>, Option<alloc::vec::Vec<G>>) = (false, alloc::vec::Vec::new(), None);
    for (ofd, mine) in g.iter() {
        if !mine.contains(&(group, sid)) { continue; }
        any = true;
        let f = fl.get(ofd).and_then(|v| v.iter().find(|x| x.group == group && x.sid == sid));
        let (include, srcs) = f.map_or((false, &[][..]), |f| (f.include, &f.srcs[..]));
        if include { for s in srcs { if !inc.contains(s) { inc.push(*s); } } }
        else {
            exc = Some(match exc { None => srcs.to_vec(), Some(p) => p.into_iter().filter(|x| srcs.contains(x)).collect() });
        }
    }
    if !any { return None; }
    Some(match exc {
        Some(x) => (false, x.into_iter().filter(|s| !inc.contains(s)).collect()),
        None => (true, inc),
    })
}

/// Tell the IGMPv3 / MLDv2 reporter that `group`'s state on `sid` may have
/// changed (`GROUPS` held by the caller).
fn note(g: &Groups, sid: Sid, group: G) {
    let st = iface_state_g(g, sid, group);
    if !is_v4(&group) { mld2::changed(sid, group, st); return; }
    let a4 = |s: &G| [s[12], s[13], s[14], s[15]];
    igmp3::changed(sid, [group[12], group[13], group[14], group[15]], st.map(|(i, v)| (i, v.iter().map(a4).collect())));
}

/// IP_ADD_MEMBERSHIP for description `ofd`.
pub(crate) fn join(ofd: u32, group: G, addr: [u8; 4], ifindex: i32) -> Result<(), i32> {
    let sid = interface_of(group, addr, ifindex)?;
    let mut g = GROUPS.lock();
    join_locked(&mut g, ofd, group, sid)?;
    note(&g, sid, group);
    Ok(())
}

type Groups = alloc::collections::BTreeMap<u32, alloc::vec::Vec<(G, Sid)>>;

fn join_locked(g: &mut Groups, ofd: u32, group: G, sid: Sid) -> Result<(), i32> {
    if !g.contains_key(&ofd) { g.retain(|&k, _| vfs::ofd::live(k)); }
    let (has, n) = g.get(&ofd).map_or((false, 0), |m| (m.contains(&(group, sid)), m.len()));
    if has { return Err(-98); } // EADDRINUSE
    if n >= MAX_PER_SOCKET { return Err(-105); } // ENOBUFS
    let first = !g.values().any(|v| v.contains(&(group, sid)));
    if first { interface_set(sid, group, true)?; }
    g.entry(ofd).or_default().push((group, sid));
    memb_add(ofd, group);
    Ok(())
}

/// IP_DROP_MEMBERSHIP for description `ofd`: the membership of `group` on
/// the interface named (by index, else address); with neither, the first
/// membership of that group (Linux ip_mc_leave_group).
pub(crate) fn leave(ofd: u32, group: G, addr: [u8; 4], ifindex: i32) -> Result<(), i32> {
    let want = if ifindex > 0 || addr != [0; 4] { Some(interface_of(group, addr, ifindex).map_err(|_| -99i32)?) } else { None };
    let mut g = GROUPS.lock();
    let Some(mine) = g.get_mut(&ofd) else { return Err(-99) }; // EADDRNOTAVAIL
    let Some(i) = mine.iter().position(|&(gr, s)| gr == group && want.map_or(true, |w| w == s)) else { return Err(-99) };
    let (_, sid) = mine.remove(i);
    if mine.is_empty() { g.remove(&ofd); }
    drop_filter(ofd, group, sid);
    memb_del(ofd, group);
    if !g.values().any(|v| v.contains(&(group, sid))) { let _ = interface_set(sid, group, false); }
    note(&g, sid, group);
    Ok(())
}

/// The description is gone: leave all its groups.
pub(crate) fn release(ofd: u32) {
    let mut g = GROUPS.lock();
    FILTERS.lock().remove(&ofd);
    MEMB.lock().remove(&ofd);
    NOALL.lock().remove(&ofd);
    let Some(mine) = g.remove(&ofd) else { return };
    for (group, sid) in mine {
        if !g.values().any(|v| v.contains(&(group, sid))) { let _ = interface_set(sid, group, false); }
        note(&g, sid, group);
    }
}

/// /proc/net/igmp in Linux's format: per interface its groups (the
/// all-systems group 224.0.0.1 included, as Linux lists it) with how many
/// sockets joined each.
pub(crate) fn proc_igmp(out: &mut alloc::string::String) {
    use core::fmt::Write;
    // Socket counts first: `GROUPS` is never taken under a stack lock.
    let counts: alloc::vec::Vec<(G, Sid)> = GROUPS.lock().values().flatten().copied().collect();
    let _ = out.write_str("Idx\tDevice    : Count Querier\tGroup    Users Timer\tReporter\n");
    let mut sids: alloc::vec::Vec<(Sid, alloc::string::String)> = alloc::vec![(LO, "lo".into())];
    for n in drivers::netdev::iter() { sids.push((iface::sid_of(n.id), n.name().into())); }
    for (sid, name) in sids {
        let mut ver = 2u8;
        let groups: alloc::vec::Vec<[u8; 4]> = {
            let mut stack = stack_for(sid);
            let Some(s) = stack.as_mut() else { continue };
            ver = s.interface.igmp_version(stack_now());
            s.interface.ipv4_multicast_groups().map(|a| a.0).collect()
        };
        let mut all = alloc::vec![[224u8, 0, 0, 1]];
        for g in groups { if !all.contains(&g) { all.push(g); } }
        let _ = write!(out, "{}\t{:<10}: {:5}      V{}\n", sid as u32 + 1, name, all.len(), ver);
        for g in all {
            let users = counts.iter().filter(|&&m| m == (m4(g), sid)).count().max(1);
            let _ = write!(out, "\t\t\t\t{:08X} {:5} 0:00000000\t\t0\n", u32::from_le_bytes(g), users);
        }
    }
}

/// /proc/net/igmp6 (Linux igmp6_mc_seq_show): index, device, group, users,
/// flags, timer. Every interface is a member of ff01::1 and ff02::1; every
/// interface but lo also of the solicited-node group of each of its
/// addresses. Flags: 4 (no report) for the groups MLD never reports, 2
/// (loaded) for the others.
pub(crate) fn proc_igmp6(out: &mut alloc::string::String) {
    use core::fmt::Write;
    let counts: alloc::vec::Vec<(G, Sid)> = GROUPS.lock().values().flatten().copied().collect();
    let mut sids: alloc::vec::Vec<(Sid, alloc::string::String)> = alloc::vec![(LO, "lo".into())];
    for n in drivers::netdev::iter() { sids.push((iface::sid_of(n.id), n.name().into())); }
    for (sid, name) in sids {
        let (groups, sol): (alloc::vec::Vec<[u8; 16]>, alloc::vec::Vec<[u8; 16]>) = {
            let stack = stack_for(sid);
            let Some(s) = stack.as_ref() else { continue };
            (s.interface.ipv6_multicast_groups().map(|a| a.0).collect(), mld2::solicited(s))
        };
        let mut all: alloc::vec::Vec<[u8; 16]> = alloc::vec::Vec::new();
        for lead in [1u8, 2] { let mut g = [0u8; 16]; g[0] = 0xff; g[1] = lead; g[15] = 1; all.push(g); }
        for g in sol.into_iter().chain(groups) { if !all.contains(&g) { all.push(g); } }
        for g in all {
            let users = counts.iter().filter(|&&m| m == (g, sid)).count().max(1);
            let _ = write!(out, "{:<4} {:<15} ", sid as u32 + 1, name);
            for b in g { let _ = write!(out, "{:02x}", b); }
            let flags = if sid != LO && mld2::reportable(&g) { 0x02 } else { 0x04 };
            let _ = write!(out, " {:5} {:08X} 0\n", users, flags);
        }
    }
}
