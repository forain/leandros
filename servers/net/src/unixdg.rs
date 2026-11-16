//! AF_UNIX SOCK_DGRAM sockets that are not a socketpair: bind()+recvfrom()
//! servers (syslogd on /dev/log, $NOTIFY_SOCKET) and the unconnected or
//! connect()ed senders that talk to them.
//!
//! A bound datagram socket owns an `Inbox` keyed by its `BoundPath::sock_id`
//! (never reused, so no ABA). Every sender pushes a `DgMsg` carrying its own
//! name, credentials and any SCM_RIGHTS fds. A socketpair keeps its
//! `UnixConn` rings and is untouched; so are stream and seqpacket sockets.
//!
//! Lock rule: `UNIX_INBOX` is a leaf. Nothing else is locked under it, and
//! `xfer_drop` and wakes run after it is released.

use super::*;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

/// Linux `max_dgram_qlen`: the 11th message fits, the 12th is EAGAIN.
const DGRAM_QLEN: usize = 10;

pub(super) struct DgMsg {
    pub data: Vec<u8>,
    /// The sender's bound name as getsockname reports it; empty = unnamed.
    pub from: Vec<u8>,
    pub cred: Ucred,
    pub fds: Vec<XferFd>,
}

struct Inbox {
    q: VecDeque<DgMsg>,
    charge: usize,
    seq: u64,
    /// `sock_id` this socket is connected to (0 = none): the only sender
    /// allowed to reach it (Linux `unix_may_send`).
    peer: u64,
    shut_rd: bool,
}

static UNIX_INBOX: Mutex<BTreeMap<u64, Inbox>> = Mutex::new(BTreeMap::new());
static AUTOBIND_NEXT: AtomicU32 = AtomicU32::new(1);

/// Key of "the inbox of `sid` became readable".
pub(super) fn rd_key(sid: u64) -> u64 { sched::poll_key(sched::poll_class::UNIX_DG, sid as u32) }
/// Key of "the inbox of `sid` freed space or died".
pub(super) fn wr_key(sid: u64) -> u64 { sched::poll_key(sched::poll_class::UNIX_DGW, sid as u32) }

/// The entry of `fd` when it is an AF_UNIX SOCK_DGRAM socket that is not a
/// socketpair end: state `Unbound` or `UnixListening` (bound).
pub(super) fn dg_entry(pid: u32, fd: usize) -> Option<SockEntry> {
    let e = sock_entry(pid, fd)?;
    if e.domain as usize != AF_UNIX || e.sock_type & 0xf != SOCK_DGRAM as u8 { return None; }
    match e.state {
        SockState::Unbound { .. } | SockState::UnixListening { .. } => Some(e),
        _ => None,
    }
}

pub(super) fn sid_of(bound_idx: usize) -> u64 {
    let b = BOUND_PATHS.lock();
    b.get(bound_idx).filter(|b| b.in_use).map_or(0, |b| b.sock_id)
}

fn bound_name(bp: &BoundPath) -> Vec<u8> {
    let len = if bp.is_abstract { bp.path_len }
              else { bp.path[..bp.path_len].iter().position(|&b| b == 0).unwrap_or(bp.path_len) };
    bp.path[..len].to_vec()
}

/// `(sock_id, name)` of this socket's own address; `(0, empty)` when unbound.
fn my_name(e: &SockEntry) -> (u64, Vec<u8>) {
    let SockState::UnixListening { bound_idx } = e.state else { return (0, Vec::new()) };
    let b = BOUND_PATHS.lock();
    match b.get(bound_idx).filter(|b| b.in_use) {
        Some(bp) => (bp.sock_id, bound_name(bp)),
        None => (0, Vec::new()),
    }
}

fn name_of_sid(sid: u64) -> Option<Vec<u8>> {
    let b = BOUND_PATHS.lock();
    b.iter().find(|b| b.in_use && b.sock_id == sid).map(bound_name)
}

pub(super) fn create(sid: u64, peer: u64) {
    UNIX_INBOX.lock().insert(sid, Inbox { q: VecDeque::new(), charge: 0, seq: 0, peer, shut_rd: false });
}

