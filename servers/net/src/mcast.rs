//! IPv4 multicast group membership: IP_ADD_MEMBERSHIP / IP_DROP_MEMBERSHIP.
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

/// The groups each description joined: (group, stack).
static GROUPS: Mutex<alloc::collections::BTreeMap<u32, alloc::vec::Vec<([u8; 4], Sid)>>> =
    Mutex::new(alloc::collections::BTreeMap::new());

/// The interface a membership request names (Linux ip_mc_find_dev): by
/// index, else by address, else the one the group routes through.
fn interface_of(group: [u8; 4], addr: [u8; 4], ifindex: i32) -> Result<Sid, i32> {
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
    let g = IpAddress::Ipv4(smoltcp::wire::Ipv4Address(group));
    iface::route_select(0, g).or_else(iface::default_sid).ok_or(ENODEV)
}

/// Make stack `sid`'s interface a member of `group` (or stop being one).
/// The IGMP message goes out now if the device takes it (else the group is
/// recorded and announced at the next query, smoltcp's behaviour).
fn interface_set(sid: Sid, group: [u8; 4], join: bool) -> Result<(), i32> {
    use smoltcp::iface::MulticastError;
    let mut stack = stack_for(sid);
    let s = stack.as_mut().ok_or(-19i32)?;
    let g = smoltcp::wire::Ipv4Address(group);
    let now = stack_now();
    let r = if sid == LO {
        match s.loopback_dev.as_mut() {
            Some(dev) => if join { s.interface.join_multicast_group(dev, g, now) }
                         else { s.interface.leave_multicast_group(dev, g, now) },
            None => return Err(-19),
        }
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

/// IP_ADD_MEMBERSHIP for description `ofd`.
pub(crate) fn join(ofd: u32, group: [u8; 4], addr: [u8; 4], ifindex: i32) -> Result<(), i32> {
    let sid = interface_of(group, addr, ifindex)?;
    let mut g = GROUPS.lock();
    if !g.contains_key(&ofd) { g.retain(|&k, _| vfs::ofd::live(k)); }
    let (has, n) = g.get(&ofd).map_or((false, 0), |m| (m.contains(&(group, sid)), m.len()));
    if has { return Err(-98); } // EADDRINUSE
    if n >= MAX_PER_SOCKET { return Err(-105); } // ENOBUFS
    let first = !g.values().any(|v| v.contains(&(group, sid)));
    if first { interface_set(sid, group, true)?; }
    g.entry(ofd).or_default().push((group, sid));
    Ok(())
}

/// IP_DROP_MEMBERSHIP for description `ofd`: the membership of `group` on
/// the interface named (by index, else address); with neither, the first
/// membership of that group (Linux ip_mc_leave_group).
pub(crate) fn leave(ofd: u32, group: [u8; 4], addr: [u8; 4], ifindex: i32) -> Result<(), i32> {
    let want = if ifindex > 0 || addr != [0; 4] { Some(interface_of(group, addr, ifindex).map_err(|_| -99i32)?) } else { None };
    let mut g = GROUPS.lock();
    let Some(mine) = g.get_mut(&ofd) else { return Err(-99) }; // EADDRNOTAVAIL
    let Some(i) = mine.iter().position(|&(gr, s)| gr == group && want.map_or(true, |w| w == s)) else { return Err(-99) };
    let (_, sid) = mine.remove(i);
    if mine.is_empty() { g.remove(&ofd); }
    if !g.values().any(|v| v.contains(&(group, sid))) { let _ = interface_set(sid, group, false); }
    Ok(())
}

/// The description is gone: leave all its groups.
pub(crate) fn release(ofd: u32) {
    let mut g = GROUPS.lock();
    let Some(mine) = g.remove(&ofd) else { return };
    for (group, sid) in mine {
        if !g.values().any(|v| v.contains(&(group, sid))) { let _ = interface_set(sid, group, false); }
    }
}

/// /proc/net/igmp in Linux's format: per interface its groups (the
/// all-systems group 224.0.0.1 included, as Linux lists it) with how many
/// sockets joined each.
pub(crate) fn proc_igmp(out: &mut alloc::string::String) {
    use core::fmt::Write;
    // Socket counts first: `GROUPS` is never taken under a stack lock.
    let counts: alloc::vec::Vec<([u8; 4], Sid)> = GROUPS.lock().values().flatten().copied().collect();
    let _ = out.write_str("Idx\tDevice    : Count Querier\tGroup    Users Timer\tReporter\n");
    let mut sids: alloc::vec::Vec<(Sid, alloc::string::String)> = alloc::vec![(LO, "lo".into())];
    for n in drivers::netdev::iter() { sids.push((iface::sid_of(n.id), n.name().into())); }
    for (sid, name) in sids {
        let groups: alloc::vec::Vec<[u8; 4]> = {
            let stack = stack_for(sid);
            let Some(s) = stack.as_ref() else { continue };
            s.interface.ipv4_multicast_groups().map(|a| a.0).collect()
        };
        let mut all = alloc::vec![[224u8, 0, 0, 1]];
        for g in groups { if !all.contains(&g) { all.push(g); } }
        let _ = write!(out, "{}\t{:<10}: {:5}      V2\n", sid as u32 + 1, name, all.len());
        for g in all {
            let users = counts.iter().filter(|&&m| m == (g, sid)).count().max(1);
            let _ = write!(out, "\t\t\t\t{:08X} {:5} 0:00000000\t\t0\n", u32::from_le_bytes(g), users);
        }
    }
}
