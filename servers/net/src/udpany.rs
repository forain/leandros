//! UDP servers bound to INADDR_ANY: one smoltcp socket on every stack.
//!
//! A UDP socket lives on one stack (see `udp_materialize`), which for a
//! socket bound to 0.0.0.0:PORT was the default route's: it received only
//! what arrived on that interface (never on loopback, never on a second NIC
//! or wifi0), and on a machine whose default route moves (roaming, the cable
//! pulled) it stayed deaf on the interface that now carries the traffic.
//!
//! A socket bound to INADDR_ANY with an explicit port (a server: Linux's
//! wildcard bind) therefore gets a *mirror* on every other stack, bound to
//! the same port. The table entry keeps naming one socket (the primary,
//! where it was created); this table, keyed by the socket's open file
//! description, names the rest. Receive and poll look at all of them; a send
//! leaves from the stack the destination routes through (so the source
//! address is that interface's, as Linux picks it), falling back to the
//! primary. Sockets bound to port 0, connected sockets and sockets bound to
//! an address keep one stack (clients: replies come back where they left), and
//! a wildcard server that connect()s collapses to the socket on its peer's
//! stack (`collapse`).
//!
//! Lock order: SOCK_TABLES, then this leaf map, then one stack at a time —
//! the map is never held while a stack is locked.

use super::*;

/// Mirrors: smaller receive buffers than a primary's, but room for the
/// largest datagram both ways (a send leaves through the mirror on the
/// destination's interface; with 32 KiB a 64 KiB datagram never fit).
fn new_mirror_socket() -> udp::Socket<'static> {
    let rx = udp::PacketBuffer::new(alloc::vec![udp::PacketMetadata::EMPTY; 64], alloc_net_buf(128 * 1024, 16 * 1024));
    let tx = udp::PacketBuffer::new(alloc::vec![udp::PacketMetadata::EMPTY; 32], alloc_net_buf(128 * 1024, 8 * 1024));
    udp::Socket::new(rx, tx)
}

/// Per description: the socket on each stack, primary included.
static MIRRORS: Mutex<alloc::collections::BTreeMap<u32, [Option<SocketHandle>; MAX_STACKS]>> =
    Mutex::new(alloc::collections::BTreeMap::new());

/// Port of each wildcard server in MIRRORS, for a stack built after it
/// (`adopt`). Leaf lock, taken after MIRRORS when both are needed.
/// (port, family): an AF_INET server's mirrors never hear v6 datagrams, nor a V6ONLY
/// AF_INET6 server's v4 ones.
static PORTS: Mutex<crate::latestack::Registry<(u16, Option<smoltcp::wire::IpVersion>)>> = Mutex::new(crate::latestack::Registry::new());

/// Stack `sid` was just built (wifi0, once its MAC is known): give every
/// wildcard server a mirror there. Returns how many it carries. Caller
/// holds no net lock.
pub(crate) fn adopt(sid: Sid) -> usize {
    let snap = PORTS.lock().snapshot();
    // The commit below checks, under MIRRORS, that the server still is one;
    // from then on `release`/`collapse` own the mirror like any other.
    crate::latestack::adopt(&snap, |_| true,
        |ofd, pf| {
            let (port, fam) = *pf;
            let h = {
                let mut stack = stack_for(sid);
                let Some(s) = stack.as_mut() else { return false };
                let mut socket = new_mirror_socket();
                if socket.bind(port).is_err() { return false; }
                if fam.is_some() { socket.set_family(fam); }
                socket.set_reuseport(sock_opts(ofd).reuseport);
                socket.set_owner(ofd);
                s.socket_set.add(socket)
            };
            // Commit only into a server that is still a wildcard one and
            // has no socket there yet; otherwise the new one goes again.
            let kept = match MIRRORS.lock().get_mut(&ofd) {
                Some(hs) if hs[sid as usize].is_none() => { hs[sid as usize] = Some(h); true }
                _ => false,
            };
            if !kept {
                let mut stack = stack_for(sid);
                if let Some(s) = stack.as_mut() { release_inet_socket(s, h); }
            }
            kept
        },
        |_| true, |_| {})
}