/// The last reference to the address went away: drop the queue (and the fds
/// it holds) and wake everything parked on it.
pub(super) fn destroy(sid: u64) {
    let ib = UNIX_INBOX.lock().remove(&sid);
    if let Some(ib) = ib {
        for m in ib.q { for x in m.fds { xfer_drop(x); } }
        sched::wake_poll_key(rd_key(sid));
        sched::wake_poll_key(wr_key(sid));
    }
}

fn is_full(ib: &Inbox, n: usize) -> bool {
    ib.q.len() > DGRAM_QLEN
        || (!ib.q.is_empty() && ib.charge + n + DGRAM_OVERHEAD > UNIX_SNDBUF_DEFAULT as usize)
}

/// Queue `msg` for `dst`. Errors hand the message back (the caller drops
/// its fds with no lock held).
fn push(my_sid: u64, dst: u64, msg: DgMsg) -> Result<usize, (i32, DgMsg)> {
    let n = msg.data.len();
    let mut m = UNIX_INBOX.lock();
    let Some(ib) = m.get_mut(&dst) else { return Err((-111, msg)) };
    if ib.peer != 0 && ib.peer != my_sid { return Err((-1, msg)); }
    if ib.shut_rd { return Err((-32, msg)); }
    if is_full(ib, n) { return Err((-11, msg)); }
    ib.charge += n + DGRAM_OVERHEAD;
    ib.seq = ib.seq.wrapping_add(1);
    ib.q.push_back(msg);
    drop(m);
    sched::wake_poll_key(rd_key(dst));
    Ok(n)
}

pub(super) enum Pop { Msg(DgMsg), Empty { shut: bool }, Gone }

pub(super) fn pop(sid: u64, peek: bool) -> Pop {
    let mut m = UNIX_INBOX.lock();
    let Some(ib) = m.get_mut(&sid) else { return Pop::Gone };
    if peek {
        return match ib.q.front() {
            Some(h) => Pop::Msg(DgMsg { data: h.data.clone(), from: h.from.clone(), cred: h.cred, fds: Vec::new() }),
            None => Pop::Empty { shut: ib.shut_rd },
        };
    }
    match ib.q.pop_front() {
        Some(h) => {
            ib.charge = ib.charge.saturating_sub(h.data.len() + DGRAM_OVERHEAD);
            ib.seq = ib.seq.wrapping_add(1);
            drop(m);
            sched::wake_poll_key(wr_key(sid));
            Pop::Msg(h)
        }
        None => Pop::Empty { shut: ib.shut_rd },
    }
}

/// `(queued, edge seq, shut_rd)` of an inbox.
fn state_of(sid: u64) -> (bool, u64, bool) {
    UNIX_INBOX.lock().get(&sid).map_or((false, 0, true), |ib| (!ib.q.is_empty(), ib.seq, ib.shut_rd))
}

/// Length of the head datagram (FIONREAD).
pub(super) fn head_len(sid: u64) -> usize {
    UNIX_INBOX.lock().get(&sid).and_then(|ib| ib.q.front().map(|m| m.data.len())).unwrap_or(0)
}

fn set_peer(sid: u64, peer: u64) {
    if let Some(ib) = UNIX_INBOX.lock().get_mut(&sid) { ib.peer = peer; }
}

/// Poll answer of a datagram socket (`sid` = its bound id, 0 when unbound).
pub(super) fn poll(sid: u64, ofd: u32) -> (u64, Option<u64>, Option<sched::PollTag>) {
    let o = sock_opts(ofd);
    let (nonempty, seq, shut) = if sid != 0 { state_of(sid) } else { (false, 0, false) };
    let mut ev = 0u64;
    if nonempty || o.shut & SHUT_RD_BIT != 0 || (sid != 0 && shut) { ev |= POLLIN; }
    let peer_full = o.dg_peer != 0 && UNIX_INBOX.lock().get(&o.dg_peer).map_or(false, |ib| is_full(ib, 0));
    if !peer_full { ev |= POLLOUT; }
    if o.shut & (SHUT_RD_BIT | SHUT_WR_BIT) == (SHUT_RD_BIT | SHUT_WR_BIT) { ev |= POLLHUP; }
    let mut keys = alloc::vec![sock_state_key(ofd)];
    if sid != 0 { keys.push(rd_key(sid)); }
    if o.dg_peer != 0 { keys.push(wr_key(o.dg_peer)); }
    (ev, Some(seq), Some(keys_tag(&keys)))
}

