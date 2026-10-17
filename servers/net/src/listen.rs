//! TCP listening sockets: a listener and its accept queue on each stack.
//!
//! smoltcp has no accept queue: a socket in LISTEN takes the first SYN for
//! its port and *becomes* that connection. With one socket per stack per
//! listener (what this server had until lane netroam), every other SYN for
//! the port got an RST — "Connection refused" — for as long as that one
//! socket was not back in LISTEN: during each handshake, until accept() took
//! a completed connection, and forever when a handshake never completed (a
//! peer that vanished after its SYN: roaming, a lossy link) or completed into
//! a state the listener's poll did not report (CLOSE-WAIT before accept).
//! The real laptop hit the "forever" case after Wi-Fi roaming: ping worked,
//! sshd's 0.0.0.0:22 answered RST, sshd never saw a connection.
//!
//! Now each listener is a [`Listen`] on every stack it listens on, held by
//! that stack (so it is read and changed under the stack lock only):
//!
//! * `arm`: a socket in LISTEN, re-armed by [`maintain`] after every daemon
//!   poll as soon as the previous one took a SYN, so a new SYN always finds
//!   one (up to [`BACKLOG`] connections pending).
//! * `queue`: the sockets that took a SYN, in arrival order: handshakes in
//!   progress and completed connections waiting for accept() (ESTABLISHED or
//!   CLOSE-WAIT; both are "ready"). A handshake that does not complete within
//!   [`SYN_RECV_TIMEOUT_NS`] is aborted (Linux gives up after five SYN-ACK
//!   retries, about a minute); a connection reset before accept() goes.
//! * The arm is kept at a higher socket slot than every queued socket of its
//!   listener: smoltcp hands a segment to the first socket (in slot order)
//!   that accepts it, and a LISTEN socket accepts any SYN, so a retransmitted
//!   SYN of a handshake in progress must reach that handshake's socket (which
//!   ignores it) and not the arm (which would start a second, conflicting
//!   handshake for the same 4-tuple).
//!
//! Which stacks: an INADDR_ANY listener is on every stack (loopback and every
//! NIC), a 127/8 one on loopback only, any other explicit address on every
//! NIC stack with that address as smoltcp's listen filter — so it follows
//! the address to whichever interface holds it, as on Linux. NIC stacks exist
//! from boot (registration is write-once), so a listener needs no update when
//! an interface gains, loses or changes its address; and whatever happens to
//! its sockets (a reset on address loss, a stuck handshake) `maintain` puts a
//! LISTEN socket back.

use super::*;
use smoltcp::wire::IpListenEndpoint;

/// Connections a listener holds before accept() takes them (handshakes in
/// progress included), per stack. A SYN beyond that gets an RST.
pub(crate) const BACKLOG: usize = 16;
/// A handshake (SYN-RECEIVED) older than this is aborted.
pub(crate) const SYN_RECV_TIMEOUT_NS: u64 = 60_000_000_000;

/// One listener on one stack (see the module docs).
pub struct Listen {
    pub lid: u32,
    pub ep: IpListenEndpoint,
    pub arm: Option<SocketHandle>,
    /// (socket, `monotonic_ns` when it left LISTEN), oldest first.
    pub queue: alloc::vec::Vec<(SocketHandle, u64)>,
    /// EPOLLET edge-seq: a new `INET_SEQ_GEN` value each time a queued
    /// connection becomes ready for accept() (0: none ever did). An EPOLLET
    /// acceptor is owed one edge per new connection, also while older ones
    /// wait (Linux wakes the listener for each), and none in between.
    pub seq: u64,
    /// The queued sockets that already produced their edge.
    pub seen: alloc::vec::Vec<SocketHandle>,
}

static NEXT_LID: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);

/// A new listener id (never 0).
pub(crate) fn new_lid() -> u32 {
    loop {
        let l = NEXT_LID.fetch_add(1, Ordering::Relaxed);
        if l != 0 { return l; }
    }
}

/// The stacks a listener on `local` lives on.
pub(crate) fn stacks_for(local: IpEndpoint) -> alloc::vec::Vec<Sid> {
    let mut v = alloc::vec::Vec::new();
    if local.addr.is_unspecified() {
        v.push(LO);
        v.extend(iface::nic_sids());
    } else if is_loopback_addr(local.addr) {
        v.push(LO);
    } else {
        v.extend(iface::nic_sids());
    }
    v
}

