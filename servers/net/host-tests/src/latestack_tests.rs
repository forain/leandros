//! A listener set up before Wi-Fi's stack exists (lane pisshrace): sshd's
//! `listen(0.0.0.0:22)` ran before brcmf learned its MAC, so the late wifi0
//! stack had no LISTEN socket and every SYN over Wi-Fi got an RST
//! ("Connection refused") while ping worked. Real smoltcp, real `latestack`.

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, EthernetFrame, HardwareAddress, IpAddress, IpCidr, IpListenEndpoint,
                    Ipv4Address, Ipv4Packet, TcpPacket};

use crate::latestack::{self, Registry};

const ME: [u8; 6] = [0x2c, 0xcf, 0x67, 0xd8, 0xca, 0x1b];
const PEER: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
const MY_IP: [u8; 4] = [172, 16, 162, 184];
const PEER_IP: [u8; 4] = [172, 16, 1, 10];

/// rx and tx kept apart (smoltcp's Loopback would feed our RST back in).
#[derive(Default)]
struct Dev { rx: std::collections::VecDeque<Vec<u8>>, tx: Vec<Vec<u8>> }
struct Rx(Vec<u8>);
struct Tx<'a>(&'a mut Vec<Vec<u8>>);
impl RxToken for Rx { fn consume<R, F: FnOnce(&mut [u8]) -> R>(mut self, f: F) -> R { f(&mut self.0) } }
impl<'a> TxToken for Tx<'a> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut b = vec![0u8; len]; let r = f(&mut b); self.0.push(b); r
    }
}
impl Device for Dev {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx<'a>;
    fn receive(&mut self, _t: Instant) -> Option<(Rx, Tx<'_>)> { let f = self.rx.pop_front()?; Some((Rx(f), Tx(&mut self.tx))) }
    fn transmit(&mut self, _t: Instant) -> Option<Tx<'_>> { Some(Tx(&mut self.tx)) }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default(); c.medium = Medium::Ethernet; c.max_transmission_unit = 1514; c
    }
}

struct Stack { iface: Interface, dev: Dev, sockets: SocketSet<'static>, listens: Vec<(u32, SocketHandle)> }

fn new_stack(mac: [u8; 6], ip: [u8; 4]) -> Stack {
    let mut dev = Dev::default();
    let mut cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    cfg.random_seed = 7;
    let mut iface = Interface::new(cfg, &mut dev, Instant::from_millis(0));
    iface.update_ip_addrs(|a| { a.push(IpCidr::new(IpAddress::v4(ip[0], ip[1], ip[2], ip[3]), 16)).unwrap(); });
    Stack { iface, dev, sockets: SocketSet::new(vec![]), listens: vec![] }
}

/// `listen::create`: idempotent per listener id.
fn create(s: &mut Stack, lid: u32, port: u16) -> bool {
    if s.listens.iter().any(|l| l.0 == lid) { return true; }
    let mut t = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; 4096]), tcp::SocketBuffer::new(vec![0; 4096]));
    if t.listen(IpListenEndpoint { addr: None, port }).is_err() { return false; }
    let h = s.sockets.add(t);
    s.listens.push((lid, h));
    true
}

fn destroy(s: &mut Stack, lid: u32) {
    if let Some(i) = s.listens.iter().position(|l| l.0 == lid) { let (_, h) = s.listens.swap_remove(i); s.sockets.remove(h); }
}

/// A SYN from the peer to MY_IP:port, as wifi0 receives it.
fn syn(port: u16) -> Vec<u8> {
    let mut f = vec![0u8; 14 + 20 + 20];
    f[0..6].copy_from_slice(&ME);
    f[6..12].copy_from_slice(&PEER);
    f[12..14].copy_from_slice(&[0x08, 0x00]);
    let ip = &mut f[14..];
    ip[0] = 0x45; ip[2..4].copy_from_slice(&40u16.to_be_bytes()); ip[8] = 64; ip[9] = 6;
    ip[12..16].copy_from_slice(&PEER_IP); ip[16..20].copy_from_slice(&MY_IP);
    let tcp = &mut ip[20..];
    tcp[0..2].copy_from_slice(&50000u16.to_be_bytes()); tcp[2..4].copy_from_slice(&port.to_be_bytes());
    tcp[4..8].copy_from_slice(&1000u32.to_be_bytes()); tcp[12] = 5 << 4; tcp[13] = 0x02;
    tcp[14..16].copy_from_slice(&65535u16.to_be_bytes());
    let (src, dst) = (IpAddress::Ipv4(Ipv4Address::from_bytes(&PEER_IP)), IpAddress::Ipv4(Ipv4Address::from_bytes(&MY_IP)));
    TcpPacket::new_unchecked(&mut f[34..]).fill_checksum(&src, &dst);
    Ipv4Packet::new_unchecked(&mut f[14..]).fill_checksum();
    f
}