/// What a blocking call on a datagram socket parks on (`fd_wake_tag`).
pub(super) fn wake_tag(e: &SockEntry) -> sched::PollTag {
    let o = sock_opts(e.ofd);
    let mut keys = alloc::vec![sock_state_key(e.ofd)];
    if let SockState::UnixListening { bound_idx } = e.state {
        let sid = sid_of(bound_idx);
        if sid != 0 { keys.push(rd_key(sid)); }
    }
    if o.dg_wait != 0 { keys.push(wr_key(o.dg_wait)); }
    if o.dg_peer != 0 && o.dg_peer != o.dg_wait { keys.push(wr_key(o.dg_peer)); }
    keys_tag(&keys)
}

/// Resolve a sockaddr_un name to `(sock_id, sock_type)` of a live bound
/// socket: an abstract name matches by bytes, a pathname goes through the VFS.
pub(super) fn resolve(pid: u32, slot: usize, pbytes: &[u8; PATH_MAX], path_len: usize,
                      host: Option<&[u8]>) -> Result<(u64, u8), i32> {
    if pbytes[0] == 0 {
        let nsid = sock_netns(pid, slot);
        let bound = BOUND_PATHS.lock();
        for bp in bound.iter() {
            if bp.in_use && bp.is_abstract && bp.netns == nsid && bp.path_len == path_len
               && bp.path[..path_len] == pbytes[..path_len] {
                return Ok((bp.sock_id, bp.sock_type));
            }
        }
        return Err(-111); // ECONNREFUSED
    }
    let name_end = pbytes[..path_len].iter().position(|&b| b == 0).unwrap_or(path_len);
    let name = &pbytes[..name_end];
    if name.is_empty() { return Err(-22); }
    let r = vfs::unix_resolve_node(pid, host.unwrap_or(name)); // sock_id (>=0) or -errno
    if r < 0 { return Err(r as i32); }
    let sock_id = r as u64;
    // The node resolved, but its socket may have since closed (Linux leaves
    // the socket file behind): ECONNREFUSED.
    let bound = BOUND_PATHS.lock();
    match bound.iter().find(|bp| bp.in_use && !bp.is_abstract && bp.sock_id == sock_id) {
        Some(bp) => Ok((sock_id, bp.sock_type)),
        None => Err(-111),
    }
}

/// bind(addrlen == 2) / SO_PASSCRED send or connect: give an unbound
/// datagram socket a fresh abstract name `\0xxxxx`.
pub(super) fn autobind(pid: u32, fd: usize) -> Result<(), i32> {
    let Some(e) = dg_entry(pid, fd) else { return Ok(()) };
    if !matches!(e.state, SockState::Unbound { .. }) { return Ok(()); }
    let Some(slot) = fd_to_slot(fd) else { return Err(-9) };
    let nsid = sock_netns(pid, slot);
    let mut pbytes = [0u8; PATH_MAX];
    let sock_id = alloc_sock_id();
    let idx = {
        let mut bound = BOUND_PATHS.lock();
        let mut tries = 0u32;
        loop {
            let c = AUTOBIND_NEXT.fetch_add(1, Ordering::Relaxed) & 0xf_ffff;
            const HEX: &[u8; 16] = b"0123456789abcdef";
            for i in 0..5 { pbytes[1 + i] = HEX[((c >> (4 * (4 - i))) & 0xf) as usize]; }
            if !bound.iter().any(|bp| bp.in_use && bp.is_abstract && bp.netns == nsid
                                      && bp.path_len == 6 && bp.path[..6] == pbytes[..6]) { break; }
            tries += 1;
            if tries > 0x10_0000 { return Err(-98); }
        }
        let Some(idx) = bound.iter().position(|b| !b.in_use) else { return Err(-12) };
        bound[idx] = BoundPath { in_use: true, path: pbytes, path_len: 6, sock_id, is_abstract: true,
                                 sock_type: SOCK_DGRAM as u8, netns: nsid, refs: 1,
                                 _owner_pid: pid, _owner_sock: slot };
        idx
    };
    create(sock_id, sock_opts(e.ofd).dg_peer);
    let ok = {
        let mut tbls = SOCK_TABLES.lock();
        match find_tbl(pid, &mut *tbls) {
            Some(t) if slot < t.socks.len() && t.socks[slot].in_use
                && matches!(t.socks[slot].state, SockState::Unbound { .. }) => {
                t.socks[slot].state = SockState::UnixListening { bound_idx: idx };
                true
            }
            _ => false,
        }
    };
    if !ok { free_bound_idx(idx); }
    Ok(())
}

