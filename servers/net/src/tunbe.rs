//! The net server's side of `/dev/net/tun` (see `tundev`): tap/tun
//! interfaces as dynamic vifs of the opener's network namespace, each with
//! its own smoltcp stack.
//!
//! Packet flow (design-podnet.md section 3):
//! * the stack transmits into `TapDev.tx`, a Vec filled during `poll_stack`;
//!   the frames are handed to `tundev::deliver` after the stack lock is
//!   released (stack lock -> nothing);
//! * `tundev` write -> `input` pushes onto the vif's `rxq` (leaf `VIFS`
//!   lock), sets `pending` and kicks the daemon; the next pass drains `rxq`
//!   before taking the stack lock.
//!
//! The stack always runs Ethernet. IFF_TUN (L3) mode adds/strips the
//! Ethernet header at the fd boundary and answers the stack's ARP for the
//! peer itself.

use super::*;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use syngate::{Frames, GateRx};
use tundev::TunHandle;
use vif::{VIFS, NETNS};
use vifsel::{NetNsState, Vif, VifKind, NO_TUN, NO_PEER, RXQ_CAP, ns_sid_cap};
use iface::FIRST_DYN;

const CAP_NET_ADMIN: u32 = 12;

static BACKEND: tundev::TunBackend = tundev::TunBackend { attach, input, detach };

pub(crate) fn init() { tundev::set_backend(&BACKEND); }

fn peer_mac(mac: [u8; 6]) -> [u8; 6] { let mut m = mac; m[5] ^= 0x80; m }

fn name_len(n: &[u8; 16]) -> usize { n.iter().position(|&b| b == 0).unwrap_or(16) }

fn is_tun_kind(k: VifKind) -> bool { matches!(k, VifKind::Tap | VifKind::Tun | VifKind::Veth | VifKind::Bridge) }

// ── hooks ────────────────────────────────────────────────────────────────────

fn attach(ns: u16, name: &[u8; 16], tap: bool, h: TunHandle) -> Result<(u32, [u8; 16]), i32> {
    let n = name_len(name);
    let kind = if tap { VifKind::Tap } else { VifKind::Tun };
    crate::vif::claim(ns);
    let base: Vec<u8> = if n == 0 { (if tap { b"tap%d" } else { b"tun%d" }).to_vec() } else { name[..n].to_vec() };
    // Re-attach of a persistent interface by exact name.
    let has_pct = base.windows(2).any(|w| w == b"%d");
    if !has_pct {
        let sid = {
            let nss = NETNS.lock();
            let v = VIFS.lock();
            nss.get(&ns).and_then(|st| st.sids.iter().copied().find(|&s| {
                v[s as usize].as_ref().map_or(false, |x| x.name[..name_len(&x.name)] == base[..])
            }))
        };
        if let Some(sid) = sid {
            let mut v = VIFS.lock();
            let x = v[sid as usize].as_mut().ok_or(19)?;
            if !is_tun_kind(x.kind) || x.kind != kind || x.tun != NO_TUN { return Err(16); } // EBUSY
            x.tun = h;
            return Ok((x.ifindex, x.name));
        }
    }
    // A new interface: a free name, a free sid, an ifindex.
    let (sid, fin, ifindex, mac) = {
        let mut nss = NETNS.lock();
        let mut v = VIFS.lock();
        let st = nss.entry(ns).or_insert_with(|| NetNsState::new(ns));
        if st.sids.len() >= ns_sid_cap(ns) { return Err(23); }
        let taken = |s: &NetNsState, v: &vifsel::VifTable, nm: &[u8]| {
            s.sids.iter().any(|&q| v[q as usize].as_ref().map_or(false, |x| x.name[..name_len(&x.name)] == *nm))
            || (nm == b"lo")
        };
        let mut fin = [0u8; 16];
        if let Some(p) = base.windows(2).position(|w| w == b"%d") {
            let mut found = false;
            for i in 0..64u32 {
                let mut s = Vec::new();
                s.extend_from_slice(&base[..p]);
                s.extend_from_slice(alloc::format!("{}", i).as_bytes());
                s.extend_from_slice(&base[p + 2..]);
                if s.len() > 15 { return Err(22); }
                if !taken(st, &v, &s) { fin[..s.len()].copy_from_slice(&s); found = true; break; }
            }
            if !found { return Err(17); }
        } else {
            if taken(st, &v, &base) { return Err(17); } // EEXIST
            fin[..base.len()].copy_from_slice(&base);
        }
        let Some(sid) = (FIRST_DYN as usize..MAX_STACKS).find(|&i| v[i].is_none()) else { return Err(23) };
        let ifindex = st.next_ifindex;
        st.next_ifindex += 1;
        let mac = [0x02, 0x00, 0x5e, 0x10, (sid as u8), 0x01];
        v[sid] = Some(Vif { netns: ns, ifindex, name: fin, kind, admin_up: false, mtu: 1500, mac,
                            addrs: Vec::new(), addrs6: Vec::new(), routes6: Vec::new(), rxq: VecDeque::new(), rxsrc: VecDeque::new(), tun: h, pending: false, peer: NO_PEER, master: NO_PEER });
        st.sids.push(sid as Sid);
        (sid as Sid, fin, ifindex, mac)
    };
    make_stack(sid, mac);
    Ok((ifindex, fin))
}

fn detach(h: TunHandle, keep: bool) {
    let found = {
        let nss = NETNS.lock();
        let v = VIFS.lock();
        nss.iter().find_map(|(&ns, st)| st.sids.iter().copied()
            .find(|&s| v[s as usize].as_ref().map_or(false, |x| x.tun == h && is_tun_kind(x.kind)))
            .map(|s| (ns, s)))
    };
    let Some((ns, sid)) = found else { return };
    if keep {
        let mut v = VIFS.lock();
        if let Some(x) = v[sid as usize].as_mut() { x.tun = NO_TUN; x.rxq.clear(); x.rxsrc.clear(); }
        return;
    }
    {
        let mut nss = NETNS.lock();
        let mut v = VIFS.lock();
        v[sid as usize] = None;
        if let Some(st) = nss.get_mut(&ns) { st.sids.retain(|&s| s != sid); }
    }
    // Neither table is held: the stack lock is taken alone.
    *stack_for(sid) = None;
}

