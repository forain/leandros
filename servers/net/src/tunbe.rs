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
use vifsel::{NetNsState, Vif, VifKind, NO_TUN, RXQ_CAP, MAX_NS_SIDS};
use iface::FIRST_DYN;

const CAP_NET_ADMIN: u32 = 12;

static BACKEND: tundev::TunBackend = tundev::TunBackend { attach, input, detach };

pub(crate) fn init() { tundev::set_backend(&BACKEND); }

fn peer_mac(mac: [u8; 6]) -> [u8; 6] { let mut m = mac; m[5] ^= 0x80; m }

fn name_len(n: &[u8; 16]) -> usize { n.iter().position(|&b| b == 0).unwrap_or(16) }

fn is_tun_kind(k: VifKind) -> bool { matches!(k, VifKind::Tap | VifKind::Tun) }

// ── hooks ────────────────────────────────────────────────────────────────────

fn attach(ns: u16, name: &[u8; 16], tap: bool, h: TunHandle) -> Result<(u32, [u8; 16]), i32> {
    let n = name_len(name);
    let kind = if tap { VifKind::Tap } else { VifKind::Tun };
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
        if st.sids.len() >= MAX_NS_SIDS { return Err(23); }
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
                            addrs: Vec::new(), rxq: VecDeque::new(), tun: h, pending: false });
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
        if let Some(x) = v[sid as usize].as_mut() { x.tun = NO_TUN; x.rxq.clear(); }
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

/// The vif as a smoltcp device for one poll: `rx` was drained from the vif's
/// queue before the stack lock was taken, `tx` is delivered after it is dropped.
pub(crate) struct TapDev { pub rx: VecDeque<Vec<u8>>, pub tx: Vec<Vec<u8>>, pub mtu: usize }
pub struct TapTx<'a>(&'a mut Vec<Vec<u8>>);

impl<'a> smoltcp::phy::TxToken for TapTx<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R where F: FnOnce(&mut [u8]) -> R {
        let mut buf = alloc::vec![0u8; len];
        let r = f(&mut buf);
        self.0.push(buf);
        r
    }
}

impl smoltcp::phy::Device for TapDev {
    type RxToken<'a> = GateRx;
    type TxToken<'a> = TapTx<'a>;

    fn receive(&mut self, _ts: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let f = self.rx.pop_front()?;
        Some((GateRx(f), TapTx(&mut self.tx)))
    }
    fn transmit(&mut self, _ts: Instant) -> Option<Self::TxToken<'_>> {
        if self.tx.len() >= 256 { None } else { Some(TapTx(&mut self.tx)) }
    }
    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        let mut caps = smoltcp::phy::DeviceCapabilities::default();
        caps.max_transmission_unit = self.mtu + 14;
        caps.medium = smoltcp::phy::Medium::Ethernet;
        caps
    }
}

impl Frames for TapDev {
    fn frame<'a>(rx: GateRx) -> Vec<u8> where Self: 'a { rx.0 }
}

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
        let (rx, mtu, h, l3, mac) = {
            let mut v = VIFS.lock();
            let Some(x) = v[sid as usize].as_mut() else { continue };
            x.pending = false;
            (core::mem::take(&mut x.rxq), x.mtu as usize, x.tun, x.kind == VifKind::Tun, x.mac)
        };
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
                if !x.admin_up { x.rxq.clear(); }
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
                if !(68..=65000).contains(&m) { return Some(-22); }
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
                mac: x.mac, mtu: x.mtu, up: x.admin_up, running: x.admin_up && x.tun != NO_TUN,
                loopback: false,
                addr: if x.admin_up { x.addrs.first().copied() } else { None },
            });
        }
    }
    out
}
