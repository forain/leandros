//! LEANDROS: tests for the LeandrOS changes to UDP demultiplexing, ICMP
//! errors and IPv4 fragmentation (see LEANDROS-PATCH.md).

use super::*;
use crate::iface::SocketHandle;
use crate::socket::{tcp, udp};

fn udp_socket(cap: usize) -> udp::Socket<'static> {
    udp::Socket::new(
        udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 64], vec![0; cap]),
        udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 64], vec![0; cap]),
    )
}

#[test]
fn test_udp_port_unreachable_round_trip() {
    let (mut iface, mut sockets, mut device) = setup(Medium::Ethernet);
    let mut c = udp_socket(1024);
    c.bind(4000).unwrap();
    c.set_peer(Some(IpEndpoint::new(IpAddress::Ipv4(ME), 9)));
    let c = sockets.add(c);
    sockets
        .get_mut::<udp::Socket>(c)
        .send_slice(b"x", (IpAddress::Ipv4(ME), 9))
        .unwrap();
    for i in 0..10 {
        iface.poll(Instant::from_millis(i * 10), &mut device, &mut sockets);
    }
    assert_eq!(sockets.get_mut::<udp::Socket>(c).take_error(), 111);
}

const ME: Ipv4Address = Ipv4Address([192, 168, 1, 1]);
const PEER: Ipv4Address = Ipv4Address([192, 168, 1, 2]);

/// Hand `payload` from src:sport to dst:dport to the UDP demultiplexer.
fn deliver(
    iface: &mut Interface,
    sockets: &mut SocketSet,
    src: Ipv4Address,
    sport: u16,
    dst: Ipv4Address,
    dport: u16,
    payload: &[u8],
) {
    let udp_repr = UdpRepr {
        src_port: sport,
        dst_port: dport,
    };
    let ip_repr = IpRepr::Ipv4(Ipv4Repr {
        src_addr: src,
        dst_addr: dst,
        next_header: IpProtocol::Udp,
        payload_len: udp_repr.header_len() + payload.len(),
        hop_limit: 64,
    });
    let _ = iface.inner.process_udp(
        sockets,
        PacketMeta::default(),
        ip_repr,
        udp_repr,
        false,
        payload,
        &[],
    );
}

fn count(sockets: &mut SocketSet, h: SocketHandle) -> usize {
    let s = sockets.get_mut::<udp::Socket>(h);
    let mut n = 0;
    while s.recv().is_ok() {
        n += 1;
    }
    n
}

#[test]
fn test_udp_broadcast_and_multicast_reach_every_socket() {
    let (mut iface, mut sockets, _device) = setup(Medium::Ethernet);
    let mut a = udp_socket(256);
    a.bind(5000).unwrap();
    let a = sockets.add(a);
    let mut b = udp_socket(256);
    b.bind(5000).unwrap();
    let b = sockets.add(b);
    // Bound to the unicast address: hears neither (Linux).
    let mut c = udp_socket(256);
    c.bind((IpAddress::Ipv4(ME), 5000)).unwrap();
    let c = sockets.add(c);

    deliver(&mut iface, &mut sockets, PEER, 7, Ipv4Address::BROADCAST, 5000, b"bc");
    deliver(&mut iface, &mut sockets, PEER, 7, Ipv4Address([192, 168, 1, 255]), 5000, b"bc");
    deliver(&mut iface, &mut sockets, PEER, 7, Ipv4Address([224, 0, 0, 251]), 5000, b"mc");
    assert_eq!(count(&mut sockets, a), 3);
    assert_eq!(count(&mut sockets, b), 3);
    assert_eq!(count(&mut sockets, c), 0);
}

#[test]
fn test_udp_unicast_best_match() {
    let (mut iface, mut sockets, _device) = setup(Medium::Ethernet);
    let mut old = udp_socket(256);
    old.bind(5000).unwrap();
    let old = sockets.add(old);
    let mut new = udp_socket(256);
    new.bind(5000).unwrap();
    let new = sockets.add(new);
    // Newest bound of two equal matches (Linux puts it first in the chain).
    deliver(&mut iface, &mut sockets, PEER, 7, ME, 5000, b"u");
    assert_eq!((count(&mut sockets, old), count(&mut sockets, new)), (0, 1));
    // A socket bound to the address beats the wildcard ones.
    let mut spec = udp_socket(256);
    spec.bind((IpAddress::Ipv4(ME), 5000)).unwrap();
    let spec = sockets.add(spec);
    deliver(&mut iface, &mut sockets, PEER, 7, ME, 5000, b"u");
    assert_eq!(count(&mut sockets, spec), 1);
    // A connected one beats both for its peer, and only hears its peer.
    sockets
        .get_mut::<udp::Socket>(old)
        .set_peer(Some(IpEndpoint::new(IpAddress::Ipv4(PEER), 7)));
    deliver(&mut iface, &mut sockets, PEER, 7, ME, 5000, b"u");
    deliver(&mut iface, &mut sockets, PEER, 8, ME, 5000, b"u");
    assert_eq!(count(&mut sockets, old), 1);
    assert_eq!(count(&mut sockets, spec), 1);
    assert_eq!(count(&mut sockets, new), 0);
}