pub(super) fn disconnect(pid: u32, fd: usize) -> Option<Message> {
    let e = dg_entry(pid, fd)?;
    let mut o = sock_opts(e.ofd);
    o.dg_peer = 0;
    set_sock_opts(e.ofd, o);
    let (sid, _) = my_name(&e);
    if sid != 0 { set_peer(sid, 0); }
    Some(ok_reply())
}

pub(super) fn connect(pid: u32, fd: usize, e: &SockEntry, pbytes: &[u8; PATH_MAX], path_len: usize,
                      host: Option<&[u8]>) -> Message {
    let Some(slot) = fd_to_slot(fd) else { return err_reply(-9) };
    if sock_opts(e.ofd).passcred {
        if let Err(er) = autobind(pid, fd) { return err_reply(er); }
    }
    let (dst, tt) = match resolve(pid, slot, pbytes, path_len, host) { Ok(v) => v, Err(er) => return err_reply(er) };
    if tt != SOCK_DGRAM as u8 { return err_reply(-91); } // EPROTOTYPE
    let me = sock_entry(pid, fd).unwrap_or_else(|| e.clone());
    let (my_sid, _) = my_name(&me);
    let their_peer = match UNIX_INBOX.lock().get(&dst) { Some(ib) => ib.peer, None => return err_reply(-111) };
    if their_peer != 0 && their_peer != my_sid { return err_reply(-1); } // EPERM
    let mut o = sock_opts(e.ofd);
    o.dg_peer = dst;
    set_sock_opts(e.ofd, o);
    if my_sid != 0 { set_peer(my_sid, dst); }
    ok_reply()
}

pub(super) fn getpeername(pid: u32, fd: usize, addr_ptr: usize, addrlen_ptr: usize) -> Option<Message> {
    let e = dg_entry(pid, fd)?;
    let peer = sock_opts(e.ofd).dg_peer;
    if peer == 0 { return Some(err_reply(-107)); }
    let Some(name) = name_of_sid(peer) else { return Some(err_reply(-107)) };
    Some(if put_sockaddr_un(addr_ptr, addrlen_ptr, &name) { ok_reply() } else { err_reply(-14) })
}

/// shutdown() of a datagram socket (bound or not): the UDP-style bits.
pub(super) fn shutdown(e: &SockEntry, how: usize) -> Message {
    let bits = match how { SHUT_RD => SHUT_RD_BIT, SHUT_WR => SHUT_WR_BIT, _ => SHUT_RD_BIT | SHUT_WR_BIT };
    let mut o = sock_opts(e.ofd);
    o.shut |= bits;
    set_sock_opts(e.ofd, o);
    if bits & SHUT_RD_BIT != 0 {
        let (sid, _) = my_name(e);
        if sid != 0 {
            if let Some(ib) = UNIX_INBOX.lock().get_mut(&sid) { ib.shut_rd = true; ib.seq = ib.seq.wrapping_add(1); }
            sched::wake_poll_key(rd_key(sid));
            sched::wake_poll_key(wr_key(sid));
        }
    }
    sched::wake_poll_key(sock_state_key(e.ofd));
    ok_reply()
}

fn drop_fds(fds: Vec<XferFd>) { for x in fds { xfer_drop(x); } }

