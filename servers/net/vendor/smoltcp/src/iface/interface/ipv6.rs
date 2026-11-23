use super::*;

#[cfg(feature = "socket-icmp")]
use crate::socket::icmp;
use crate::socket::AnySocket;

use crate::phy::PacketMeta;
use crate::wire::*;

/// Enum used for the process_hopbyhop function. In some cases, when discarding a packet, an ICMMP
/// parameter problem message needs to be transmitted to the source of the address. In other cases,
/// the processing of the IP packet can continue.
#[allow(clippy::large_enum_variant)]
enum HopByHopResponse<'frame> {
    /// Continue processing the IPv6 packet.
    Continue((IpProtocol, &'frame [u8])),
    /// Discard the packet and maybe send back an ICMPv6 packet.
    Discard(Option<Packet<'frame>>),
}

// We implement `Default` such that we can use the check! macro.
impl Default for HopByHopResponse<'_> {
    fn default() -> Self {
        Self::Discard(None)
    }
}

impl InterfaceInner {
    pub(super) fn process_ipv6<'frame>(
        &mut self,
        sockets: &mut SocketSet,
        meta: PacketMeta,
        ipv6_packet: &Ipv6Packet<&'frame [u8]>,
        frag: &'frame mut FragmentsBuffer,
    ) -> Option<Packet<'frame>> {
        #[cfg(feature = "proto-ipv4-fragmentation")]
        let reasm = Some((&mut frag.assembler, frag.reassembly_timeout));
        #[cfg(not(feature = "proto-ipv4-fragmentation"))]
        let _ = frag;
        self.process_ipv6_inner(
            sockets,
            meta,
            ipv6_packet,
            #[cfg(feature = "proto-ipv4-fragmentation")]
            reasm,
        )
    }

    /// LEANDROS: `process_ipv6` with the reassembly state passed apart from the
    /// `FragmentsBuffer`, so a caller whose packet borrows another field of the
    /// buffer (6LoWPAN: the decompression buffer or the 6LoWPAN assembler) can
    /// still hand over the assembler it does not hold. `None` drops IPv6
    /// fragment headers instead of reassembling them.
    pub(super) fn process_ipv6_inner<'frame>(
        &mut self,
        sockets: &mut SocketSet,
        meta: PacketMeta,
        ipv6_packet: &Ipv6Packet<&'frame [u8]>,
        #[cfg(feature = "proto-ipv4-fragmentation")] reasm: Option<(
            &'frame mut PacketAssemblerSet<FragKey>,
            Duration,
        )>,
    ) -> Option<Packet<'frame>> {
        let ipv6_repr = check!(Ipv6Repr::parse(ipv6_packet));
        // LEANDROS: the traffic class, for IPV6_RECVTCLASS (see udp::Socket::process).
        self.rx_tos = ipv6_packet.traffic_class();

        if !ipv6_repr.src_addr.is_unicast() {
            // Discard packets with non-unicast source addresses.
            net_debug!("non-unicast source address");
            return None;
        }

        let (next_header, ip_payload) = if ipv6_repr.next_header == IpProtocol::HopByHop {
            match self.process_hopbyhop(ipv6_repr, ipv6_packet.payload()) {
                HopByHopResponse::Discard(e) => return e,
                HopByHopResponse::Continue(next) => next,
            }
        } else {
            (ipv6_repr.next_header, ipv6_packet.payload())
        };

        if !self.has_ip_addr(ipv6_repr.dst_addr)
            && !self.has_multicast_group(ipv6_repr.dst_addr)
            && !ipv6_repr.dst_addr.is_loopback()
        {
            net_trace!("packet IP address not for this interface");
            return None;
        }

        // LEANDROS: reassemble a fragmented datagram (RFC 8200 4.5); the
        // payload that follows is the upper layer of the whole.
        #[cfg(feature = "proto-ipv4-fragmentation")]
        let (next_header, ip_payload) = if next_header == IpProtocol::Ipv6Frag {
            let fh = check!(Ipv6FragmentHeader::new_checked(check!(ip_payload.get(2..).ok_or(crate::wire::Error))));
            let first_nh = ip_payload[0];
            let (fr, data) = (check!(Ipv6FragmentRepr::parse(&fh)), &ip_payload[8..]);
            let off = fr.frag_offset as usize * 8;
            if fr.more_frags && (data.is_empty() || data.len() % 8 != 0) {
                return None;
            }
            if data.is_empty() && !(fr.more_frags) && off == 0 {
                return None;
            }
            let key = FragKey::Ipv6(Ipv6FragKey { src: ipv6_repr.src_addr, dst: ipv6_repr.dst_addr, ident: fr.ident });
            let (assembler, timeout) = reasm?;
            let f = match assembler.get(&key, self.now + timeout) {
                Ok(f) => f,
                Err(_) => {
                    net_debug!("No available packet assembler for fragmented packet");
                    return None;
                }
            };
            if !fr.more_frags {
                check!(f.set_total_size(off + data.len()));
            }
            if let Err(e) = f.add(data, off) {
                net_debug!("fragmentation error: {:?}", e);
                f.reset();
                return None;
            }
            match f.assemble() {
                Some(payload) => (IpProtocol::from(first_nh), payload),
                None => return None,
            }
        } else {
            (next_header, ip_payload)
        };

        #[cfg(feature = "socket-raw")]
        let handled_by_raw_socket = self.raw_socket_filter(sockets, &ipv6_repr.into(), ip_payload);
        #[cfg(not(feature = "socket-raw"))]
        let handled_by_raw_socket = false;

        self.process_nxt_hdr(
            sockets,
            meta,
            ipv6_repr,
            next_header,
            handled_by_raw_socket,
            ip_payload,
        )
    }

    /// LEANDROS: the next IPv6 fragment of the packet the fragmenter holds.
    #[cfg(feature = "proto-ipv4-fragmentation")]
    pub(super) fn dispatch_ipv6_frag<Tx: TxToken>(&mut self, tx_token: Tx, frag: &mut Fragmenter) {
        let mtu_max = if frag.ipv6.mtu != 0 { frag.ipv6.mtu } else { self.ip_mtu() };
        let left = frag.packet_len - frag.sent_bytes;
        let offset = frag.sent_bytes - 40;
        // every fragment but the last carries a multiple of 8 bytes
        let payload_len = if 40 + 8 + left <= mtu_max { left } else { (mtu_max - 40 - 8) & !7 };
        let more = left != payload_len;
        let mut tx_len = 40 + 8 + payload_len;
        #[cfg(feature = "medium-ethernet")]
        if matches!(self.caps.medium, Medium::Ethernet) {
            tx_len += EthernetFrame::<&[u8]>::header_len();
        }
        let src = self.hardware_addr;
        let repr = frag.ipv6.repr;
        let (ident, tclass) = (frag.ipv6.ident, frag.ipv6.tclass);
        let data = &frag.buffer[40 + offset..40 + offset + payload_len];
        #[cfg(feature = "medium-ethernet")]
        let dst_hw = frag.ipv6.dst_hardware_addr;
        tx_token.consume(tx_len, |mut tx_buffer| {
            #[cfg(feature = "medium-ethernet")]
            if matches!(self.caps.medium, Medium::Ethernet) {
                let mut frame = EthernetFrame::new_unchecked(&mut tx_buffer[..]);
                frame.set_src_addr(src.ethernet_or_panic());
                frame.set_dst_addr(dst_hw);
                frame.set_ethertype(EthernetProtocol::Ipv6);
                tx_buffer = &mut tx_buffer[EthernetFrame::<&[u8]>::header_len()..];
            }
            emit_ipv6_fragment(&repr, &mut tx_buffer[..], ident, offset, more, tclass, data);
        });
        frag.sent_bytes += payload_len;
    }

    fn process_hopbyhop<'frame>(
        &mut self,
        ipv6_repr: Ipv6Repr,
        ip_payload: &'frame [u8],
    ) -> HopByHopResponse<'frame> {
        let param_problem = || {
            let payload_len =
                icmp_reply_payload_len(ip_payload.len(), IPV6_MIN_MTU, ipv6_repr.buffer_len());
            self.icmpv6_reply(
                ipv6_repr,
                Icmpv6Repr::ParamProblem {
                    reason: Icmpv6ParamProblem::UnrecognizedOption,
                    pointer: ipv6_repr.buffer_len() as u32,
                    header: ipv6_repr,
                    data: &ip_payload[0..payload_len],
                },
            )
        };

        let ext_hdr = check!(Ipv6ExtHeader::new_checked(ip_payload));
        let ext_repr = check!(Ipv6ExtHeaderRepr::parse(&ext_hdr));
        let hbh_hdr = check!(Ipv6HopByHopHeader::new_checked(ext_repr.data));
        let hbh_repr = check!(Ipv6HopByHopRepr::parse(&hbh_hdr));

        for opt_repr in &hbh_repr.options {
            match opt_repr {
                Ipv6OptionRepr::Pad1 | Ipv6OptionRepr::PadN(_) => (),
                #[cfg(feature = "proto-rpl")]
                Ipv6OptionRepr::Rpl(_) => {}

                Ipv6OptionRepr::Unknown { type_, .. } => {
                    match Ipv6OptionFailureType::from(*type_) {
                        Ipv6OptionFailureType::Skip => (),
                        Ipv6OptionFailureType::Discard => {
                            return HopByHopResponse::Discard(None);
                        }
                        Ipv6OptionFailureType::DiscardSendAll => {
                            return HopByHopResponse::Discard(param_problem());
                        }
                        Ipv6OptionFailureType::DiscardSendUnicast
                            if !ipv6_repr.dst_addr.is_multicast() =>
                        {
                            return HopByHopResponse::Discard(param_problem());
                        }
                        _ => unreachable!(),
                    }
                }
            }
        }

        HopByHopResponse::Continue((
            ext_repr.next_header,
            &ip_payload[ext_repr.header_len() + ext_repr.data.len()..],
        ))
    }

    /// Given the next header value forward the payload onto the correct process
    /// function.
    fn process_nxt_hdr<'frame>(
        &mut self,
        sockets: &mut SocketSet,
        meta: PacketMeta,
        ipv6_repr: Ipv6Repr,
        nxt_hdr: IpProtocol,
        handled_by_raw_socket: bool,
        ip_payload: &'frame [u8],
    ) -> Option<Packet<'frame>> {
        match nxt_hdr {
            IpProtocol::Icmpv6 => self.process_icmpv6(sockets, ipv6_repr.into(), ip_payload),

            #[cfg(any(feature = "socket-udp", feature = "socket-dns"))]
            IpProtocol::Udp => {
                let udp_packet = check!(UdpPacket::new_checked(ip_payload));
                let udp_repr = check!(UdpRepr::parse(
                    &udp_packet,
                    &ipv6_repr.src_addr.into(),
                    &ipv6_repr.dst_addr.into(),
                    &self.checksum_caps(),
                ));

                self.process_udp(
                    sockets,
                    meta,
                    ipv6_repr.into(),
                    udp_repr,
                    handled_by_raw_socket,
                    udp_packet.payload(),
                    ip_payload,
                )
            }

            #[cfg(feature = "socket-tcp")]
            IpProtocol::Tcp => self.process_tcp(sockets, ipv6_repr.into(), ip_payload),

            #[cfg(feature = "socket-raw")]
            _ if handled_by_raw_socket => None,

            _ => {
                // Send back as much of the original payload as we can.
                let payload_len =
                    icmp_reply_payload_len(ip_payload.len(), IPV6_MIN_MTU, ipv6_repr.buffer_len());
                let icmp_reply_repr = Icmpv6Repr::ParamProblem {
                    reason: Icmpv6ParamProblem::UnrecognizedNxtHdr,
                    // The offending packet is after the IPv6 header.
                    pointer: ipv6_repr.buffer_len() as u32,
                    header: ipv6_repr,
                    data: &ip_payload[0..payload_len],
                };
                self.icmpv6_reply(ipv6_repr, icmp_reply_repr)
            }
        }
    }

    /// LEANDROS: IPV6_RECVERR (Linux __udp6_lib_err -> ipv6_icmp_error).
    #[cfg(feature = "socket-udp")]
    fn icmpv6_error_q(&mut self, sockets: &mut SocketSet, errno: u8, ty: u8, code: u8, info: u32, offender: IpAddress, header: &Ipv6Repr, data: &[u8]) {
        if header.next_header != IpProtocol::Udp || data.len() < 8 {
            return;
        }
        let sport = u16::from_be_bytes([data[0], data[1]]);
        let dport = u16::from_be_bytes([data[2], data[3]]);
        let local = IpAddress::Ipv6(header.src_addr);
        let remote = IpEndpoint::new(IpAddress::Ipv6(header.dst_addr), dport);
        for u in sockets.items_mut().filter_map(|i| crate::socket::udp::Socket::downcast_mut(&mut i.socket)) {
            let ep = u.endpoint();
            if u.recverr()
                && ep.port == sport
                && ep.addr.map_or(true, |a| a == local)
                && u.peer().map_or(true, |p| p == remote)
            {
                u.push_err(crate::socket::udp::ErrEntry {
                    errno,
                    origin: 3,
                    ty,
                    code,
                    info,
                    offender,
                    dst: remote,
                    payload: data[8..].to_vec(),
                });
                u.set_error(errno);
            }
        }
    }
    #[cfg(not(feature = "socket-udp"))]
    fn icmpv6_error_q(&mut self, _: &mut SocketSet, _: u8, _: u8, _: u8, _: u32, _: IpAddress, _: &Ipv6Repr, _: &[u8]) {}

    /// A fatal ICMPv6 error reaches a connected UDP socket as its pending error.
    #[cfg(feature = "socket-udp")]
    fn icmpv6_error_fatal(&mut self, sockets: &mut SocketSet, errno: u8, header: &Ipv6Repr, data: &[u8]) {
        if header.next_header != IpProtocol::Udp || data.len() < 8 {
            return;
        }
        let sport = u16::from_be_bytes([data[0], data[1]]);
        let dport = u16::from_be_bytes([data[2], data[3]]);
        let local = IpAddress::Ipv6(header.src_addr);
        let remote = IpEndpoint::new(IpAddress::Ipv6(header.dst_addr), dport);
        for u in sockets.items_mut().filter_map(|i| crate::socket::udp::Socket::downcast_mut(&mut i.socket)) {
            let ep = u.endpoint();
            if ep.port == sport && ep.addr.map_or(true, |a| a == local) && u.peer() == Some(remote) {
                u.set_error(errno);
            }
        }
    }
    #[cfg(not(feature = "socket-udp"))]
    fn icmpv6_error_fatal(&mut self, _: &mut SocketSet, _: u8, _: &Ipv6Repr, _: &[u8]) {}

    pub(super) fn process_icmpv6<'frame>(
        &mut self,
        _sockets: &mut SocketSet,
        ip_repr: IpRepr,
        ip_payload: &'frame [u8],
    ) -> Option<Packet<'frame>> {
        // LEANDROS: an MLD query (type 130) is the net server's.
        if self.igmp_v3 && ip_payload.first() == Some(&130) {
            let h = MLD_QUERY_HOOK.load(core::sync::atomic::Ordering::Relaxed);
            if h != 0 {
                if let (IpAddress::Ipv6(s), IpAddress::Ipv6(d)) = (ip_repr.src_addr(), ip_repr.dst_addr()) {
                    // SAFETY: only ever stored from a `fn([u8; 16], [u8; 16], u8, &[u8]) -> bool`.
                    #[allow(unsafe_code)]
                    let f: fn([u8; 16], [u8; 16], u8, &[u8]) -> bool = unsafe { core::mem::transmute(h) };
                    if f(s.0, d.0, ip_repr.hop_limit(), ip_payload) { return None; }
                }
            }
        }
        let icmp_packet = check!(Icmpv6Packet::new_checked(ip_payload));
        let icmp_repr = check!(Icmpv6Repr::parse(
            &ip_repr.src_addr(),
            &ip_repr.dst_addr(),
            &icmp_packet,
            &self.caps.checksum,
        ));

        #[cfg(feature = "socket-icmp")]
        let mut handled_by_icmp_socket = false;

        #[cfg(feature = "socket-icmp")]
        for icmp_socket in _sockets
            .items_mut()
            .filter_map(|i| icmp::Socket::downcast_mut(&mut i.socket))
        {
            if icmp_socket.accepts(self, &ip_repr, &icmp_repr.into()) {
                icmp_socket.process(self, &ip_repr, &icmp_repr.into());
                handled_by_icmp_socket = true;
            }
        }

        match icmp_repr {
            // Respond to echo requests.
            Icmpv6Repr::EchoRequest {
                ident,
                seq_no,
                data,
            } => match ip_repr {
                IpRepr::Ipv6(ipv6_repr) => {
                    let icmp_reply_repr = Icmpv6Repr::EchoReply {
                        ident,
                        seq_no,
                        data,
                    };
                    self.icmpv6_reply(ipv6_repr, icmp_reply_repr)
                }
                #[allow(unreachable_patterns)]
                _ => unreachable!(),
            },

            // Ignore any echo replies.
            Icmpv6Repr::EchoReply { .. } => None,

            // Forward any NDISC packets to the ndisc packet handler
            #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
            Icmpv6Repr::Ndisc(repr) if ip_repr.hop_limit() == 0xff => match ip_repr {
                IpRepr::Ipv6(ipv6_repr) => match self.caps.medium {
                    #[cfg(feature = "medium-ethernet")]
                    Medium::Ethernet => self.process_ndisc(ipv6_repr, repr),
                    #[cfg(feature = "medium-ieee802154")]
                    Medium::Ieee802154 => self.process_ndisc(ipv6_repr, repr),
                    #[cfg(feature = "medium-ip")]
                    Medium::Ip => None,
                },
                #[allow(unreachable_patterns)]
                _ => unreachable!(),
            },

            // LEANDROS: Linux icmpv6_err_convert -- (errno, fatal).
            Icmpv6Repr::DstUnreachable { reason, header, data } => {
                let code: u8 = reason.into();
                const TABLE: [(u8, bool); 7] = [
                    (101, false), // no route: ENETUNREACH
                    (13, true),   // administratively prohibited: EACCES
                    (113, false), // (reserved): EHOSTUNREACH
                    (113, false), // address unreachable: EHOSTUNREACH
                    (111, true),  // port unreachable: ECONNREFUSED
                    (13, true),   // source address failed policy: EACCES
                    (13, true),   // reject route: EACCES
                ];
                let (errno, fatal) = TABLE.get(code as usize).copied().unwrap_or((113, false));
                self.icmpv6_error_q(_sockets, errno, 1, code, 0, ip_repr.src_addr(), &header, data);
                if fatal {
                    self.icmpv6_error_fatal(_sockets, errno, &header, data);
                }
                None
            }
            Icmpv6Repr::TimeExceeded { reason, header, data } => {
                self.icmpv6_error_q(_sockets, 113, 3, reason.into(), 0, ip_repr.src_addr(), &header, data);
                None
            }

            // LEANDROS: RFC 8201 -- a "packet too big" teaches the path MTU
            // towards the quoted datagram's destination.
            Icmpv6Repr::PktTooBig { mtu, header, data } => {
                self.icmpv6_error_q(_sockets, 90, 2, 0, mtu, ip_repr.src_addr(), &header, data);
                let m = mtu.min(u16::MAX as u32) as u16;
                crate::pmtu::update(self.pmtu_scope, header.dst_addr.0, m, crate::pmtu::MIN_PMTU6, self.now.total_millis() as u64);
                None
            }

            // Don't report an error if a packet with unknown type
            // has been handled by an ICMP socket
            #[cfg(feature = "socket-icmp")]
            _ if handled_by_icmp_socket => None,

            // FIXME: do something correct here?
            _ => None,
        }
    }

    #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
    pub(super) fn process_ndisc<'frame>(
        &mut self,
        ip_repr: Ipv6Repr,
        repr: NdiscRepr<'frame>,
    ) -> Option<Packet<'frame>> {
        match repr {
            NdiscRepr::NeighborAdvert {
                lladdr,
                target_addr,
                flags,
            } => {
                // LEANDROS: the resolved address is the NA's target (RFC 4861 7.2.5), not its IP source:
                // a proxy (pasta) answers for ::2 from its own link-local address.
                let _ = &ip_repr;
                let ip_addr = target_addr.into();
                if let Some(lladdr) = lladdr {
                    let lladdr = check!(lladdr.parse(self.caps.medium));
                    if !lladdr.is_unicast() || !target_addr.is_unicast() {
                        return None;
                    }
                    if flags.contains(NdiscNeighborFlags::OVERRIDE)
                        || !self.neighbor_cache.lookup(&ip_addr, self.now).found()
                    {
                        self.neighbor_cache.fill(ip_addr, lladdr, self.now)
                    }
                }
                None
            }
            NdiscRepr::NeighborSolicit {
                target_addr,
                lladdr,
                ..
            } => {
                if let Some(lladdr) = lladdr {
                    let lladdr = check!(lladdr.parse(self.caps.medium));
                    if !lladdr.is_unicast() || !target_addr.is_unicast() {
                        return None;
                    }
                    self.neighbor_cache
                        .fill(ip_repr.src_addr.into(), lladdr, self.now);
                }

                if self.has_solicited_node(ip_repr.dst_addr) && self.has_ip_addr(target_addr) {
                    let advert = Icmpv6Repr::Ndisc(NdiscRepr::NeighborAdvert {
                        flags: NdiscNeighborFlags::SOLICITED,
                        target_addr,
                        #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
                        lladdr: Some(self.hardware_addr.into()),
                    });
                    let ip_repr = Ipv6Repr {
                        src_addr: target_addr,
                        dst_addr: ip_repr.src_addr,
                        next_header: IpProtocol::Icmpv6,
                        hop_limit: 0xff,
                        payload_len: advert.buffer_len(),
                    };
                    Some(Packet::new_ipv6(ip_repr, IpPayload::Icmpv6(advert)))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    pub(super) fn icmpv6_reply<'frame, 'icmp: 'frame>(
        &self,
        ipv6_repr: Ipv6Repr,
        icmp_repr: Icmpv6Repr<'icmp>,
    ) -> Option<Packet<'frame>> {
        let src_addr = ipv6_repr.dst_addr;
        let dst_addr = ipv6_repr.src_addr;

        let src_addr = if src_addr.is_unicast() {
            src_addr
        } else if let Some(addr) = self.get_source_address_ipv6(&dst_addr) {
            addr
        } else {
            net_debug!("no suitable source address found");
            return None;
        };

        let ipv6_reply_repr = Ipv6Repr {
            src_addr,
            dst_addr,
            next_header: IpProtocol::Icmpv6,
            payload_len: icmp_repr.buffer_len(),
            hop_limit: 64,
        };
        Some(Packet::new_ipv6(
            ipv6_reply_repr,
            IpPayload::Icmpv6(icmp_repr),
        ))
    }
}