#[test]
fn test_udp_reuseport_spreads_flows() {
    let (mut iface, mut sockets, _device) = setup(Medium::Ethernet);
    let mut hs = vec![];
    for _ in 0..2 {
        let mut s = udp_socket(64 * 64);
        s.set_reuseport(true);
        s.bind(5000).unwrap();
        hs.push(sockets.add(s));
    }
    for sport in 1000..1032 {
        // Twice per flow: a flow always lands on the same socket.
        deliver(&mut iface, &mut sockets, PEER, sport, ME, 5000, b"r");
        deliver(&mut iface, &mut sockets, PEER, sport, ME, 5000, b"r");
    }
    let (n0, n1) = (count(&mut sockets, hs[0]), count(&mut sockets, hs[1]));
    assert_eq!(n0 + n1, 64);
    assert!(n0 > 0 && n1 > 0, "{n0} {n1}");
    assert!(n0 % 2 == 0 && n1 % 2 == 0);
}

/// An ICMPv4 error of `ty`/`code` about a packet ME -> PEER of `proto` whose
/// payload begins with `head`, processed by the interface.
fn icmp_error(
    iface: &mut Interface,
    sockets: &mut SocketSet,
    reason: Icmpv4DstUnreachable,
    proto: IpProtocol,
    head: &[u8],
) {
    let quoted = Ipv4Repr {
        src_addr: ME,
        dst_addr: PEER,
        next_header: proto,
        payload_len: 1000, // longer than the quote, as in real errors
        hop_limit: 64,
    };
    let repr = Icmpv4Repr::DstUnreachable {
        reason,
        header: quoted,
        data: head,
    };
    let mut bytes = vec![0u8; repr.buffer_len()];
    repr.emit(
        &mut Icmpv4Packet::new_unchecked(&mut bytes[..]),
        &ChecksumCapabilities::default(),
    );
    let outer = IpRepr::Ipv4(Ipv4Repr {
        src_addr: PEER,
        dst_addr: ME,
        next_header: IpProtocol::Icmp,
        payload_len: bytes.len(),
        hop_limit: 64,
    });
    let _ = iface.inner.process_icmpv4(sockets, outer, &bytes);
}

#[test]
fn test_udp_port_unreachable_sets_error_on_connected_socket() {
    let (mut iface, mut sockets, _device) = setup(Medium::Ethernet);
    let mut conn = udp_socket(256);
    conn.bind(5000).unwrap();
    conn.set_peer(Some(IpEndpoint::new(IpAddress::Ipv4(PEER), 9)));
    let conn = sockets.add(conn);
    let mut unconn = udp_socket(256);
    unconn.bind(5001).unwrap();
    let unconn = sockets.add(unconn);
    let head = |sport: u16| {
        let mut h = [0u8; 8];
        h[0..2].copy_from_slice(&sport.to_be_bytes());
        h[2..4].copy_from_slice(&9u16.to_be_bytes());
        h
    };
    icmp_error(&mut iface, &mut sockets, Icmpv4DstUnreachable::PortUnreachable, IpProtocol::Udp, &head(5000));
    icmp_error(&mut iface, &mut sockets, Icmpv4DstUnreachable::PortUnreachable, IpProtocol::Udp, &head(5001));
    assert_eq!(sockets.get_mut::<udp::Socket>(conn).take_error(), 111);
    assert_eq!(sockets.get_mut::<udp::Socket>(conn).take_error(), 0);
    assert_eq!(sockets.get_mut::<udp::Socket>(unconn).error(), 0);
    // Not fatal for UDP (Linux): host unreachable leaves it alone.
    icmp_error(&mut iface, &mut sockets, Icmpv4DstUnreachable::HostUnreachable, IpProtocol::Udp, &head(5000));
    assert_eq!(sockets.get_mut::<udp::Socket>(conn).error(), 0);
}

