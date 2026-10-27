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
//! from boot (registration is write-once) except Wi-Fi's, built once its MAC
//! is known, which adopts every registered listener (`adopt`, `latestack`);
//! so a listener needs no update when
//! an interface gains, loses or changes its address; and whatever happens to
//! its sockets (a reset on address loss, a stuck handshake) `maintain` puts a
//! LISTEN socket back.

use super::*;
use smoltcp::wire::IpListenEndpoint;
use core::fmt::Write as _;

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

/// The exact poll key of listener `lid`, the same on every stack it lives
/// on. One helper for the producer (`maintain`, `accept`, `destroy_all`) and
/// the poll answer (`handle_poll`'s `InetListening` arm), so the tag a poller
/// parks with and the key the daemon wakes cannot drift apart. Without it the
/// listener answered no tag, and every poll/epoll set holding one (sshd's
/// tokio reactor) parked broadcast: ~2,800 wakes/s on the laptop.
pub(crate) fn wake_key(lid: u32) -> u64 { sched::poll_key(sched::poll_class::LISTEN, lid) }

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
    // Idempotent: listen() and a late stack's `adopt` may both get here.
    if s.listens.iter().any(|l| l.lid == lid) { return true; }
    let ep = listen_ep(local);
    let Some(arm) = new_arm(&mut s.socket_set, ep) else { return false };
    s.listens.push(Listen { lid, ep, arm: Some(arm), queue: alloc::vec::Vec::new(), seq: 0, seen: alloc::vec::Vec::new() });
    true
}

/// Every live listener, for stacks built after it (`latestack`).
static REGISTRY: Mutex<crate::latestack::Registry<IpEndpoint>> = Mutex::new(crate::latestack::Registry::new());

/// Record listener `lid` before it is put on the existing stacks.
pub(crate) fn register(lid: u32, local: IpEndpoint) { REGISTRY.lock().register(lid, local); }

/// Stack `sid` was just built: put every registered listener that belongs
/// on it there. Returns how many it carries. Caller holds no net lock.
pub(crate) fn adopt(sid: Sid) -> usize {
    let snap = REGISTRY.lock().snapshot();
    crate::latestack::adopt(&snap, |local| stacks_for(*local).contains(&sid),
        |lid, local| create(sid, lid, *local),
        |lid| REGISTRY.lock().contains(lid),
        |lid| destroy(sid, lid))
}

/// Take listener `lid` off every stack and wake its pollers: whoever is
/// parked on it (another thread of the closing process, or a disconnect by
/// connect(AF_UNSPEC)) re-probes and sees the socket's new state. Takes one
/// stack lock at a time; none is held across the wake.
pub(crate) fn destroy_all(lid: u32) {
    REGISTRY.lock().unregister(lid);
    for sid in all_sids() { destroy(sid, lid); }
    sched::wake_poll_key(wake_key(lid));
}