fn input(h: TunHandle, frame: &[u8]) -> Result<(), i32> {
    let (sid, _ns) = find_sid(h).ok_or(77)?;
    let mut v = VIFS.lock();
    let x = v[sid as usize].as_mut().ok_or(77)?;
    if !x.admin_up { return Err(5); } // EIO: link down
    let f = if x.kind == VifKind::Tun {
        if frame.is_empty() { return Err(22); }
        let et: u16 = if frame[0] >> 4 == 6 { 0x86dd } else { 0x0800 };
        let mut f = alloc::vec![0u8; 14];
        f[0..6].copy_from_slice(&x.mac);
        f[6..12].copy_from_slice(&peer_mac(x.mac));
        f[12..14].copy_from_slice(&et.to_be_bytes());
        f.extend_from_slice(frame);
        f
    } else {
        if frame.len() < 14 { return Err(22); }
        frame.to_vec()
    };
    if x.rxq.len() < RXQ_CAP { x.rxq.push_back(f); }
    x.pending = true;
    drop(v);
    drivers::netdev::kick();
    Ok(())
}

fn find_sid(h: TunHandle) -> Option<(Sid, u16)> {
    let nss = NETNS.lock();
    let v = VIFS.lock();
    nss.iter().find_map(|(&ns, st)| st.sids.iter().copied()
        .find(|&s| v[s as usize].as_ref().map_or(false, |x| x.tun == h && is_tun_kind(x.kind)))
        .map(|s| (s, ns)))
}

// ── the stack's device ───────────────────────────────────────────────────────

pub(crate) use framedev::FrameDev as TapDev;

fn make_stack(sid: Sid, mac: [u8; 6]) {
    let mut dev = TapDev { rx: VecDeque::new(), tx: Vec::new(), mtu: 1500 };
    let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    config.random_seed = sched::ticks() ^ sched::monotonic_ns();
    let interface = Interface::new(config, &mut dev, stack_now());
    *stack_for(sid) = Some(NetStack {
        interface,
        socket_set: SocketSet::new(alloc::vec![]),
        dhcp_handle: None,
        loopback_dev: None,
        orphans: Vec::new(),
        icmp_peeked: alloc::collections::BTreeMap::new(),
        listens: Vec::new(),
        inet_seq: alloc::collections::BTreeMap::new(),
    });
    // Host wildcard listeners set up before this stack existed (httpd on
    // 0.0.0.0 before podman0 is created) belong on it too: Linux delivers to
    // them on every host address. `adopt` skips stacks of other namespaces
    // (listen::stacks_for) and is a no-op with none.
    let _ = crate::listen::adopt(sid);
}

/// Take from `tx` the frames a stack built for one of its own interface
/// addresses and queue them on its own receive side: an ARP request for an
/// own address is answered (with the interface MAC) and an IPv4 frame
/// addressed to the interface's own MAC is received as sent, so a connect()
/// to the netns's own address reaches its listener or an RST from it.
fn loop_own(sid: Sid, mac: [u8; 6], tx: &mut Vec<Vec<u8>>) {
    if tx.is_empty() { return; }
    let addrs: Vec<[u8; 4]> = {
        let v = VIFS.lock();
        match v[sid as usize].as_ref() { Some(x) => x.addrs.iter().map(|&(a, _)| a).collect(), None => return }
    };
    let mut back: Vec<Vec<u8>> = Vec::new();
    tx.retain(|f| {
        if f.len() < 14 { return true; }
        let et = u16::from_be_bytes([f[12], f[13]]);
        if et == 0x0806 && f.len() >= 42 && f[20..22] == [0, 1] && addrs.iter().any(|a| a[..] == f[38..42]) {
            let mut r = alloc::vec![0u8; 42];
            r[0..6].copy_from_slice(&f[6..12]);
            r[6..12].copy_from_slice(&mac);
            r[12..14].copy_from_slice(&[0x08, 0x06]);
            r[14..20].copy_from_slice(&f[14..20]);
            r[20..22].copy_from_slice(&[0, 2]);
            r[22..28].copy_from_slice(&mac);
            r[28..32].copy_from_slice(&f[38..42]);
            r[32..38].copy_from_slice(&f[22..28]);
            r[38..42].copy_from_slice(&f[28..32]);
            back.push(r);
            return false;
        }
        if et == 0x0800 && f[0] & 1 == 0 && f[0..6] == mac {
            back.push(f.clone());
            return false;
        }
        true
    });
    if back.is_empty() { return; }
    {
        let mut v = VIFS.lock();
        if let Some(x) = v[sid as usize].as_mut() {
            for f in back { if x.rxq.len() < RXQ_CAP { x.rxq.push_back(f); } }
            x.pending = true;
        }
    }
    drivers::netdev::kick();
}

/// Poll every up tun/tap stack once. Called from the daemon pass.
pub(crate) fn poll_all(timestamp: Instant, sigs: &mut [Vec<Sig>], keys: &mut sched::WakeKeys,
                       changed: &mut bool, mask: &mut u64, next: &mut Option<smoltcp::time::Duration>) {
    let mut sids: Vec<Sid> = Vec::new();
    {
        let v = VIFS.lock();
        for i in FIRST_DYN as usize..MAX_STACKS {
            if let Some(x) = v[i].as_ref() { if is_tun_kind(x.kind) && x.admin_up { sids.push(i as Sid); } }
        }
    }
    for sid in sids {
        // Drain the vif's queue before the stack lock is taken.
        let (rx, rxsrc, mtu, h, l3, mac, peer, kind, master) = {
            let mut v = VIFS.lock();
            let Some(x) = v[sid as usize].as_mut() else { continue };
            x.pending = false;
            let peer = if x.kind == VifKind::Veth { Some(x.peer) } else { None };
            (core::mem::take(&mut x.rxq), core::mem::take(&mut x.rxsrc), x.mtu as usize, x.tun, x.kind == VifKind::Tun, x.mac, peer, x.kind, x.master)
        };
        // Frames to the bridge MAC for another network may be forwarded (fwd.rs).
        let rx = if kind == VifKind::Bridge { crate::fwd::divert(sid, rx, rxsrc) } else { rx };
        // A bridge port has no stack: what its peer sent goes into the switch.
        if kind == VifKind::Veth && master != NO_PEER {
            if !rx.is_empty() { crate::l2::forward(master, sid, rx.into_iter().collect()); }
            continue;
        }
        let mut dev = TapDev { rx, tx: Vec::new(), mtu };
        {
            let mut g = stack_for(sid);
            if let Some(ref mut s) = *g {
                let (c, m) = poll_stack(s, &mut dev, timestamp, &mut sigs[sid as usize], sid, keys);
                *changed |= c; *mask |= m;
                reap_orphans(s);
                if let Some(d) = s.interface.poll_delay(stack_now(), &s.socket_set) {
                    *next = Some(next.map_or(d, |n| n.min(d)));
                }
            }
        }
        // A stack talking to its own interface address: Linux delivers it
        // locally (the `local` table); here the frames loop back to its own
        // receive queue instead of waiting on a neighbour that is itself.
        let mut dev = dev;
        loop_own(sid, mac, &mut dev.tx);
        // Stack lock released: the bridge's own frames enter the switch.
        if kind == VifKind::Bridge {
            if !dev.tx.is_empty() { crate::l2::forward(sid, sid, dev.tx); }
            continue;
        }
        // A veth end hands its frames to the other end.
        if let Some(p) = peer {
            if dev.tx.is_empty() { continue; }
            let mut v = VIFS.lock();
            let mtu_ok = |f: &Vec<u8>, m: usize| f.len() <= m + 14;
            if let Some(x) = v.get_mut(p as usize).and_then(|x| x.as_mut()) {
                if x.admin_up && x.kind == VifKind::Veth {
                    let m = x.mtu as usize;
                    for f in dev.tx {
                        if x.rxq.len() < RXQ_CAP && mtu_ok(&f, m) { x.rxq.push_back(f); }
                    }
                    x.pending = true;
                    drop(v);
                    drivers::netdev::kick();
                }
            }
            continue;
        }
        // Stack lock released: hand the frames to tundev.
        if h == NO_TUN { continue; }
        for f in dev.tx {
            if f.len() < 14 { continue; }
            if !l3 { tundev::deliver(h, f); continue; }
            let et = u16::from_be_bytes([f[12], f[13]]);
            if et == 0x0806 && f.len() >= 42 && f[20..22] == [0, 1] {
                // ARP request for the peer: the fd's reader never sees L2.
                let pm = peer_mac(mac);
                let mut r = alloc::vec![0u8; 42];
                r[0..6].copy_from_slice(&f[22..28]);
                r[6..12].copy_from_slice(&pm);
                r[12..14].copy_from_slice(&[0x08, 0x06]);
                r[14..20].copy_from_slice(&f[14..20]);
                r[20..22].copy_from_slice(&[0, 2]);
                r[22..28].copy_from_slice(&pm);
                r[28..32].copy_from_slice(&f[38..42]);
                r[32..38].copy_from_slice(&f[22..28]);
                r[38..42].copy_from_slice(&f[28..32]);
                let mut v = VIFS.lock();
                if let Some(x) = v[sid as usize].as_mut() { if x.rxq.len() < RXQ_CAP { x.rxq.push_back(r); x.pending = true; } }
                drop(v);
                drivers::netdev::kick();
            } else if et == 0x0800 || et == 0x86dd {
                tundev::deliver(h, f[14..].to_vec());
            }
        }
    }
}