/// The per-stack sockets of description `ofd` (`None` = not a wildcard
/// server: one stack only).
pub(crate) fn of(ofd: u32) -> Option<[Option<SocketHandle>; MAX_STACKS]> {
    if ofd == 0 { return None; }
    let mut hs = MIRRORS.lock().get(&ofd).copied()?;
    // A mirror on a stack that was freed (DELLINK / netns reap) names a socket
    // that is gone, or worse a socket of a new stack at the same id: callers
    // `get_mut` these handles, which panics on a stale one. Map lock released.
    for (sid, h) in hs.iter_mut().enumerate() {
        let Some(x) = *h else { continue };
        let live = stack_for(sid as Sid).as_ref().map_or(false, |s| s.socket_set.contains(x));
        if !live { *h = None; }
    }
    Some(hs)
}

/// Give a just-materialized wildcard server socket (`primary` on `psid`,
/// port `port`) a socket on every other stack. Caller holds SOCK_TABLES.
pub(crate) fn create(ns: u16, ofd: u32, psid: Sid, primary: SocketHandle, port: u16, fam: Option<smoltcp::wire::IpVersion>) {
    if ofd == 0 { return; }
    let mut hs = [None; MAX_STACKS];
    hs[psid as usize] = Some(primary);
    for sid in iface::ns_sids(ns) {
        if sid == psid { continue; }
        let mut stack = stack_for(sid);
        let Some(s) = stack.as_mut() else { continue };
        let mut socket = new_mirror_socket();
        if socket.bind(port).is_err() { continue; }
        if fam.is_some() { socket.set_family(fam); }
        // SO_REUSEPORT: the group's unicast spreading (smoltcp's demux).
        socket.set_reuseport(sock_opts(ofd).reuseport);
        socket.set_owner(ofd);
        hs[sid as usize] = Some(s.socket_set.add(socket));
    }
    MIRRORS.lock().insert(ofd, hs);
    // Only netns 0's servers follow a late NIC stack (`adopt`).
    if ns == 0 { PORTS.lock().register(ofd, (port, fam)); }
}

/// The socket is gone (its last holder let go): remove the mirrors (the
/// primary is released by the caller). No lock held.
pub(crate) fn release(ofd: u32, primary_sid: Option<Sid>) {
    if ofd == 0 { return; }
    PORTS.lock().unregister(ofd);
    let Some(hs) = MIRRORS.lock().remove(&ofd) else { return };
    for (sid, h) in hs.iter().enumerate() {
        let Some(h) = *h else { continue };
        if Some(sid as Sid) == primary_sid { continue; }
        let mut stack = stack_for(sid as Sid);
        if let Some(s) = stack.as_mut() { release_inet_socket(s, h); }
    }
}

/// A wildcard server that connect()s: from then on it talks to one peer,
/// which reaches it through stack `keep` (the route's), so every socket but
/// the one on `keep` goes and the description stops being a wildcard
/// server. `None`: it was not one. `Some(h)`: its socket on `keep`, `None`
/// when it had none there (then all of them are gone). Caller holds
/// SOCK_TABLES; the map is released before any stack is locked.
pub(crate) fn collapse(ofd: u32, keep: Sid) -> Option<Option<SocketHandle>> {
    if ofd == 0 { return None; }
    let hs = MIRRORS.lock().remove(&ofd)?;
    PORTS.lock().unregister(ofd);
    let mut kept = None;
    for (sid, h) in hs.iter().enumerate() {
        let Some(h) = *h else { continue };
        if sid as Sid == keep { kept = Some(h); continue; }
        let mut stack = stack_for(sid as Sid);
        if let Some(s) = stack.as_mut() { release_inet_socket(s, h); }
    }
    Some(kept)
}

/// The stack and socket a datagram to `dest` leaves from.
pub(crate) fn send_via(ns: u16, hs: &[Option<SocketHandle>; MAX_STACKS], primary: (Sid, SocketHandle), dest: IpAddress)
    -> (Sid, SocketHandle)
{
    match iface::route_select(ns, dest) {
        Some(sid) => match hs[sid as usize] {
            Some(h) => (sid, h),
            None => primary,
        },
        None => primary,
    }
}

/// Every (stack, socket) of a wildcard server, primary first.
pub(crate) fn order(hs: &[Option<SocketHandle>; MAX_STACKS], primary: (Sid, SocketHandle))
    -> alloc::vec::Vec<(Sid, SocketHandle)>
{
    let mut v = alloc::vec![primary];
    for (sid, h) in hs.iter().enumerate() {
        if let Some(h) = *h {
            if sid as Sid != primary.0 { v.push((sid as Sid, h)); }
        }
    }
    v
}