/// Every stack, for the paths that look a listener up by id.
pub(crate) fn all_sids() -> impl Iterator<Item = Sid> { core::iter::once(LO).chain(iface::nic_sids()) }

fn listen_ep(local: IpEndpoint) -> IpListenEndpoint {
    IpListenEndpoint { addr: if local.addr.is_unspecified() { None } else { Some(local.addr) }, port: local.port }
}

/// A fresh LISTEN socket for `ep` in `set`.
fn new_arm(set: &mut SocketSet<'static>, ep: IpListenEndpoint) -> Option<SocketHandle> {
    let mut socket = new_tcp_socket(set);
    socket.listen(ep).ok()?;
    Some(set.add(socket))
}

/// Put listener `lid` on stack `sid` and arm it. False when the stack does
/// not exist or the endpoint cannot listen. Takes that stack's lock.
pub(crate) fn create(sid: Sid, lid: u32, local: IpEndpoint) -> bool {
    let mut stack = stack_for(sid);
    let Some(s) = stack.as_mut() else { return false };
    let ep = listen_ep(local);
    let Some(arm) = new_arm(&mut s.socket_set, ep) else { return false };
    s.listens.push(Listen { lid, ep, arm: Some(arm), queue: alloc::vec::Vec::new(), seq: 0, seen: alloc::vec::Vec::new() });
    true
}

/// Take listener `lid` off stack `sid`: its LISTEN socket goes, connections
/// nobody accepted are reset (as Linux does when a listener closes). Takes
/// that stack's lock.
pub(crate) fn destroy(sid: Sid, lid: u32) {
    let mut stack = stack_for(sid);
    let Some(s) = stack.as_mut() else { return };
    let Some(i) = s.listens.iter().position(|l| l.lid == lid) else { return };
    let l = s.listens.swap_remove(i);
    if let Some(a) = l.arm { s.socket_set.remove(a); }
    for (h, _) in l.queue {
        let t = s.socket_set.get_mut::<tcp::Socket>(h);
        if matches!(t.state(), tcp::State::Closed | tcp::State::Listen) {
            s.socket_set.remove(h);
        } else {
            // The RST goes out on the next poll; the orphan reaper frees it.
            t.abort();
            s.orphans.push((h, sched::ticks() + ORPHAN_TICKS));
        }
    }
}

fn ready_state(st: tcp::State) -> bool { matches!(st, tcp::State::Established | tcp::State::CloseWait) }

/// Swap two sockets' contents in `set` (`a != b`): the handles keep their
/// slots, the sockets trade places.
fn swap_sockets(set: &mut SocketSet<'static>, a: SocketHandle, b: SocketHandle) {
    let mut ra = None;
    let mut rb = None;
    for (h, sk) in set.iter_mut() {
        if h == a { ra = Some(sk); } else if h == b { rb = Some(sk); }
    }
    if let (Some(x), Some(y)) = (ra, rb) { core::mem::swap(x, y); }
}