// ── SIOCSIF* on an interface of the caller's namespace ──────────────────────

/// SIOCSIFFLAGS/ADDR/NETMASK/MTU for a tun/tap named in `ifr` in `ns`.
/// `None` = no such tun/tap here (the caller falls through to `lo` / ENODEV).
pub fn sock_set(ns: u16, cmd: u32, ifr: &[u8; 40]) -> Option<i32> {
    let mut nm = [0u8; 16];
    nm[..15].copy_from_slice(&ifr[..15]);
    let n = name_len(&nm);
    let sid = {
        let nss = NETNS.lock();
        let v = VIFS.lock();
        let st = nss.get(&ns)?;
        st.sids.iter().copied().find(|&s| v[s as usize].as_ref()
            .map_or(false, |x| is_tun_kind(x.kind) && x.name[..name_len(&x.name)] == nm[..n]))?
    };
    if !sched::caps::ns_capable_in(sched::ns::owner_userns(1, ns), CAP_NET_ADMIN) { return Some(-1); }
    let mut new_addr: Option<([u8; 4], u8)> = None;
    {
        let mut v = VIFS.lock();
        let x = v[sid as usize].as_mut()?;
        match cmd {
            0x8914 => { // SIFFLAGS
                let f = u16::from_ne_bytes([ifr[16], ifr[17]]);
                x.admin_up = f & 1 != 0;
                if !x.admin_up { x.rxq.clear(); x.rxsrc.clear(); }
            }
            0x8916 | 0x891c => { // SIFADDR / SIFNETMASK
                let a = [ifr[20], ifr[21], ifr[22], ifr[23]];
                if cmd == 0x8916 && u16::from_ne_bytes([ifr[16], ifr[17]]) != 2 { return Some(-97); }
                let cur = x.addrs.first().copied().unwrap_or(([0; 4], 24));
                let new = if cmd == 0x8916 { (a, cur.1) } else { (cur.0, u32::from_be_bytes(a).leading_ones() as u8) };
                x.addrs.clear();
                x.addrs.push(new);
                new_addr = Some(new);
            }
            0x8922 => { // SIFMTU
                let m = i32::from_ne_bytes([ifr[16], ifr[17], ifr[18], ifr[19]]);
                if !(68..=65535).contains(&m) { return Some(-22); }
                x.mtu = m as u32;
            }
            _ => return Some(-95),
        }
    }
    if let Some((a, p)) = new_addr {
        // VIFS released: stack lock alone.
        if let Some(s) = stack_for(sid).as_mut() {
            s.interface.update_ip_addrs(|v| {
                v.clear();
                let _ = v.push(IpCidr::new(IpAddress::v4(a[0], a[1], a[2], a[3]), p));
            });
        }
    }
    drivers::netdev::kick();
    Some(0)
}

/// Does `ns` have any tun/tap interface?
pub(crate) fn has_vifs(ns: u16) -> bool {
    let nss = NETNS.lock();
    let v = VIFS.lock();
    nss.get(&ns).map_or(false, |st| st.sids.iter().any(|&s| v[s as usize].as_ref().map_or(false, |x| is_tun_kind(x.kind))))
}

/// The tun/tap interfaces of `ns` as netlink/ioctl links.
pub(crate) fn links(ns: u16) -> Vec<rtnetlink::Link> {
    let nss = NETNS.lock();
    let v = VIFS.lock();
    let mut out = Vec::new();
    if let Some(st) = nss.get(&ns) {
        for &s in st.sids.iter() {
            let Some(x) = v[s as usize].as_ref() else { continue };
            if !is_tun_kind(x.kind) { continue; }
            out.push(rtnetlink::Link {
                index: x.ifindex,
                name: alloc::string::String::from(core::str::from_utf8(&x.name[..name_len(&x.name)]).unwrap_or("?")),
                mac: x.mac, mtu: x.mtu, up: x.admin_up,
                running: x.admin_up && if x.kind == VifKind::Bridge { true } else if x.kind == VifKind::Veth {
                    v.get(x.peer as usize).and_then(|p| p.as_ref()).map_or(false, |p| p.admin_up)
                } else { x.tun != NO_TUN },
                tap: true, loopback: false,
                addr: if x.admin_up { x.addrs.first().copied() } else { None },
                veth: x.kind == VifKind::Veth,
                master: if x.master != NO_PEER { v.get(x.master as usize).and_then(|m| m.as_ref()).map_or(0, |m| m.ifindex) } else { 0 },
                br: if x.kind == VifKind::Bridge { let c = crate::l2::cfg_get(s); Some((c.ageing, c.fwd_delay, c.stp, c.vlan)) } else { None },
                peer: if x.kind == VifKind::Veth { v.get(x.peer as usize).and_then(|p| p.as_ref()).map_or(0, |p| p.ifindex) } else { 0 },
            });
        }
    }
    out
}