/// Send one datagram from `e`. `name`: the user sockaddr (ptr, len), if the
/// caller gave one.
fn send_data(pid: u32, fd: usize, e: &SockEntry, data: Vec<u8>, name: Option<(usize, usize)>,
             host: Option<&[u8]>, fds: Vec<XferFd>, explicit: Option<Ucred>) -> Message {
    let ofd = e.ofd;
    let opts = sock_opts(ofd);
    if data.len() > unix_sndbuf(ofd).saturating_sub(32) || data.len() > UNIX_DGRAM_MAX {
        drop_fds(fds);
        return err_reply(-90); // EMSGSIZE
    }
    if opts.shut & SHUT_WR_BIT != 0 { drop_fds(fds); return err_reply(-32); }
    let Some(slot) = fd_to_slot(fd) else { drop_fds(fds); return err_reply(-9) };
    let mut named = false;
    let dst = match name {
        Some((ptr, len)) if ptr != 0 && len > 0 => {
            named = true;
            if len < 3 || len > 2 + PATH_MAX { drop_fds(fds); return err_reply(-22); }
            match sched::uaccess::read_user::<u16>(ptr) {
                Some(f) if f as usize == AF_UNIX => {}
                Some(_) => { drop_fds(fds); return err_reply(-22); }
                None => { drop_fds(fds); return err_reply(-14); }
            }
            let path_len = len - 2;
            let mut pbytes = [0u8; PATH_MAX];
            if !ucopy_in(&mut pbytes[..path_len], ptr + 2) { drop_fds(fds); return err_reply(-14); }
            match resolve(pid, slot, &pbytes, path_len, host) {
                Ok((sid, tt)) => {
                    if tt != SOCK_DGRAM as u8 { drop_fds(fds); return err_reply(-91); }
                    sid
                }
                Err(er) => { drop_fds(fds); return err_reply(er); }
            }
        }
        _ => {
            if opts.dg_peer == 0 { drop_fds(fds); return err_reply(-107); } // ENOTCONN
            opts.dg_peer
        }
    };
    if opts.passcred {
        if let Err(er) = autobind(pid, fd) { drop_fds(fds); return err_reply(er); }
    }
    let me = sock_entry(pid, fd).unwrap_or_else(|| e.clone());
    let (my_sid, from) = my_name(&me);
    let cred = explicit.unwrap_or(Ucred { pid, uid: sched::ruid_of(pid), gid: sched::rgid_of(pid) });
    match push(my_sid, dst, DgMsg { data, from, cred, fds }) {
        Ok(n) => val_reply(n as u64),
        Err((er, m)) => {
            drop_fds(m.fds);
            let mut o = sock_opts(ofd);
            if er == -11 && o.dg_wait != dst {
                o.dg_wait = dst;
                set_sock_opts(ofd, o);
            } else if er == -111 && !named && o.dg_peer == dst {
                // The connected peer is gone: forget it (Linux
                // unix_dgram_disconnected), the next send is ENOTCONN.
                o.dg_peer = 0;
                set_sock_opts(ofd, o);
            }
            err_reply(er)
        }
    }
}

/// send/sendto on a datagram socket.
pub(super) fn send_k(pid: u32, fd: usize, e: SockEntry, buf_ptr: usize, len: usize,
                     addr_ptr: usize, addrlen: usize, kdata: Option<Vec<u8>>, host: Option<&[u8]>) -> Message {
    let data = match kdata {
        Some(d) => d,
        None => {
            if len > UNIX_DGRAM_MAX { return err_reply(-90); }
            let mut d: Vec<u8> = Vec::new();
            if d.try_reserve_exact(len).is_err() { return err_reply(-105); }
            d.resize(len, 0);
            if len > 0 && !ucopy_in(&mut d, buf_ptr) { return err_reply(-14); }
            d
        }
    };
    send_data(pid, fd, &e, data, Some((addr_ptr, addrlen)), host, Vec::new(), None)
}