/// Bring one listener up to date after a poll (see the module docs).
fn maintain_one(set: &mut SocketSet<'static>, l: &mut Listen, now: u64) {
    // The arm took a SYN (or was reset or aborted): it joins the queue.
    if let Some(a) = l.arm {
        match set.get::<tcp::Socket>(a).state() {
            tcp::State::Listen => {}
            tcp::State::Closed => { set.remove(a); l.arm = None; }
            _ => { l.queue.push((a, now)); l.arm = None; }
        }
    }
    let mut i = 0;
    while i < l.queue.len() {
        let (h, since) = l.queue[i];
        let st = set.get::<tcp::Socket>(h).state();
        match st {
            // Reset by the peer, aborted by us (stuck handshake, address
            // gone) or timed out: nothing left to accept.
            tcp::State::Closed | tcp::State::TimeWait => {
                set.remove(h);
                l.queue.remove(i);
                continue;
            }
            // An RST during the handshake puts a smoltcp socket back into
            // LISTEN: it is an arm again (or a spare one, dropped).
            tcp::State::Listen => {
                l.queue.remove(i);
                if l.arm.is_none() { l.arm = Some(h); } else { set.remove(h); }
                continue;
            }
            tcp::State::SynReceived if now.saturating_sub(since) >= SYN_RECV_TIMEOUT_NS => {
                set.get_mut::<tcp::Socket>(h).abort();
            }
            _ => {}
        }
        i += 1;
    }
    if l.arm.is_none() && l.queue.len() < BACKLOG {
        l.arm = new_arm(set, l.ep);
    }
    // The arm must come after every queued socket in slot order.
    if let Some(a) = l.arm {
        if let Some(qi) = (0..l.queue.len()).max_by_key(|&k| l.queue[k].0) {
            let q = l.queue[qi].0;
            if q > a {
                swap_sockets(set, a, q);
                l.arm = Some(q);
                l.queue[qi].0 = a;
                // The two sockets traded slots: an edge already counted
                // follows its socket.
                let had_a = l.seen.contains(&a);
                let had_q = l.seen.contains(&q);
                l.seen.retain(|&h| h != a && h != q);
                if had_a { l.seen.push(q); }
                if had_q { l.seen.push(a); }
            }
        }
    }
    // Edges: each queued connection that became ready since the last look.
    // States only change in a daemon poll, and this runs right after each.
    l.seen.retain(|h| l.queue.iter().any(|(q, _)| q == h));
    for &(h, _) in l.queue.iter() {
        if ready_state(set.get::<tcp::Socket>(h).state()) && !l.seen.contains(&h) {
            l.seen.push(h);
            l.seq = INET_SEQ_GEN.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Every listener of a stack, after its poll. Caller holds the stack lock.
pub(crate) fn maintain(s: &mut NetStack, now: u64) {
    if s.listens.is_empty() { return; }
    let NetStack { socket_set, listens, .. } = s;
    for l in listens.iter_mut() { maintain_one(socket_set, l, now); }
}

/// The oldest completed connection of listener `lid` on stack `sid`, taken
/// out of its queue (the caller owns the handle from now on). Takes that
/// stack's lock.
pub(crate) fn accept(sid: Sid, lid: u32) -> Option<SocketHandle> {
    let mut stack = stack_for(sid);
    let s = stack.as_mut()?;
    let NetStack { socket_set, listens, .. } = s;
    let l = listens.iter_mut().find(|l| l.lid == lid)?;
    maintain_one(socket_set, l, sched::monotonic_ns());
    let i = l.queue.iter().position(|(h, _)| ready_state(socket_set.get::<tcp::Socket>(*h).state()))?;
    let (h, _) = l.queue.remove(i);
    // A slot freed in the queue: re-arm if the backlog was full.
    maintain_one(socket_set, l, sched::monotonic_ns());
    Some(h)
}

/// Whether a completed connection is waiting on stack `sid`, and the
/// listener's edge-seq there. Takes that stack's lock.
pub(crate) fn ready_seq(sid: Sid, lid: u32) -> (bool, u64) {
    let mut stack = stack_for(sid);
    let Some(s) = stack.as_mut() else { return (false, 0) };
    let Some(l) = s.listens.iter().find(|l| l.lid == lid) else { return (false, 0) };
    let set = &s.socket_set;
    let ready = l.queue.iter().any(|(h, _)| ready_state(set.get::<tcp::Socket>(*h).state()))
        || l.arm.map_or(false, |a| ready_state(set.get::<tcp::Socket>(a).state()));
    (ready, l.seq)
}

/// (handshakes in progress, connections waiting for accept) of listener
/// `lid` across every stack, for /proc and the socket dump.
pub(crate) fn backlog(lid: u32) -> (usize, usize) {
    let (mut syn, mut est) = (0, 0);
    for sid in all_sids() {
        let stack = stack_for(sid);
        let Some(s) = stack.as_ref() else { continue };
        let Some(l) = s.listens.iter().find(|l| l.lid == lid) else { continue };
        for (h, _) in l.queue.iter() {
            match s.socket_set.get::<tcp::Socket>(*h).state() {
                tcp::State::SynReceived => syn += 1,
                st if ready_state(st) => est += 1,
                _ => {}
            }
        }
    }
    (syn, est)
}