// ── rtnetlink writes on an interface of the caller's namespace ──────────────

use rtnetlink::RtOp;

fn net_of(a: [u8; 4], p: u8) -> u32 {
    let m = if p == 0 { 0 } else { u32::MAX << (32 - p.min(32) as u32) };
    u32::from_be_bytes(a) & m
}

/// Push the vif's address list into its smoltcp stack (VIFS released).
fn sync_addrs(sid: Sid, addrs: &[([u8; 4], u8)]) {
    let v6 = addrs6_of(sid);
    if let Some(s) = stack_for(sid).as_mut() {
        s.interface.update_ip_addrs(|v| {
            v.clear();
            for &(a, p) in addrs { let _ = v.push(IpCidr::new(IpAddress::v4(a[0], a[1], a[2], a[3]), p)); }
            for &(a, p) in &v6 { let _ = v.push(IpCidr::new(IpAddress::Ipv6(smoltcp::wire::Ipv6Address(a)), p)); }
        });
    }
}

/// The modified EUI-64 link-local address of a MAC.
pub(crate) fn ll_from_mac(m: [u8; 6]) -> [u8; 16] {
    [0xfe, 0x80, 0, 0, 0, 0, 0, 0, m[0] ^ 2, m[1], m[2], 0xff, 0xfe, m[3], m[4], m[5]]
}

/// Every v6 address a vif's stack holds: link-local while up, then the static ones.
fn addrs6_of(sid: Sid) -> Vec<([u8; 16], u8)> {
    let v = VIFS.lock();
    let Some(x) = v[sid as usize].as_ref() else { return Vec::new() };
    let mut o = Vec::new();
    if is_tun_kind(x.kind) && x.admin_up && !sched::ns::ipv6_disabled(x.netns, &x.name[..name_len(&x.name)]) { o.push((ll_from_mac(x.mac), 64)); }
    o.extend(x.addrs6.iter().copied());
    o
}

/// Re-push a vif's address list (after link state or address changes).
fn resync(sid: Sid) {
    let a = { let v = VIFS.lock(); v[sid as usize].as_ref().map(|x| x.addrs.clone()) };
    if let Some(a) = a { sync_addrs(sid, &a); }
}

/// IPv6 addresses of the tun/tap/veth/bridge links of `ns` (rtnetlink view).
pub(crate) fn addrs6(ns: u16) -> Vec<rtnetlink::A6> {
    let nss = NETNS.lock();
    let mut out = Vec::new();
    let Some(st) = nss.get(&ns) else { return out };
    let sids: Vec<Sid> = st.sids.iter().copied().collect();
    let idx: Vec<(Sid, u32)> = { let v = VIFS.lock(); sids.iter().filter_map(|&s| v[s as usize].as_ref().filter(|x| is_tun_kind(x.kind)).map(|x| (s, x.ifindex))).collect() };
    drop(nss);
    for (s, i) in idx {
        for (k, (a, p)) in addrs6_of(s).into_iter().enumerate() {
            let ll = a[0] == 0xfe && a[1] == 0x80;
            let _ = k;
            out.push(rtnetlink::A6 { index: i, addr: a, prefix: p, scope: if ll { 253 } else { 0 }, flags: 0x80 });
        }
    }
    out
}

/// Connected IPv6 routes of the vifs of `ns`.
pub(crate) fn routes6(ns: u16) -> Vec<rtnetlink::R6> {
    let mut out = Vec::new();
    for a in addrs6(ns) {
        let mut d = a.addr; let p = a.prefix;
        for (i, b) in d.iter_mut().enumerate() { let keep = (p as i32 - i as i32 * 8).clamp(0, 8) as u32; *b &= if keep == 0 { 0 } else { 0xffu8 << (8 - keep) }; }
        out.push(rtnetlink::R6 { dst: d, plen: p, gw: None, oif: a.index, metric: 256, proto: 2 });
    }
    {
        let v = VIFS.lock();
        for s in sids_of(ns) {
            let Some(x) = v[s as usize].as_ref() else { continue };
            for &(d, p, g, m) in &x.routes6 { out.push(rtnetlink::R6 { dst: d, plen: p, gw: Some(g), oif: x.ifindex, metric: m, proto: 3 }); }
        }
    }
    out
}

/// The vif of `ns` a v6 destination leaves through: the longest on-link prefix.
pub(crate) fn route6_in_ns(ns: u16, dst: [u8; 16]) -> Option<Sid> {
    let sids: Vec<Sid> = { let nss = NETNS.lock(); nss.get(&ns)?.sids.iter().copied().collect() };
    let mut best: Option<(u8, Sid)> = None;
    for s in sids {
        for (a, p) in addrs6_of(s) {
            if a[0] == 0xfe && a[1] == 0x80 { continue; }
            let full = (p / 8) as usize;
            let ok = dst[..full] == a[..full] && (p % 8 == 0 || (dst[full] ^ a[full]) >> (8 - p % 8) == 0);
            if ok && best.map_or(true, |b| p > b.0) { best = Some((p, s)); }
        }
    }
    if best.is_none() {
        // No on-link prefix: the longest static gateway route (a default route included).
        let v = VIFS.lock();
        for s in sids_of(ns) {
            let Some(x) = v[s as usize].as_ref() else { continue };
            for &(d, p, _, _) in &x.routes6 {
                if pfx_match(&d, &dst, p) && best.map_or(true, |b| p > b.0) { best = Some((p, s)); }
            }
        }
    }
    best.map(|b| b.1)
}

fn sids_of(ns: u16) -> Vec<Sid> { NETNS.lock().get(&ns).map_or(Vec::new(), |st| st.sids.iter().copied().collect()) }

fn pfx_match(net: &[u8; 16], a: &[u8; 16], p: u8) -> bool {
    let full = (p / 8) as usize;
    net[..full] == a[..full] && (p % 8 == 0 || (net[full] ^ a[full]) >> (8 - p % 8) == 0)
}