/// sendmsg on a datagram socket: msg_name, the iovecs, SCM_RIGHTS and an
/// explicit SCM_CREDENTIALS.
pub(super) fn sendmsg(pid: u32, fd: usize, e: SockEntry, msghdr_ptr: usize, syn: bool,
                      one: Option<(usize, usize)>, iov_ptr: usize, iovcnt: usize,
                      ctrl_ptr: usize, ctrl_len: usize, host: Option<&[u8]>) -> Message {
    let (name_ptr, name_len) = if syn { (0, 0) } else {
        let Some(p) = sched::uaccess::read_user::<usize>(msghdr_ptr) else { return err_reply(-14) };
        let Some(l) = sched::uaccess::read_user::<u32>(msghdr_ptr + 8) else { return err_reply(-14) };
        (p, l as usize)
    };
    let mut fd_nums = [0i32; SCM_MAX_FD];
    let mut nfd = 0usize;
    let mut explicit = None;
    if ctrl_ptr != 0 && ctrl_len >= CMSG_HDR_LEN {
        match unsafe { parse_scm_rights(ctrl_ptr, ctrl_len.min(MAX_CONTROL), &mut fd_nums) } {
            Ok(n) => nfd = n,
            Err(er) => return err_reply(er),
        }
        match parse_scm_creds(pid, ctrl_ptr, ctrl_len.min(MAX_CONTROL)) {
            Ok(c) => explicit = c,
            Err(er) => return err_reply(er),
        }
    }
    let mut data: Vec<u8> = Vec::new();
    for i in 0..iovcnt.min(UIO_MAXIOV) {
        let iov = iov_ptr + i * 16;
        let (base, len) = match one {
            Some(o) => o,
            None => {
                let (Some(b), Some(l)) = (sched::uaccess::read_user::<usize>(iov),
                                          sched::uaccess::read_user::<usize>(iov + 8)) else { return err_reply(-14) };
                (b, l)
            }
        };
        if len > 0 && !sched::uaccess::user_range_ok(base, len) { return err_reply(-14); }
        if data.len() + len > UNIX_DGRAM_MAX { return err_reply(-90); }
        let at = data.len();
        if data.try_reserve(len).is_err() { return err_reply(-105); }
        data.resize(at + len, 0);
        if len > 0 && !ucopy_in(&mut data[at..], base) { return err_reply(-14); }
    }
    let mut batch: Vec<XferFd> = Vec::new();
    for i in 0..nfd {
        match xfer_export(pid, fd_nums[i] as usize) {
            Some(tf) => batch.push(tf),
            None => { drop_fds(batch); return err_reply(-9); }
        }
    }
    send_data(pid, fd, &e, data, Some((name_ptr, name_len)), host, batch, explicit)
}

/// recv/recvfrom: the head datagram into `buf_ptr`.
pub(super) fn recv(sid: u64, buf_ptr: usize, len: usize, addr_ptr: usize, addrlen_ptr: usize,
                   flags: usize) -> Message {
    let peek = flags & MSG_PEEK != 0;
    match pop(sid, peek) {
        Pop::Msg(m) => {
            let n = m.data.len().min(len);
            if n > 0 && !ucopy_out(buf_ptr, &m.data[..n]) { drop_fds(m.fds); return err_reply(-14); }
            if addr_ptr != 0 && addrlen_ptr != 0 {
                if m.from.is_empty() { wr_user(addrlen_ptr, 0u32); }
                else { put_sockaddr_un(addr_ptr, addrlen_ptr, &m.from); }
            }
            let full = m.data.len();
            drop_fds(m.fds);
            val_reply((if flags & MSG_TRUNC as usize != 0 { full } else { n }) as u64)
        }
        Pop::Empty { shut: true } => val_reply(0),
        Pop::Empty { .. } => err_reply(-11),
        Pop::Gone => err_reply(-9),
    }
}

