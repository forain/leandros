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
//! an address keep one stack (clients: replies come back where they left).
//!
//! Lock order: SOCK_TABLES, then this leaf map, then one stack at a time —
//! the map is never held while a stack is locked.

use super::*;

/// Mirrors are servers' sockets: smaller buffers than a primary's.
fn new_mirror_socket() -> udp::Socket<'static> {
    let rx = udp::PacketBuffer::new(alloc::vec![udp::PacketMetadata::EMPTY; 64], alloc_net_buf(64 * 1024, 16 * 1024));
    let tx = udp::PacketBuffer::new(alloc::vec![udp::PacketMetadata::EMPTY; 32], alloc_net_buf(32 * 1024, 8 * 1024));
    udp::Socket::new(rx, tx)
}

/// Per description: the socket on each stack, primary included.
static MIRRORS: Mutex<alloc::collections::BTreeMap<u32, [Option<SocketHandle>; MAX_STACKS]>> =
    Mutex::new(alloc::collections::BTreeMap::new());

/// The per-stack sockets of description `ofd` (`None` = not a wildcard
/// server: one stack only).
pub(crate) fn of(ofd: u32) -> Option<[Option<SocketHandle>; MAX_STACKS]> {
    if ofd == 0 { return None; }
    MIRRORS.lock().get(&ofd).copied()
}

/// Give a just-materialized wildcard server socket (`primary` on `psid`,
/// port `port`) a socket on every other stack. Caller holds SOCK_TABLES.
pub(crate) fn create(ofd: u32, psid: Sid, primary: SocketHandle, port: u16) {
    if ofd == 0 { return; }
    let mut hs = [None; MAX_STACKS];
    hs[psid as usize] = Some(primary);
    for sid in core::iter::once(LO).chain(iface::nic_sids()) {
        if sid == psid { continue; }
        let mut stack = stack_for(sid);
        let Some(s) = stack.as_mut() else { continue };
        let mut socket = new_mirror_socket();
        if socket.bind(port).is_err() { continue; }
        hs[sid as usize] = Some(s.socket_set.add(socket));
    }
    MIRRORS.lock().insert(ofd, hs);
}

/// The socket is gone (its last holder let go): remove the mirrors (the
/// primary is released by the caller). No lock held.
pub(crate) fn release(ofd: u32, primary_sid: Option<Sid>) {
    if ofd == 0 { return; }
    let Some(hs) = MIRRORS.lock().remove(&ofd) else { return };
    for (sid, h) in hs.iter().enumerate() {
        let Some(h) = *h else { continue };
        if Some(sid as Sid) == primary_sid { continue; }
        let mut stack = stack_for(sid as Sid);
        if let Some(s) = stack.as_mut() { s.socket_set.remove(h); }
    }
}

/// The stack and socket a datagram to `dest` leaves from.
pub(crate) fn send_via(hs: &[Option<SocketHandle>; MAX_STACKS], primary: (Sid, SocketHandle), dest: IpAddress)
    -> (Sid, SocketHandle)
{
    match iface::route_select(dest) {
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