/// Apply an IPv6 gateway-route write to a vif of `ns`; None when `oif`/gateway names no vif of ours.
fn rtnl_route6(ns: u16, op: RtOp) -> Option<i32> {
    let sid;
    {
        let (add, dst, plen, gw, oif, metric, excl, replace) = match &op {
            RtOp::AddRoute6 { dst, plen, gw, oif, metric, excl, replace } => (true, *dst, *plen, *gw, *oif, Some(*metric), *excl, *replace),
            RtOp::DelRoute6 { dst, plen, gw, oif, metric } => (false, *dst, *plen, *gw, *oif, *metric, false, false),
            _ => return None,
        };
        let sids = sids_of(ns);
        let mut v = VIFS.lock();
        let by_oif = if oif != 0 { sids.iter().copied().find(|&s| v[s as usize].as_ref().map_or(false, |x| x.ifindex == oif && is_tun_kind(x.kind))) } else { None };
        if oif != 0 && by_oif.is_none() { return None; }
        let sd = match (by_oif, gw) {
            (Some(s), _) => s,
            (None, Some(g)) => {
                let hit = sids.iter().copied().find(|&s| v[s as usize].as_ref().map_or(false, |x| is_tun_kind(x.kind)
                    && x.addrs6.iter().any(|&(a, p)| !crate::ip6::is_link_local(&g) && pfx_match(&a, &g, p))));
                match hit { Some(s) => s, None => return Some(if crate::ip6::is_link_local(&g) { 22 } else { 101 }) }
            }
            (None, None) => return None,
        };
        sid = sd;
        let x = v[sd as usize].as_mut()?;
        if add {
            let Some(g) = gw else { return Some(95) }; // only gateway routes; connected ones come from addresses
            if !crate::ip6::is_link_local(&g) && !x.addrs6.iter().any(|&(a, p)| pfx_match(&a, &g, p)) { return Some(101); }
            let m = metric.unwrap_or(1024);
            if let Some(i) = x.routes6.iter().position(|r| r.0 == dst && r.1 == plen && r.3 == m) {
                if replace && !excl { x.routes6.remove(i); } else { return Some(17); }
            }
            if x.routes6.len() >= 32 { return Some(105); }
            x.routes6.push((dst, plen, g, m));
        } else {
            let Some(i) = x.routes6.iter().position(|r| r.0 == dst && r.1 == plen && gw.map_or(true, |g| r.2 == g) && metric.map_or(true, |m| r.3 == m)) else { return Some(3) };
            x.routes6.remove(i);
        }
    }
    resync_routes(ns, sid);
    drivers::netdev::kick();
    Some(0)
}

fn resync_routes(ns: u16, sid: Sid) {
    let routes = NETNS.lock().get(&ns).map_or(Vec::new(), |st| st.routes.clone());
    sync_routes(sid, &routes);
}

/// The sid of the vif of `ns` with interface index `index`.
pub(crate) fn sid_by_ifindex(ns: u16, index: u32) -> Option<Sid> {
    let nss = NETNS.lock();
    let v = VIFS.lock();
    sid_of_index(nss.get(&ns)?, &v, index)
}

fn rtnl_addr6(ns: u16, op: RtOp) -> Option<i32> {
    let (index, addr, prefix, add, excl) = match op {
        RtOp::AddAddr6 { index, addr, prefix, excl, .. } => (index, addr, prefix, true, excl),
        RtOp::DelAddr6 { index, addr, prefix } => (index, addr, prefix, false, false),
        _ => return None,
    };
    let sid = sid_by_ifindex(ns, index)?;
    {
        let mut v = VIFS.lock();
        let x = v[sid as usize].as_mut()?;
        if add {
            if addr == [0; 16] || addr[0] == 0xff { return Some(22); }
            if x.addrs6.iter().any(|&(a, _)| a == addr) { return Some(if excl { 17 } else { 0 }); }
            if x.addrs6.len() >= 5 { return Some(105); }
            x.addrs6.push((addr, prefix));
        } else {
            let Some(i) = x.addrs6.iter().position(|&(a, p)| a == addr && (prefix == 0 || p == prefix)) else { return Some(99) };
            x.addrs6.remove(i);
        }
    }
    resync(sid);
    drivers::netdev::kick();
    Some(0)
}

/// Rebuild the stack's gateway routes from the namespace's route list.
fn sync_routes(sid: Sid, routes: &[vifsel::Route]) {
    if let Some(s) = stack_for(sid).as_mut() {
        s.interface.routes_mut().update(|t| {
            t.clear();
            for r in routes.iter().filter(|r| r.oif == sid) {
                let Some(g) = r.gw else { continue };
                let _ = t.push(smoltcp::iface::Route {
                    cidr: IpCidr::new(IpAddress::v4(r.dst[0], r.dst[1], r.dst[2], r.dst[3]), r.prefix),
                    via_router: IpAddress::v4(g[0], g[1], g[2], g[3]),
                    preferred_until: None, expires_at: None,
                });
            }
            let r6: Vec<_> = VIFS.lock()[sid as usize].as_ref().map_or(Vec::new(), |x| x.routes6.clone());
            for (d, p, g, _) in r6 {
                let _ = t.push(smoltcp::iface::Route {
                    cidr: IpCidr::new(IpAddress::Ipv6(smoltcp::wire::Ipv6Address(d)), p),
                    via_router: IpAddress::Ipv6(smoltcp::wire::Ipv6Address(g)),
                    preferred_until: None, expires_at: None,
                });
            }
        });
    }
}

fn sid_of_index(st: &NetNsState, v: &vifsel::VifTable, index: u32) -> Option<Sid> {
    st.sids.iter().copied().find(|&s| v[s as usize].as_ref().map_or(false, |x| is_tun_kind(x.kind) && x.ifindex == index))
}