/// Take listener `lid` off stack `sid`: its LISTEN socket goes, connections
/// nobody accepted are reset (as Linux does when a listener closes). Takes
/// that stack's lock.
fn destroy(sid: Sid, lid: u32) {
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

/// Bring one listener up to date after a poll (see the module docs). True
/// when its edge-seq moved, i.e. a queued connection became ready for
/// accept(): the caller must then wake `wake_key(l.lid)`. This is the only
/// way the listener's poll answer turns readable (a connection only becomes
/// ready in a stack poll, and every stack poll is followed by `maintain`), so
/// the seq and the wake are produced by one condition and cannot diverge.
fn maintain_one(set: &mut SocketSet<'static>, l: &mut Listen, now: u64) -> bool {
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
    let mut moved = false;
    for &(h, _) in l.queue.iter() {
        if ready_state(set.get::<tcp::Socket>(h).state()) && !l.seen.contains(&h) {
            l.seen.push(h);
            l.seq = INET_SEQ_GEN.fetch_add(1, Ordering::Relaxed);
            moved = true;
        }
    }
    moved
}

/// Every listener of a stack, after its poll. Caller holds the stack lock.
/// Each listener whose edge-seq moved goes into `keys` (the daemon wakes
/// them once every stack lock is released), and the returned mask holds
/// their bloom bits, for when `keys` overflows into a bit wake.
pub(crate) fn maintain(s: &mut NetStack, now: u64, keys: &mut sched::WakeKeys) -> u64 {
    if s.listens.is_empty() { return 0; }
    let NetStack { socket_set, listens, .. } = s;
    let mut mask = 0u64;
    for l in listens.iter_mut() {
        if maintain_one(socket_set, l, now) {
            keys.push(wake_key(l.lid));
            mask |= sched::poll_key_bit(wake_key(l.lid));
        }
    }
    mask
}

/// The oldest completed connection of listener `lid` on stack `sid`, taken
/// out of its queue (the caller owns the handle from now on). Takes that
/// stack's lock.
pub(crate) fn accept(sid: Sid, lid: u32) -> Option<SocketHandle> {
    let mut moved = false;
    let r = (|| {
        let mut stack = stack_for(sid);
        let s = stack.as_mut()?;
        let NetStack { socket_set, listens, .. } = s;
        let l = listens.iter_mut().find(|l| l.lid == lid)?;
        moved |= maintain_one(socket_set, l, sched::monotonic_ns());
        let i = l.queue.iter().position(|(h, _)| ready_state(socket_set.get::<tcp::Socket>(*h).state()))?;
        let (h, _) = l.queue.remove(i);
        // A slot freed in the queue: re-arm if the backlog was full.
        moved |= maintain_one(socket_set, l, sched::monotonic_ns());
        Some(h)
    })();
    // The daemon's own `maintain` runs after every poll, so this never
    // fires today; it keeps the contract of `maintain_one` (an edge is
    // always woken) should a state ever change elsewhere.
    if moved { sched::wake_poll_key(wake_key(lid)); }
    r
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

/// `/proc/net/listen`: every listener on every stack, then each registered
/// listener with how many of the stacks it belongs on carry it. A line
/// `missing` names a stack that should hold a listener and does not (the
/// symptom lane pisshrace fixed: SYNs on that interface get an RST).
pub(crate) fn proc_listen(w: &mut dyn core::fmt::Write) {
    let name = |sid: Sid| -> alloc::string::String {
        if sid == LO { return "lo".into(); }
        drivers::netdev::iter().find(|n| iface::sid_of(n.id) == sid)
            .map(|n| alloc::string::String::from(n.name())).unwrap_or_else(|| alloc::format!("sid{}", sid))
    };
    let mut have: alloc::vec::Vec<(Sid, u32)> = alloc::vec::Vec::new();
    for sid in all_sids() {
        let nm = name(sid);
        let mut stack = stack_for(sid);
        let Some(s) = stack.as_mut() else { let _ = writeln!(w, "{} nostack", nm); continue };
        for l in s.listens.iter() {
            have.push((sid, l.lid));
            let arm = match l.arm {
                Some(h) => alloc::format!("{:?}", s.socket_set.get::<tcp::Socket>(h).state()),
                None => "none".into(),
            };
            let _ = writeln!(w, "{} lid {} port {} arm {} queue {}", nm, l.lid, l.ep.port, arm, l.queue.len());
        }
    }
    for (lid, local) in REGISTRY.lock().snapshot() {
        let want = stacks_for(local);
        let on = want.iter().filter(|sid| have.contains(&(**sid, lid))).count();
        let _ = write!(w, "registered lid {} {} on {}/{}", lid, local, on, want.len());
        for sid in want.iter().filter(|sid| !have.contains(&(**sid, lid))) { let _ = write!(w, " missing {}", name(*sid)); }
        let _ = writeln!(w);
    }
}