#[test]
fn test_tcp_syn_sent_ends_on_icmp_unreachable() {
    let (mut iface, mut sockets, mut device) = setup(Medium::Ethernet);
    iface.inner.neighbor_cache.fill(
        IpAddress::Ipv4(PEER),
        HardwareAddress::Ethernet(EthernetAddress([2, 0, 0, 0, 0, 2])),
        Instant::ZERO,
    );
    let mut t = tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0; 1024]),
        tcp::SocketBuffer::new(vec![0; 1024]),
    );
    t.connect(iface.context(), (IpAddress::Ipv4(PEER), 80), 49500)
        .unwrap();
    let h = sockets.add(t);
    let _ = iface.socket_egress(&mut device, &mut sockets);
    // The SYN as it left: Ethernet + IPv4 (20) + TCP.
    let (rx, _tx) = device.receive(Instant::ZERO).expect("a SYN");
    let syn: std::vec::Vec<u8> = rx.consume(|f| f.to_vec());
    let tcp_head = &syn[14 + 20..14 + 20 + 8];
    // An error naming another sequence number is ignored (Linux: out of window).
    let mut wrong = [0u8; 8];
    wrong.copy_from_slice(tcp_head);
    wrong[7] ^= 0x55;
    icmp_error(&mut iface, &mut sockets, Icmpv4DstUnreachable::HostUnreachable, IpProtocol::Tcp, &wrong);
    assert_eq!(sockets.get::<tcp::Socket>(h).state(), tcp::State::SynSent);
    icmp_error(&mut iface, &mut sockets, Icmpv4DstUnreachable::HostUnreachable, IpProtocol::Tcp, tcp_head);
    let t = sockets.get::<tcp::Socket>(h);
    assert_eq!(t.state(), tcp::State::Closed);
    assert_eq!(t.icmp_errno(), 113);
}

#[test]
fn test_ipv4_fragmented_broadcast_round_trip() {
    let (mut iface, mut sockets, mut device) = setup(Medium::Ethernet);
    let mut tx = udp_socket(8192);
    tx.bind(4000).unwrap();
    let tx = sockets.add(tx);
    let mut a = udp_socket(8192);
    a.bind(5000).unwrap();
    let a = sockets.add(a);
    let mut b = udp_socket(8192);
    b.bind(5000).unwrap();
    let b = sockets.add(b);
    let data: std::vec::Vec<u8> = (0..4000).map(|i| (i % 251) as u8).collect();
    sockets
        .get_mut::<udp::Socket>(tx)
        .send_slice(&data, (IpAddress::v4(192, 168, 1, 255), 5000))
        .unwrap();
    for _ in 0..4 {
        iface.poll(Instant::ZERO, &mut device, &mut sockets);
    }
    for h in [a, b] {
        let s = sockets.get_mut::<udp::Socket>(h);
        let (p, m) = s.recv().expect("reassembled datagram");
        assert_eq!(p, &data[..]);
        assert_eq!(m.local_address, Some(IpAddress::v4(192, 168, 1, 255)));
        assert_eq!(m.hop_limit, Some(64));
    }
}

#[test]
fn test_ipv4_fragment_offsets_are_multiples_of_8() {
    let (mut iface, mut sockets, mut device) = setup(Medium::Ethernet);
    let mut tx = udp_socket(8192);
    tx.bind(4000).unwrap();
    let tx = sockets.add(tx);
    let data = vec![7u8; 3000];
    sockets
        .get_mut::<udp::Socket>(tx)
        .send_slice(&data, (IpAddress::v4(192, 168, 1, 255), 5000))
        .unwrap();
    // Egress only: take the frames out before the interface reads them back.
    let _ = iface.socket_egress(&mut device, &mut sockets);
    let mut total = 0;
    let mut n = 0;
    while let Some((rx, _)) = device.receive(Instant::ZERO) {
        rx.consume(|f| {
            assert!(f.len() <= 1514);
            let ip = Ipv4Packet::new_checked(&f[14..]).unwrap();
            assert_eq!(ip.frag_offset() as usize, total);
            let len = ip.payload().len();
            if ip.more_frags() {
                assert_eq!(len % 8, 0);
            }
            assert!(!ip.dont_frag());
            total += len;
            n += 1;
        });
    }
    assert_eq!(total, 3008);
    assert_eq!(n, 3);
}

#[test]
fn test_udp_tos_and_ttl_per_datagram() {
    let (mut iface, mut sockets, mut device) = setup(Medium::Ethernet);
    let mut tx = udp_socket(8192);
    tx.bind(4000).unwrap();
    let tx = sockets.add(tx);
    let mut rx = udp_socket(8192);
    rx.bind(5000).unwrap();
    let rx = sockets.add(rx);
    for len in [100usize, 3000] {
        let mut meta = udp::UdpMetadata::from((IpAddress::v4(192, 168, 1, 255), 5000));
        meta.meta.tos = 0xb8;
        meta.hop_limit = Some(7);
        sockets
            .get_mut::<udp::Socket>(tx)
            .send_slice(&vec![1u8; len], meta)
            .unwrap();
        for _ in 0..4 {
            iface.poll(Instant::ZERO, &mut device, &mut sockets);
        }
        let (p, m) = sockets.get_mut::<udp::Socket>(rx).recv().expect("datagram");
        assert_eq!(p.len(), len);
        assert_eq!(m.tos, 0xb8);
        assert_eq!(m.hop_limit, Some(7));
    }
}