/// Apply a parsed rtnetlink write to a tun/tap of `ns`. 0 or a positive errno.
pub(crate) fn rtnl_apply(ns: u16, op: RtOp) -> i32 {
    if !sched::caps::ns_capable_in(sched::ns::owner_userns(1, ns), CAP_NET_ADMIN) { return 1; }
    match op {
        RtOp::AddAddr6 { .. } | RtOp::DelAddr6 { .. } if sid_by_ifindex(ns, match &op { RtOp::AddAddr6 { index, .. } | RtOp::DelAddr6 { index, .. } => *index, _ => 0 }).is_some() => rtnl_addr6(ns, op).unwrap_or(19),
        RtOp::AddRoute6 { .. } | RtOp::DelRoute6 { .. } if ns != 0 => rtnl_route6(ns, op).unwrap_or(19),
        RtOp::AddAddr6 { .. } | RtOp::DelAddr6 { .. } | RtOp::AddRoute6 { .. } | RtOp::DelRoute6 { .. } => crate::ip6::apply(ns, op),
        RtOp::SetLink { index, up, mtu, mac, name, master, ns: nsel } => {
            let sid;
            {
                let nss = NETNS.lock();
                let mut v = VIFS.lock();
                let Some(st) = nss.get(&ns) else { return 19 };
                let Some(sd) = sid_of_index(st, &v, index) else { return 19 };
                sid = sd;
                if let Some(m) = mtu { if !(68..=65535).contains(&m) { return 22; } }
                if let Some(n) = name.as_ref() {
                    if n.is_empty() || n.len() > 15 { return 22; }
                    if v[sid as usize].as_ref().map_or(false, |x| x.admin_up) { return 16; } // EBUSY
                    let dup = st.sids.iter().any(|&o| o != sid && v[o as usize].as_ref()
                        .map_or(false, |x| &x.name[..name_len(&x.name)] == n.as_bytes()));
                    if dup { return 17; }
                }
                let x = v[sid as usize].as_mut().unwrap();
                if let Some(m) = mtu { x.mtu = m; }
                if let Some(m) = mac { x.mac = m; }
                if let Some(n) = name.as_ref() { x.name = [0; 16]; x.name[..n.len()].copy_from_slice(n.as_bytes()); }
                if let Some(u) = up { x.admin_up = u; if !u { x.rxq.clear(); x.rxsrc.clear(); } }
            }
            if let Some(m) = mac {
                if let Some(s) = stack_for(sid).as_mut() {
                    s.interface.set_hardware_addr(HardwareAddress::Ethernet(EthernetAddress(m)));
                }
            }
            if let Some(m) = master {
                let e = if m == 0 { release(sid) } else { enslave(ns, sid, m) };
                if e != 0 { return e; }
            }
            // IFLA_NET_NS_FD / IFLA_NET_NS_PID: move the link to another netns
            // (Linux `dev_change_net_namespace`: leaves its bridge, goes down,
            // loses its addresses, gets a new ifindex there).
            if !matches!(nsel, rtnetlink::PeerNs::Same) {
                let tns = match resolve_peer_ns(&nsel, ns) { Ok(n) => n, Err(e) => return e };
                if tns != ns {
                    let e = move_link(ns, sid, tns);
                    if e != 0 { return e; }
                }
            }
            resync(sid);
            drivers::netdev::kick();
            0
        }
        RtOp::NewVeth { name, mtu, mac, up, master, peer } => veth_create(ns, name, mtu, mac, up, master, peer),
        RtOp::NewBridge { name, mtu, mac, up, opts } => bridge_create(ns, name, mtu, mac, up, opts),
        RtOp::DelLink { index } => veth_delete(ns, index),
        RtOp::AddAddr { index, addr, prefix, excl } => {
            let (sid, addrs);
            {
                let nss = NETNS.lock();
                let mut v = VIFS.lock();
                let Some(st) = nss.get(&ns) else { return 19 };
                let Some(sd) = sid_of_index(st, &v, index) else { return 19 };
                sid = sd;
                let x = v[sid as usize].as_mut().unwrap();
                if x.addrs.iter().any(|&(a, _)| a == addr) { return if excl { 17 } else { 0 }; }
                if x.addrs.len() >= 2 { return 105; } // ENOBUFS: smoltcp keeps two addresses
                x.addrs.push((addr, prefix));
                addrs = x.addrs.clone();
            }
            sync_addrs(sid, &addrs);
            drivers::netdev::kick();
            0
        }
        RtOp::DelAddr { index, addr, prefix } => {
            let (sid, addrs);
            {
                let nss = NETNS.lock();
                let mut v = VIFS.lock();
                let Some(st) = nss.get(&ns) else { return 19 };
                let Some(sd) = sid_of_index(st, &v, index) else { return 19 };
                sid = sd;
                let x = v[sid as usize].as_mut().unwrap();
                let Some(i) = x.addrs.iter().position(|&(a, p)| a == addr && p == prefix) else { return 99 };
                x.addrs.remove(i);
                addrs = x.addrs.clone();
            }
            sync_addrs(sid, &addrs);
            0
        }
        RtOp::AddRoute { dst, prefix, gw, oif, metric, excl: _, replace } => {
            let (sid, routes);
            {
                let mut nss = NETNS.lock();
                let v = VIFS.lock();
                let Some(st) = nss.get_mut(&ns) else { return 3 }; // no tap here: ESRCH-like
                let on_link = |s: Sid, g: [u8; 4]| v[s as usize].as_ref()
                    .map_or(false, |x| x.addrs.iter().any(|&(a, p)| net_of(a, p) == net_of(g, p)));
                let sd = if oif != 0 {
                    match sid_of_index(st, &v, oif) { Some(s) => s, None => return 19 }
                } else if let Some(g) = gw {
                    match st.sids.iter().copied().find(|&s| Some(s) != st.lo && on_link(s, g)) { Some(s) => s, None => return 101 }
                } else { return 22 };
                if let Some(g) = gw { if !on_link(sd, g) { return 101; } }
                sid = sd;
                let same = |r: &vifsel::Route| r.dst == dst && r.prefix == prefix && r.metric == metric;
                if st.routes.iter().any(same) {
                    if !replace { return 17; }
                    st.routes.retain(|r| !same(r));
                }
                if st.routes.len() >= 64 { return 105; }
                st.routes.push(vifsel::Route { dst, prefix, gw, oif: sid, metric });
                routes = st.routes.clone();
            }
            sync_routes(sid, &routes);
            drivers::netdev::kick();
            0
        }
        RtOp::DelRoute { dst, prefix, gw, oif, metric } => {
            let (sid, routes);
            {
                let mut nss = NETNS.lock();
                let v = VIFS.lock();
                let Some(st) = nss.get_mut(&ns) else { return 3 };
                let want_oif = if oif != 0 { sid_of_index(st, &v, oif) } else { None };
                if oif != 0 && want_oif.is_none() { return 19; }
                let pos = st.routes.iter().position(|r| r.dst == dst && r.prefix == prefix
                    && gw.map_or(true, |g| r.gw == Some(g)) && want_oif.map_or(true, |o| r.oif == o)
                    && metric.map_or(true, |m| r.metric == m));
                let Some(i) = pos else { return 3 }; // ESRCH
                sid = st.routes.remove(i).oif;
                routes = st.routes.clone();
            }
            sync_routes(sid, &routes);
            0
        }
    }
}