/// Deliver `frame` to the stack; true when it answered with an RST.
fn deliver(s: &mut Stack, frame: Vec<u8>) -> bool {
    // Teach the neighbor cache the peer's MAC so a SYN-ACK can leave at once.
    let mut arp = vec![0u8; 42];
    arp[0..6].copy_from_slice(&ME); arp[6..12].copy_from_slice(&PEER); arp[12..14].copy_from_slice(&[0x08, 0x06]);
    arp[14..22].copy_from_slice(&[0, 1, 8, 0, 6, 4, 0, 2]);
    arp[22..28].copy_from_slice(&PEER); arp[28..32].copy_from_slice(&PEER_IP);
    arp[32..38].copy_from_slice(&ME); arp[38..42].copy_from_slice(&MY_IP);
    s.dev.rx.push_back(arp);
    s.dev.rx.push_back(frame);
    s.iface.poll(Instant::from_millis(1), &mut s.dev, &mut s.sockets);
    let mut rst = false;
    for b in s.dev.tx.drain(..) {
        let e = EthernetFrame::new_unchecked(&b[..]);
        if e.ethertype() != smoltcp::wire::EthernetProtocol::Ipv4 { continue; }
        let ip = Ipv4Packet::new_unchecked(e.payload());
        if ip.next_header() != smoltcp::wire::IpProtocol::Tcp { continue; }
        if TcpPacket::new_unchecked(ip.payload()).rst() { rst = true; }
    }
    rst
}

fn handshaking(s: &Stack, lid: u32) -> bool {
    s.listens.iter().any(|l| l.0 == lid && s.sockets.get::<tcp::Socket>(l.1).state() == tcp::State::SynReceived)
}

/// The boot order of the failing Pi runs: lo up, sshd listens, wifi0 built.
fn boot(adopt: bool) -> (Stack, u32) {
    let mut reg: Registry<u16> = Registry::new();
    let mut lo = new_stack([0; 6], [127, 0, 0, 1]);
    let lid = 1;
    reg.register(lid, 22);
    assert!(create(&mut lo, lid, 22));
    let mut wifi = new_stack(ME, MY_IP);
    if adopt {
        let snap = reg.snapshot();
        let n = latestack::adopt(&snap, |_| true, |l, p| create(&mut wifi, l, *p), |l| reg.contains(l), |_| unreachable!());
        assert_eq!(n, 1);
    }
    (wifi, lid)
}

#[test]
fn legacy_late_stack_refuses_ssh() {
    let (mut wifi, lid) = boot(false);
    assert!(deliver(&mut wifi, syn(22)), "no listener on the late stack: RST (Connection refused)");
    assert!(!handshaking(&wifi, lid));
}

#[test]
fn late_stack_adopts_listener_and_answers_syn() {
    let (mut wifi, lid) = boot(true);
    assert!(!deliver(&mut wifi, syn(22)), "SYN on the late stack must not be refused");
    assert!(handshaking(&wifi, lid), "the adopted LISTEN socket took the SYN");
}

#[test]
fn listen_racing_stack_creation_is_on_the_stack_once() {
    // listen() registered, then saw the new stack too: adopt must not add a
    // second LISTEN socket for the same listener.
    let mut reg: Registry<u16> = Registry::new();
    let mut wifi = new_stack(ME, MY_IP);
    reg.register(5, 22);
    assert!(create(&mut wifi, 5, 22));
    let snap = reg.snapshot();
    latestack::adopt(&snap, |_| true, |l, p| create(&mut wifi, l, *p), |l| reg.contains(l), |_| unreachable!());
    assert_eq!(wifi.listens.len(), 1);
    assert_eq!(wifi.sockets.iter().count(), 1);
}

#[test]
fn close_racing_adopt_leaves_nothing_behind() {
    // close() unregistered between adopt's snapshot and its create.
    let mut reg: Registry<u16> = Registry::new();
    reg.register(9, 22);
    let snap = reg.snapshot();
    reg.unregister(9);
    let mut wifi = new_stack(ME, MY_IP);
    let w = std::cell::RefCell::new(&mut wifi);
    let n = latestack::adopt(&snap, |_| true, |l, p| create(&mut w.borrow_mut(), l, *p), |l| reg.contains(l),
                             |l| destroy(&mut w.borrow_mut(), l));
    assert_eq!(n, 0);
    assert!(wifi.listens.is_empty());
    assert_eq!(wifi.sockets.iter().count(), 0);
    assert!(deliver(&mut wifi, syn(22)));
}

#[test]
fn explicit_address_listeners_are_filtered_by_wants() {
    let mut reg: Registry<u16> = Registry::new();
    reg.register(1, 22);
    reg.register(2, 8080);
    let mut wifi = new_stack(ME, MY_IP);
    let snap = reg.snapshot();
    let n = latestack::adopt(&snap, |p| *p == 22, |l, p| create(&mut wifi, l, *p), |l| reg.contains(l), |_| {});
    assert_eq!(n, 1);
    assert!(deliver(&mut wifi, syn(8080)));
}