/// recvmsg: data over the iovecs, the sender's name, SCM_RIGHTS and (with
/// SO_PASSCRED) SCM_CREDENTIALS.
pub(super) fn recvmsg(pid: u32, e: &SockEntry, sid: u64, msghdr_ptr: usize, syn: bool,
                      iov_ptr: usize, iovcnt: usize, ctrl_ptr: usize, ctrl_cap: usize,
                      flags: usize) -> Message {
    let peek = flags & MSG_PEEK != 0;
    let mut iovs: Vec<(usize, usize)> = Vec::new();
    for i in 0..iovcnt.min(UIO_MAXIOV) {
        let iov = iov_ptr + i * 16;
        let (Some(b), Some(l)) = (sched::uaccess::read_user::<usize>(iov),
                                  sched::uaccess::read_user::<usize>(iov + 8)) else { return err_reply(-14) };
        if l > 0 && !sched::uaccess::user_range_ok(b, l) { return err_reply(-14); }
        iovs.push((b, l));
    }
    let m = match pop(sid, peek) {
        Pop::Msg(m) => m,
        Pop::Empty { shut: true } => {
            if !syn { unsafe { write_msg_tail(msghdr_ptr, 0, 0); } }
            return val_reply(0);
        }
        Pop::Empty { .. } => return err_reply(-11),
        Pop::Gone => return err_reply(-9),
    };
    let full = m.data.len();
    let mut off = 0usize;
    for &(b, l) in iovs.iter() {
        if off >= full { break; }
        let k = l.min(full - off);
        if k > 0 && !ucopy_out(b, &m.data[off..off + k]) { drop_fds(m.fds); return err_reply(-14); }
        off += k;
    }
    if !syn {
        if let Some(name_ptr) = sched::uaccess::read_user::<usize>(msghdr_ptr).filter(|&p| p != 0) {
            if m.from.is_empty() { wr_user(msghdr_ptr + 8, 0u32); }
            else { put_sockaddr_un(name_ptr, msghdr_ptr + 8, &m.from); }
        }
    }
    let cred = if sock_opts(e.ofd).passcred { Some(m.cred) } else { None };
    let fds = if m.fds.is_empty() { None } else { Some(m.fds) };
    let (ctrl_written, ctrunc) = deliver_scm(pid, fds, cred, ctrl_ptr, ctrl_cap, flags);
    let truncated = full > off;
    let mflags = (if truncated { MSG_TRUNC } else { 0 }) | (if ctrunc { MSG_CTRUNC } else { 0 });
    if !syn { unsafe { write_msg_tail(msghdr_ptr, ctrl_written, mflags); } }
    val_reply((if truncated && flags & MSG_TRUNC as usize != 0 { full } else { off }) as u64)
}

/// The host path the kernel resolved for a pathname destination (args 6-7 of
/// NET_SEND / NET_SENDMSG), so a chrooted or mount-namespace sender reaches
/// the socket node a plain open() would.
fn host_arg(msg: &Message) -> Option<&'static [u8]> {
    let (ptr, len) = (arg(msg, 6) as usize, arg(msg, 7) as usize);
    if ptr == 0 || len == 0 { return None; }
    // SAFETY: a kernel buffer owned by the calling syscall (see `host_path_arg`).
    Some(unsafe { core::slice::from_raw_parts(ptr as *const u8, len) })
}

/// NET_SEND carrying a host path: only for a datagram socket.
pub(super) fn handle_send(pid: u32, msg: &Message) -> Option<Message> {
    let fd = arg(msg, 0) as usize;
    let e = dg_entry(pid, fd)?;
    Some(send_k(pid, fd, e, arg(msg, 1) as usize, arg(msg, 2) as usize,
                arg(msg, 4) as usize, arg(msg, 5) as usize, None, host_arg(msg)))
}

/// NET_SENDMSG carrying a host path (a real msghdr, not a writev).
pub(super) fn handle_sendmsg(pid: u32, msg: &Message) -> Option<Message> {
    let fd = arg(msg, 0) as usize;
    let mh = arg(msg, 1) as usize;
    let e = dg_entry(pid, fd)?;
    let rd = |o: usize| sched::uaccess::read_user::<usize>(mh + o);
    let (Some(iov), Some(cnt), Some(cp), Some(cl)) = (rd(16), rd(24), rd(32), rd(40)) else {
        return Some(err_reply(-14));
    };
    if cp != 0 && cl > 0 && !sched::uaccess::user_range_ok(cp, cl) { return Some(err_reply(-14)); }
    Some(sendmsg(pid, fd, e, mh, false, None, iov, cnt, cp, cl, host_arg(msg)))
}