/// The routes `ns` exports from its tun/tap interfaces: connected routes of
/// up ones plus the explicit routes.
pub(crate) fn routes(ns: u16) -> Vec<rtnetlink::RouteEnt> {
    let nss = NETNS.lock();
    let v = VIFS.lock();
    let mut out = Vec::new();
    let Some(st) = nss.get(&ns) else { return out };
    for &s in st.sids.iter() {
        let Some(x) = v[s as usize].as_ref() else { continue };
        if !is_tun_kind(x.kind) || !x.admin_up { continue; }
        for &(a, p) in x.addrs.iter() {
            let n = net_of(a, p).to_be_bytes();
            out.push(rtnetlink::RouteEnt { dst: n, prefix: p, gw: None, oif: x.ifindex, prefsrc: Some(a) });
        }
    }
    for r in st.routes.iter() {
        let Some(x) = v[r.oif as usize].as_ref() else { continue };
        if !x.admin_up { continue; }
        out.push(rtnetlink::RouteEnt { dst: r.dst, prefix: r.prefix, gw: r.gw, oif: x.ifindex, prefsrc: None });
    }
    out
}

// ── veth pairs ───────────────────────────────────────────────────────────────

/// The network namespace a peer end goes to (`IFLA_NET_NS_FD` / `IFLA_NET_NS_PID`).
fn resolve_peer_ns(sel: &rtnetlink::PeerNs, cur: u16) -> Result<u16, i32> {
    match *sel {
        rtnetlink::PeerNs::Same => Ok(cur),
        rtnetlink::PeerNs::Fd(fd) => match vfs::nsfd_target(sched::current_pid(), fd as usize) {
            Ok((2, id)) => Ok(id),
            Ok(_) => Err(22),
            Err(e) => Err(if e < 0 { -e } else { 9 }),
        },
        rtnetlink::PeerNs::Pid(p) => {
            let k = sched::pidns::from_cur(p).map_err(|_| 3)?;
            Ok(sched::ns::netns_of(k))
        }
    }
}

/// A free interface name in a namespace for `base` (`%d` is expanded).
fn pick_name(st: &NetNsState, v: &vifsel::VifTable, base: &[u8], also: Option<&[u8]>) -> Result<[u8; 16], i32> {
    let taken = |nm: &[u8]| nm == b"lo" || also == Some(nm)
        || st.sids.iter().any(|&q| v[q as usize].as_ref().map_or(false, |x| x.name[..name_len(&x.name)] == *nm));
    let mut fin = [0u8; 16];
    if let Some(p) = base.windows(2).position(|w| w == b"%d") {
        for i in 0..1000u32 {
            let mut s = Vec::new();
            s.extend_from_slice(&base[..p]);
            s.extend_from_slice(alloc::format!("{}", i).as_bytes());
            s.extend_from_slice(&base[p + 2..]);
            if s.len() > 15 { return Err(22); }
            if !taken(&s) { fin[..s.len()].copy_from_slice(&s); return Ok(fin); }
        }
        return Err(17);
    }
    if base.is_empty() || base.len() > 15 || base.iter().any(|&c| c == b'/' || c == b' ') { return Err(22); }
    if taken(base) { return Err(17); }
    fin[..base.len()].copy_from_slice(base);
    Ok(fin)
}

/// Move the tun/veth/bridge `sid` of `ns` to netns `tns`.
fn move_link(ns: u16, sid: Sid, tns: u16) -> i32 {
    if !sched::caps::ns_capable_in(sched::ns::owner_userns(1, tns), CAP_NET_ADMIN) { return 1; }
    crate::vif::claim(tns);
    let _ = release(sid); // a bridge port leaves its bridge
    {
        let mut nss = NETNS.lock();
        let mut v = VIFS.lock();
        nss.entry(tns).or_insert_with(|| NetNsState::new(tns));
        if nss[&tns].sids.len() + 1 > ns_sid_cap(tns) { return 23; }
        let Some(cur) = v[sid as usize].as_ref().map(|x| x.name) else { return 19 };
        let nm = &cur[..name_len(&cur)];
        let clash = nm == b"lo" || nss[&tns].sids.iter().any(|&q| v[q as usize].as_ref()
            .map_or(false, |x| &x.name[..name_len(&x.name)] == nm));
        let fin = if clash {
            match pick_name(&nss[&tns], &v, b"eth%d", None) { Ok(n) => n, Err(e) => return e }
        } else { cur };
        let ifindex = { let st = nss.get_mut(&tns).unwrap(); let i = st.next_ifindex; st.next_ifindex += 1; i };
        if let Some(st) = nss.get_mut(&ns) { st.sids.retain(|&q| q != sid); }
        nss.get_mut(&tns).unwrap().sids.push(sid);
        let x = v[sid as usize].as_mut().unwrap();
        x.netns = tns; x.ifindex = ifindex; x.name = fin;
        x.admin_up = false; x.addrs.clear(); x.rxq.clear(); x.rxsrc.clear();
    }
    sync_addrs(sid, &[]);
    0
}

fn veth_create(ns: u16, name: Option<alloc::string::String>, mtu: Option<u32>, mac: Option<[u8; 6]>, up: bool,
               master: Option<u32>, peer: rtnetlink::VethPeer) -> i32 {
    let pns = match resolve_peer_ns(&peer.ns, ns) { Ok(n) => n, Err(e) => return e };
    if pns != ns && !sched::caps::ns_capable_in(sched::ns::owner_userns(1, pns), CAP_NET_ADMIN) { return 1; }
    for m in [mtu, peer.mtu].into_iter().flatten() { if !(68..=65535).contains(&m) { return 22; } }
    crate::vif::claim(ns);
    crate::vif::claim(pns);
    let (sa, sb, ma, mb) = {
        let mut nss = NETNS.lock();
        let mut v = VIFS.lock();
        nss.entry(ns).or_insert_with(|| NetNsState::new(ns));
        nss.entry(pns).or_insert_with(|| NetNsState::new(pns));
        let need_a = 1 + (ns == pns) as usize;
        if nss[&ns].sids.len() + need_a > ns_sid_cap(ns) || nss[&pns].sids.len() + 1 > ns_sid_cap(pns) { return 23; }
        let na = match pick_name(&nss[&ns], &v, name.as_deref().unwrap_or("veth%d").as_bytes(), None) { Ok(n) => n, Err(e) => return e };
        let nb = match pick_name(&nss[&pns], &v, peer.name.as_deref().unwrap_or("veth%d").as_bytes(),
                                 if ns == pns { Some(&na[..name_len(&na)]) } else { None }) { Ok(n) => n, Err(e) => return e };
        let mut free = (FIRST_DYN as usize..MAX_STACKS).filter(|&i| v[i].is_none());
        let (Some(sa), Some(sb)) = (free.next(), free.next()) else { return 23 };
        let ma = mac.unwrap_or([0x02, 0x00, 0x5e, 0x11, sa as u8, 0x01]);
        let mb = peer.mac.unwrap_or([0x02, 0x00, 0x5e, 0x11, sb as u8, 0x02]);
        let ia = { let st = nss.get_mut(&ns).unwrap(); let i = st.next_ifindex; st.next_ifindex += 1; i };
        let ib = { let st = nss.get_mut(&pns).unwrap(); let i = st.next_ifindex; st.next_ifindex += 1; i };
        let mk = |ns, ifindex, name, up, mtu: Option<u32>, mac, peer| Vif {
            netns: ns, ifindex, name, kind: VifKind::Veth, admin_up: up, mtu: mtu.unwrap_or(1500), mac,
            addrs: Vec::new(), addrs6: Vec::new(), routes6: Vec::new(), rxq: VecDeque::new(), rxsrc: VecDeque::new(), tun: NO_TUN, pending: false, peer, master: NO_PEER,
        };
        v[sa] = Some(mk(ns, ia, na, up, mtu, ma, sb as Sid));
        v[sb] = Some(mk(pns, ib, nb, peer.up, peer.mtu, mb, sa as Sid));
        nss.get_mut(&ns).unwrap().sids.push(sa as Sid);
        nss.get_mut(&pns).unwrap().sids.push(sb as Sid);
        (sa as Sid, sb as Sid, ma, mb)
    };
    make_stack(sa, ma);
    make_stack(sb, mb);
    // IFLA_MASTER at create: the main end joins a bridge of `ns`, the peer one of `pns`.
    for (n, s, m) in [(ns, sa, master), (pns, sb, peer.master)] {
        let Some(m) = m.filter(|&m| m != 0) else { continue };
        let e = enslave(n, s, m);
        if e != 0 { crate::vif::free_vifs(&[(ns, sa), (pns, sb)]); return e; }
    }
    drivers::netdev::kick();
    0
}

/// Enslave veth end `sid` of `ns` to the bridge with ifindex `br_index` (same namespace).
/// The port gives up its own stack: its frames go to the switch (`l2`).
fn enslave(ns: u16, sid: Sid, br_index: u32) -> i32 {
    {
        let nss = NETNS.lock();
        let mut v = VIFS.lock();
        let Some(st) = nss.get(&ns) else { return 19 };
        let Some(b) = st.sids.iter().copied().find(|&s| v[s as usize].as_ref().map_or(false, |x| x.ifindex == br_index)) else { return 19 };
        if v[b as usize].as_ref().map_or(true, |x| x.kind != VifKind::Bridge) { return 22; }
        let Some(x) = v[sid as usize].as_mut() else { return 19 };
        if x.kind != VifKind::Veth { return 22; }
        if x.master == b { return 0; }
        if x.master != NO_PEER { return 16; } // EBUSY: enslaved to another master
        x.master = b;
        x.addrs.clear();
        x.rxq.clear(); x.rxsrc.clear();
    }
    {
        let mut nss = NETNS.lock();
        if let Some(st) = nss.get_mut(&ns) { st.routes.retain(|r| r.oif != sid); }
    }
    crate::l2::forget(sid);
    // No table lock is held: the stack lock is taken alone.
    *stack_for(sid) = None;
    drivers::netdev::kick();
    0
}

/// Release a port from its bridge: it owns a stack again.
fn release(sid: Sid) -> i32 {
    let mac = {
        let mut v = VIFS.lock();
        let Some(x) = v[sid as usize].as_mut() else { return 19 };
        if x.master == NO_PEER { return 0; }
        x.master = NO_PEER;
        x.rxq.clear(); x.rxsrc.clear();
        x.mac
    };
    crate::l2::forget(sid);
    make_stack(sid, mac);
    drivers::netdev::kick();
    0
}

fn bridge_create(ns: u16, name: Option<alloc::string::String>, mtu: Option<u32>, mac: Option<[u8; 6]>, up: bool,
                 opts: rtnetlink::BrOpts) -> i32 {
    if let Some(m) = mtu { if !(68..=65535).contains(&m) { return 22; } }
    crate::vif::claim(ns);
    let (sid, m) = {
        let mut nss = NETNS.lock();
        let mut v = VIFS.lock();
        nss.entry(ns).or_insert_with(|| NetNsState::new(ns));
        if nss[&ns].sids.len() + 1 > ns_sid_cap(ns) { return 23; }
        let nm = match pick_name(&nss[&ns], &v, name.as_deref().unwrap_or("br%d").as_bytes(), None) { Ok(n) => n, Err(e) => return e };
        let Some(sid) = (FIRST_DYN as usize..MAX_STACKS).find(|&i| v[i].is_none()) else { return 23 };
        let m = mac.unwrap_or([0x02, 0x00, 0x5e, 0x12, sid as u8, 0x01]);
        let st = nss.get_mut(&ns).unwrap();
        let ifindex = st.next_ifindex;
        st.next_ifindex += 1;
        v[sid] = Some(Vif {
            netns: ns, ifindex, name: nm, kind: VifKind::Bridge, admin_up: up, mtu: mtu.unwrap_or(1500), mac: m,
            addrs: Vec::new(), addrs6: Vec::new(), routes6: Vec::new(), rxq: VecDeque::new(), rxsrc: VecDeque::new(), tun: NO_TUN, pending: false, peer: NO_PEER, master: NO_PEER,
        });
        st.sids.push(sid as Sid);
        (sid as Sid, m)
    };
    let d = crate::l2::DEFAULT_CFG;
    crate::l2::cfg_set(sid, crate::l2::BrCfg {
        ageing: opts.ageing.unwrap_or(d.ageing), fwd_delay: opts.fwd_delay.unwrap_or(d.fwd_delay),
        stp: opts.stp.unwrap_or(d.stp), vlan: opts.vlan.unwrap_or(d.vlan),
    });
    make_stack(sid, m);
    drivers::netdev::kick();
    0
}

fn veth_delete(ns: u16, index: u32) -> i32 {
    let mut ports: Vec<Sid> = Vec::new();
    let list = {
        let nss = NETNS.lock();
        let v = VIFS.lock();
        let Some(st) = nss.get(&ns) else { return 19 };
        let Some(sid) = st.sids.iter().copied().find(|&s| v[s as usize].as_ref().map_or(false, |x| x.ifindex == index)) else { return 19 };
        let x = v[sid as usize].as_ref().unwrap();
        if x.kind == VifKind::Bridge {
            ports = (0..v.len()).filter(|&i| v[i].as_ref().map_or(false, |p| p.master == sid)).map(|i| i as Sid).collect();
            alloc::vec![(ns, sid)]
        } else {
        if x.kind != VifKind::Veth { return 95; }
        let mut l = alloc::vec![(ns, sid)];
        if let Some(p) = v.get(x.peer as usize).and_then(|p| p.as_ref()) { l.push((p.netns, x.peer)); }
        l
        }
    };
    // Ports of a deleted bridge are released, not deleted (Linux).
    for p in ports { release(p); }
    crate::vif::free_vifs(&list);
    0
}
