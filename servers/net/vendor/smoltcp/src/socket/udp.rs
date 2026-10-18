use core::cmp::min;
#[cfg(feature = "async")]
use core::task::Waker;

use crate::iface::Context;
use crate::phy::PacketMeta;
use crate::socket::PollAt;
#[cfg(feature = "async")]
use crate::socket::WakerRegistration;
use crate::storage::Empty;
use crate::wire::{IpAddress, IpEndpoint, IpListenEndpoint, IpProtocol, IpRepr, UdpRepr};

/// LEANDROS: source of `Socket::bind_seq` (bind order, for Linux's "newest
/// bound socket wins" unicast demultiplexing).
static BIND_SEQ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// Metadata for a sent or received UDP packet.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy)]
pub struct UdpMetadata {
    pub endpoint: IpEndpoint,
    pub meta: PacketMeta,
    /// LEANDROS: received: the datagram's destination address (IP_PKTINFO's
    /// ipi_addr). Sent: the source address to use instead of the bound or
    /// routed one (IP_PKTINFO's ipi_spec_dst on sendmsg).
    pub local_address: Option<IpAddress>,
    /// LEANDROS: received: the IPv4 TOS byte (IP_RECVTOS). Unused on send.
    pub tos: u8,
    /// LEANDROS: received: the TTL (IP_RECVTTL). Sent: the TTL for this
    /// datagram (IP_TTL, IP_MULTICAST_TTL), else the socket's hop limit.
    pub hop_limit: Option<u8>,
}

/// LEANDROS: equal when endpoint and packet meta are (what upstream
/// compared); the received-packet details are not part of the identity.
impl PartialEq for UdpMetadata {
    fn eq(&self, other: &Self) -> bool {
        self.endpoint == other.endpoint && self.meta == other.meta
    }
}

impl Eq for UdpMetadata {}

impl<T: Into<IpEndpoint>> From<T> for UdpMetadata {
    fn from(value: T) -> Self {
        Self {
            endpoint: value.into(),
            meta: PacketMeta::default(),
            local_address: None,
            tos: 0,
            hop_limit: None,
        }
    }
}

impl core::fmt::Display for UdpMetadata {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        #[cfg(feature = "packetmeta-id")]
        return write!(f, "{}, PacketID: {:?}", self.endpoint, self.meta);

        #[cfg(not(feature = "packetmeta-id"))]
        write!(f, "{}", self.endpoint)
    }
}

/// A UDP packet metadata.
pub type PacketMetadata = crate::storage::PacketMetadata<UdpMetadata>;

/// A UDP packet ring buffer.
pub type PacketBuffer<'a> = crate::storage::PacketBuffer<'a, UdpMetadata>;

/// Error returned by [`Socket::bind`]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum BindError {
    InvalidState,
    Unaddressable,
}

impl core::fmt::Display for BindError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BindError::InvalidState => write!(f, "invalid state"),
            BindError::Unaddressable => write!(f, "unaddressable"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for BindError {}

/// Error returned by [`Socket::send`]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum SendError {
    Unaddressable,
    BufferFull,
}

impl core::fmt::Display for SendError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SendError::Unaddressable => write!(f, "unaddressable"),
            SendError::BufferFull => write!(f, "buffer full"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for SendError {}

/// Error returned by [`Socket::recv`]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum RecvError {
    Exhausted,
    Truncated,
}

impl core::fmt::Display for RecvError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RecvError::Exhausted => write!(f, "exhausted"),
            RecvError::Truncated => write!(f, "truncated"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for RecvError {}

/// A User Datagram Protocol socket.
///
/// A UDP socket is bound to a specific endpoint, and owns transmit and receive
/// packet buffers.
#[derive(Debug)]
pub struct Socket<'a> {
    endpoint: IpListenEndpoint,
    rx_buffer: PacketBuffer<'a>,
    tx_buffer: PacketBuffer<'a>,
    /// The time-to-live (IPv4) or hop limit (IPv6) value used in outgoing packets.
    hop_limit: Option<u8>,
    /// LEANDROS: the connected peer. Only its datagrams are accepted (Linux's
    /// 4-tuple lookup), so a socket connected after datagrams from others
    /// were queued keeps them, as Linux does.
    peer: Option<IpEndpoint>,
    /// LEANDROS: SO_REUSEPORT: unicast to a group of such sockets sharing a
    /// port is spread over them by a hash of the flow.
    reuseport: bool,
    /// LEANDROS: when the socket was bound (`BIND_SEQ`).
    bind_seq: u64,
    /// LEANDROS: a pending socket error (errno, 0 = none) from an ICMP
    /// error about a datagram this connected socket sent (sk_err).
    error: u8,
    #[cfg(feature = "async")]
    rx_waker: WakerRegistration,
    #[cfg(feature = "async")]
    tx_waker: WakerRegistration,
}

impl<'a> Socket<'a> {
    /// Create an UDP socket with the given buffers.
    pub fn new(rx_buffer: PacketBuffer<'a>, tx_buffer: PacketBuffer<'a>) -> Socket<'a> {
        Socket {
            endpoint: IpListenEndpoint::default(),
            rx_buffer,
            tx_buffer,
            hop_limit: None,
            peer: None,
            reuseport: false,
            bind_seq: 0,
            error: 0,
            #[cfg(feature = "async")]
            rx_waker: WakerRegistration::new(),
            #[cfg(feature = "async")]
            tx_waker: WakerRegistration::new(),
        }
    }

    /// Register a waker for receive operations.
    ///
    /// The waker is woken on state changes that might affect the return value
    /// of `recv` method calls, such as receiving data, or the socket closing.
    ///
    /// Notes:
    ///
    /// - Only one waker can be registered at a time. If another waker was previously registered,
    ///   it is overwritten and will no longer be woken.
    /// - The Waker is woken only once. Once woken, you must register it again to receive more wakes.
    /// - "Spurious wakes" are allowed: a wake doesn't guarantee the result of `recv` has
    ///   necessarily changed.
    #[cfg(feature = "async")]
    pub fn register_recv_waker(&mut self, waker: &Waker) {
        self.rx_waker.register(waker)
    }

    /// Register a waker for send operations.
    ///
    /// The waker is woken on state changes that might affect the return value
    /// of `send` method calls, such as space becoming available in the transmit
    /// buffer, or the socket closing.
    ///
    /// Notes:
    ///
    /// - Only one waker can be registered at a time. If another waker was previously registered,
    ///   it is overwritten and will no longer be woken.
    /// - The Waker is woken only once. Once woken, you must register it again to receive more wakes.
    /// - "Spurious wakes" are allowed: a wake doesn't guarantee the result of `send` has
    ///   necessarily changed.
    #[cfg(feature = "async")]
    pub fn register_send_waker(&mut self, waker: &Waker) {
        self.tx_waker.register(waker)
    }

    /// Return the bound endpoint.
    #[inline]
    pub fn endpoint(&self) -> IpListenEndpoint {
        self.endpoint
    }

    /// Return the time-to-live (IPv4) or hop limit (IPv6) value used in outgoing packets.
    ///
    /// See also the [set_hop_limit](#method.set_hop_limit) method
    pub fn hop_limit(&self) -> Option<u8> {
        self.hop_limit
    }

    /// Set the time-to-live (IPv4) or hop limit (IPv6) value used in outgoing packets.
    ///
    /// A socket without an explicitly set hop limit value uses the default [IANA recommended]
    /// value (64).
    ///
    /// # Panics
    ///
    /// This function panics if a hop limit value of 0 is given. See [RFC 1122 § 3.2.1.7].
    ///
    /// [IANA recommended]: https://www.iana.org/assignments/ip-parameters/ip-parameters.xhtml
    /// [RFC 1122 § 3.2.1.7]: https://tools.ietf.org/html/rfc1122#section-3.2.1.7
    pub fn set_hop_limit(&mut self, hop_limit: Option<u8>) {
        // A host MUST NOT send a datagram with a hop limit value of 0
        if let Some(0) = hop_limit {
            panic!("the time-to-live value of a packet must not be zero")
        }

        self.hop_limit = hop_limit
    }

    /// Bind the socket to the given endpoint.
    ///
    /// This function returns `Err(Error::Illegal)` if the socket was open
    /// (see [is_open](#method.is_open)), and `Err(Error::Unaddressable)`
    /// if the port in the given endpoint is zero.
    pub fn bind<T: Into<IpListenEndpoint>>(&mut self, endpoint: T) -> Result<(), BindError> {
        let endpoint = endpoint.into();
        if endpoint.port == 0 {
            return Err(BindError::Unaddressable);
        }

        if self.is_open() {
            return Err(BindError::InvalidState);
        }

        self.endpoint = endpoint;
        self.bind_seq = BIND_SEQ.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

        #[cfg(feature = "async")]
        {
            self.rx_waker.wake();
            self.tx_waker.wake();
        }

        Ok(())
    }

    /// LEANDROS: give up the bound port (keeping a bound address and the
    /// queued datagrams), and the peer: Linux's udp_disconnect of a socket
    /// whose port was picked automatically. `bind` may be called again.
    pub fn unbind_port(&mut self) {
        self.endpoint.port = 0;
        self.peer = None;
    }

    /// LEANDROS: datagrams are still queued to be sent (a closed socket is
    /// kept until they are out: Linux sends at sendto time).
    pub fn send_pending(&self) -> bool {
        !self.tx_buffer.is_empty()
    }

    /// LEANDROS: the connected peer (see `peer`).
    pub fn peer(&self) -> Option<IpEndpoint> {
        self.peer
    }

    /// LEANDROS: set or clear the connected peer.
    pub fn set_peer(&mut self, peer: Option<IpEndpoint>) {
        self.peer = peer;
    }

    /// LEANDROS: SO_REUSEPORT.
    pub fn set_reuseport(&mut self, on: bool) {
        self.reuseport = on;
    }

    /// LEANDROS: SO_REUSEPORT.
    pub fn reuseport(&self) -> bool {
        self.reuseport
    }

    /// LEANDROS: the order of binds (larger = bound later).
    pub fn bind_seq(&self) -> u64 {
        self.bind_seq
    }

    /// LEANDROS: the pending socket error (errno; 0 = none).
    pub fn error(&self) -> u8 {
        self.error
    }

    /// LEANDROS: read and clear the pending socket error (errno; 0 = none).
    pub fn take_error(&mut self) -> u8 {
        core::mem::replace(&mut self.error, 0)
    }

    /// LEANDROS: record an ICMP error for this socket and wake the reader.
    pub(crate) fn set_error(&mut self, errno: u8) {
        self.error = errno;
        #[cfg(feature = "async")]
        self.rx_waker.wake();
    }

    /// Close the socket.
    pub fn close(&mut self) {
        // Clear the bound endpoint of the socket.
        self.endpoint = IpListenEndpoint::default();
        self.peer = None;
        self.reuseport = false;
        self.error = 0;

        // Reset the RX and TX buffers of the socket.
        self.tx_buffer.reset();
        self.rx_buffer.reset();

        #[cfg(feature = "async")]
        {
            self.rx_waker.wake();
            self.tx_waker.wake();
        }
    }

    /// Check whether the socket is open.
    #[inline]
    pub fn is_open(&self) -> bool {
        self.endpoint.port != 0
    }

    /// Check whether the transmit buffer is full.
    #[inline]
    pub fn can_send(&self) -> bool {
        !self.tx_buffer.is_full()
    }

    /// Check whether the receive buffer is not empty.
    #[inline]
    pub fn can_recv(&self) -> bool {
        !self.rx_buffer.is_empty()
    }

    /// Return the maximum number packets the socket can receive.
    #[inline]
    pub fn packet_recv_capacity(&self) -> usize {
        self.rx_buffer.packet_capacity()
    }

    /// Return the maximum number packets the socket can transmit.
    #[inline]
    pub fn packet_send_capacity(&self) -> usize {
        self.tx_buffer.packet_capacity()
    }

    /// Return the maximum number of bytes inside the recv buffer.
    #[inline]
    pub fn payload_recv_capacity(&self) -> usize {
        self.rx_buffer.payload_capacity()
    }

    /// Return the maximum number of bytes inside the transmit buffer.
    #[inline]
    pub fn payload_send_capacity(&self) -> usize {
        self.tx_buffer.payload_capacity()
    }

    /// Enqueue a packet to be sent to a given remote endpoint, and return a pointer
    /// to its payload.
    ///
    /// This function returns `Err(Error::Exhausted)` if the transmit buffer is full,
    /// `Err(Error::Unaddressable)` if local or remote port, or remote address are unspecified,
    /// and `Err(Error::Truncated)` if there is not enough transmit buffer capacity
    /// to ever send this packet.
    pub fn send(
        &mut self,
        size: usize,
        meta: impl Into<UdpMetadata>,
    ) -> Result<&mut [u8], SendError> {
        let meta = meta.into();
        if self.endpoint.port == 0 {
            return Err(SendError::Unaddressable);
        }
        if meta.endpoint.addr.is_unspecified() {
            return Err(SendError::Unaddressable);
        }
        if meta.endpoint.port == 0 {
            return Err(SendError::Unaddressable);
        }

        let payload_buf = self
            .tx_buffer
            .enqueue(size, meta)
            .map_err(|_| SendError::BufferFull)?;

        net_trace!(
            "udp:{}:{}: buffer to send {} octets",
            self.endpoint,
            meta.endpoint,
            size
        );
        Ok(payload_buf)
    }

    /// Enqueue a packet to be send to a given remote endpoint and pass the buffer
    /// to the provided closure. The closure then returns the size of the data written
    /// into the buffer.
    ///
    /// Also see [send](#method.send).
    pub fn send_with<F>(
        &mut self,
        max_size: usize,
        meta: impl Into<UdpMetadata>,
        f: F,
    ) -> Result<usize, SendError>
    where
        F: FnOnce(&mut [u8]) -> usize,
    {
        let meta = meta.into();
        if self.endpoint.port == 0 {
            return Err(SendError::Unaddressable);
        }
        if meta.endpoint.addr.is_unspecified() {
            return Err(SendError::Unaddressable);
        }
        if meta.endpoint.port == 0 {
            return Err(SendError::Unaddressable);
        }

        let size = self
            .tx_buffer
            .enqueue_with_infallible(max_size, meta, f)
            .map_err(|_| SendError::BufferFull)?;

        net_trace!(
            "udp:{}:{}: buffer to send {} octets",
            self.endpoint,
            meta.endpoint,
            size
        );
        Ok(size)
    }

    /// Enqueue a packet to be sent to a given remote endpoint, and fill it from a slice.
    ///
    /// See also [send](#method.send).
    pub fn send_slice(
        &mut self,
        data: &[u8],
        meta: impl Into<UdpMetadata>,
    ) -> Result<(), SendError> {
        self.send(data.len(), meta)?.copy_from_slice(data);
        Ok(())
    }

    /// Dequeue a packet received from a remote endpoint, and return the endpoint as well
    /// as a pointer to the payload.
    ///
    /// This function returns `Err(Error::Exhausted)` if the receive buffer is empty.
    pub fn recv(&mut self) -> Result<(&[u8], UdpMetadata), RecvError> {
        let (remote_endpoint, payload_buf) =
            self.rx_buffer.dequeue().map_err(|_| RecvError::Exhausted)?;

        net_trace!(
            "udp:{}:{}: receive {} buffered octets",
            self.endpoint,
            remote_endpoint.endpoint,
            payload_buf.len()
        );
        Ok((payload_buf, remote_endpoint))
    }

    /// Dequeue a packet received from a remote endpoint, copy the payload into the given slice,
    /// and return the amount of octets copied as well as the endpoint.
    ///
    /// **Note**: when the size of the provided buffer is smaller than the size of the payload,
    /// the packet is dropped and a `RecvError::Truncated` error is returned.
    ///
    /// See also [recv](#method.recv).
    pub fn recv_slice(&mut self, data: &mut [u8]) -> Result<(usize, UdpMetadata), RecvError> {
        let (buffer, endpoint) = self.recv().map_err(|_| RecvError::Exhausted)?;

        if data.len() < buffer.len() {
            return Err(RecvError::Truncated);
        }

        let length = min(data.len(), buffer.len());
        data[..length].copy_from_slice(&buffer[..length]);
        Ok((length, endpoint))
    }

    /// Peek at a packet received from a remote endpoint, and return the endpoint as well
    /// as a pointer to the payload without removing the packet from the receive buffer.
    /// This function otherwise behaves identically to [recv](#method.recv).
    ///
    /// It returns `Err(Error::Exhausted)` if the receive buffer is empty.
    pub fn peek(&mut self) -> Result<(&[u8], &UdpMetadata), RecvError> {
        let endpoint = self.endpoint;
        self.rx_buffer.peek().map_err(|_| RecvError::Exhausted).map(
            |(remote_endpoint, payload_buf)| {
                net_trace!(
                    "udp:{}:{}: peek {} buffered octets",
                    endpoint,
                    remote_endpoint.endpoint,
                    payload_buf.len()
                );
                (payload_buf, remote_endpoint)
            },
        )
    }

    /// Peek at a packet received from a remote endpoint, copy the payload into the given slice,
    /// and return the amount of octets copied as well as the endpoint without removing the
    /// packet from the receive buffer.
    /// This function otherwise behaves identically to [recv_slice](#method.recv_slice).
    ///
    /// **Note**: when the size of the provided buffer is smaller than the size of the payload,
    /// no data is copied into the provided buffer and a `RecvError::Truncated` error is returned.
    ///
    /// See also [peek](#method.peek).
    pub fn peek_slice(&mut self, data: &mut [u8]) -> Result<(usize, &UdpMetadata), RecvError> {
        let (buffer, endpoint) = self.peek()?;

        if data.len() < buffer.len() {
            return Err(RecvError::Truncated);
        }

        let length = min(data.len(), buffer.len());
        data[..length].copy_from_slice(&buffer[..length]);
        Ok((length, endpoint))
    }

    pub(crate) fn accepts(&self, cx: &mut Context, ip_repr: &IpRepr, repr: &UdpRepr) -> bool {
        self.score(cx, ip_repr, repr).is_some()
    }

    /// LEANDROS: Linux's compute_score (net/ipv4/udp.c): `None` when the
    /// socket must not see the datagram, else how specific its match is. A
    /// socket bound to an address only hears datagrams sent to that address
    /// (not broadcast or multicast, as on Linux); a connected one only its
    /// peer's.
    pub(crate) fn score(&self, _cx: &mut Context, ip_repr: &IpRepr, repr: &UdpRepr) -> Option<u32> {
        if self.endpoint.port == 0 || self.endpoint.port != repr.dst_port {
            return None;
        }
        let mut score = 1;
        if let Some(addr) = self.endpoint.addr {
            if addr != ip_repr.dst_addr() {
                return None;
            }
            score += 4;
        }
        if let Some(peer) = self.peer {
            if peer.addr != ip_repr.src_addr() || peer.port != repr.src_port {
                return None;
            }
            score += 8;
        }
        Some(score)
    }

    pub(crate) fn process(
        &mut self,
        cx: &mut Context,
        meta: PacketMeta,
        ip_repr: &IpRepr,
        repr: &UdpRepr,
        payload: &[u8],
    ) {
        debug_assert!(self.accepts(cx, ip_repr, repr));

        let size = payload.len();

        let remote_endpoint = IpEndpoint {
            addr: ip_repr.src_addr(),
            port: repr.src_port,
        };

        net_trace!(
            "udp:{}:{}: receiving {} octets",
            self.endpoint,
            remote_endpoint,
            size
        );

        let metadata = UdpMetadata {
            endpoint: remote_endpoint,
            meta,
            local_address: Some(ip_repr.dst_addr()),
            tos: cx.rx_tos,
            hop_limit: Some(ip_repr.hop_limit()),
        };

        match self.rx_buffer.enqueue(size, metadata) {
            Ok(buf) => buf.copy_from_slice(payload),
            Err(_) => net_trace!(
                "udp:{}:{}: buffer full, dropped incoming packet",
                self.endpoint,
                remote_endpoint
            ),
        }

        #[cfg(feature = "async")]
        self.rx_waker.wake();
    }

    pub(crate) fn dispatch<F, E>(&mut self, cx: &mut Context, emit: F) -> Result<(), E>
    where
        F: FnOnce(&mut Context, PacketMeta, (IpRepr, UdpRepr, &[u8])) -> Result<(), E>,
    {
        let endpoint = self.endpoint;
        let sock_hop_limit = self.hop_limit.unwrap_or(64);

        let res = self.tx_buffer.dequeue_with(|packet_meta, payload_buf| {
            let hop_limit = packet_meta.hop_limit.filter(|&h| h != 0).unwrap_or(sock_hop_limit);
            let src_addr = match packet_meta.local_address.or(endpoint.addr) {
                Some(addr) => addr,
                None => match cx.get_source_address(&packet_meta.endpoint.addr) {
                    Some(addr) => addr,
                    None => {
                        net_trace!(
                            "udp:{}:{}: cannot find suitable source address, dropping.",
                            endpoint,
                            packet_meta.endpoint
                        );
                        return Ok(());
                    }
                },
            };

            net_trace!(
                "udp:{}:{}: sending {} octets",
                endpoint,
                packet_meta.endpoint,
                payload_buf.len()
            );

            let repr = UdpRepr {
                src_port: endpoint.port,
                dst_port: packet_meta.endpoint.port,
            };
            let ip_repr = IpRepr::new(
                src_addr,
                packet_meta.endpoint.addr,
                IpProtocol::Udp,
                repr.header_len() + payload_buf.len(),
                hop_limit,
            );

            emit(cx, packet_meta.meta, (ip_repr, repr, payload_buf))
        });
        match res {
            Err(Empty) => Ok(()),
            Ok(Err(e)) => Err(e),
            Ok(Ok(())) => {
                #[cfg(feature = "async")]
                self.tx_waker.wake();
                Ok(())
            }
        }
    }

    pub(crate) fn poll_at(&self, _cx: &mut Context) -> PollAt {
        if self.tx_buffer.is_empty() {
            PollAt::Ingress
        } else {
            PollAt::Now
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::wire::{IpRepr, UdpRepr};

    use crate::phy::Medium;
    use crate::tests::setup;
    use rstest::*;

    fn buffer(packets: usize) -> PacketBuffer<'static> {
        PacketBuffer::new(
            (0..packets)
                .map(|_| PacketMetadata::EMPTY)
                .collect::<Vec<_>>(),
            vec![0; 16 * packets],
        )
    }

    fn socket(
        rx_buffer: PacketBuffer<'static>,
        tx_buffer: PacketBuffer<'static>,
    ) -> Socket<'static> {
        Socket::new(rx_buffer, tx_buffer)
    }

    const LOCAL_PORT: u16 = 53;
    const REMOTE_PORT: u16 = 49500;

    cfg_if::cfg_if! {
        if #[cfg(feature = "proto-ipv4")] {
            use crate::wire::Ipv4Address as IpvXAddress;
            use crate::wire::Ipv4Repr as IpvXRepr;
            use IpRepr::Ipv4 as IpReprIpvX;

            const LOCAL_ADDR: IpvXAddress = IpvXAddress([192, 168, 1, 1]);
            const REMOTE_ADDR: IpvXAddress = IpvXAddress([192, 168, 1, 2]);
            const OTHER_ADDR: IpvXAddress = IpvXAddress([192, 168, 1, 3]);
        } else {
            use crate::wire::Ipv6Address as IpvXAddress;
            use crate::wire::Ipv6Repr as IpvXRepr;
            use IpRepr::Ipv6 as IpReprIpvX;

            const LOCAL_ADDR: IpvXAddress = IpvXAddress([
                0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
            ]);
            const REMOTE_ADDR: IpvXAddress = IpvXAddress([
                0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2,
            ]);
            const OTHER_ADDR: IpvXAddress = IpvXAddress([
                0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3,
            ]);
        }
    }

    pub const LOCAL_END: IpEndpoint = IpEndpoint {
        addr: LOCAL_ADDR.into_address(),
        port: LOCAL_PORT,
    };
    pub const REMOTE_END: IpEndpoint = IpEndpoint {
        addr: REMOTE_ADDR.into_address(),
        port: REMOTE_PORT,
    };

    pub const LOCAL_IP_REPR: IpRepr = IpReprIpvX(IpvXRepr {
        src_addr: LOCAL_ADDR,
        dst_addr: REMOTE_ADDR,
        next_header: IpProtocol::Udp,
        payload_len: 8 + 6,
        hop_limit: 64,
    });

    pub const REMOTE_IP_REPR: IpRepr = IpReprIpvX(IpvXRepr {
        src_addr: REMOTE_ADDR,
        dst_addr: LOCAL_ADDR,
        next_header: IpProtocol::Udp,
        payload_len: 8 + 6,
        hop_limit: 64,
    });

    pub const BAD_IP_REPR: IpRepr = IpReprIpvX(IpvXRepr {
        src_addr: REMOTE_ADDR,
        dst_addr: OTHER_ADDR,
        next_header: IpProtocol::Udp,
        payload_len: 8 + 6,
        hop_limit: 64,
    });

    const LOCAL_UDP_REPR: UdpRepr = UdpRepr {
        src_port: LOCAL_PORT,
        dst_port: REMOTE_PORT,
    };

    const REMOTE_UDP_REPR: UdpRepr = UdpRepr {
        src_port: REMOTE_PORT,
        dst_port: LOCAL_PORT,
    };

    const PAYLOAD: &[u8] = b"abcdef";

    #[test]
    fn test_bind_unaddressable() {
        let mut socket = socket(buffer(0), buffer(0));
        assert_eq!(socket.bind(0), Err(BindError::Unaddressable));
    }

    #[test]
    fn test_bind_twice() {
        let mut socket = socket(buffer(0), buffer(0));
        assert_eq!(socket.bind(1), Ok(()));
        assert_eq!(socket.bind(2), Err(BindError::InvalidState));
    }

    #[test]
    #[should_panic(expected = "the time-to-live value of a packet must not be zero")]
    fn test_set_hop_limit_zero() {
        let mut s = socket(buffer(0), buffer(1));
        s.set_hop_limit(Some(0));
    }

    #[test]
    fn test_send_unaddressable() {
        let mut socket = socket(buffer(0), buffer(1));

        assert_eq!(
            socket.send_slice(b"abcdef", REMOTE_END),
            Err(SendError::Unaddressable)
        );
        assert_eq!(socket.bind(LOCAL_PORT), Ok(()));
        assert_eq!(
            socket.send_slice(
                b"abcdef",
                IpEndpoint {
                    addr: IpvXAddress::UNSPECIFIED.into(),
                    ..REMOTE_END
                }
            ),
            Err(SendError::Unaddressable)
        );
        assert_eq!(
            socket.send_slice(
                b"abcdef",
                IpEndpoint {
                    port: 0,
                    ..REMOTE_END
                }
            ),
            Err(SendError::Unaddressable)
        );
        assert_eq!(socket.send_slice(b"abcdef", REMOTE_END), Ok(()));
    }

    #[rstest]
    #[case::ip(Medium::Ip)]
    #[cfg(feature = "medium-ip")]
    #[case::ethernet(Medium::Ethernet)]
    #[cfg(feature = "medium-ethernet")]
    #[case::ieee802154(Medium::Ieee802154)]
    #[cfg(feature = "medium-ieee802154")]
    fn test_send_dispatch(#[case] medium: Medium) {
        let (mut iface, _, _) = setup(medium);
        let cx = iface.context();
        let mut socket = socket(buffer(0), buffer(1));

        assert_eq!(socket.bind(LOCAL_END), Ok(()));

        assert!(socket.can_send());
        assert_eq!(
            socket.dispatch(cx, |_, _, _| unreachable!()),
            Ok::<_, ()>(())
        );

        assert_eq!(socket.send_slice(b"abcdef", REMOTE_END), Ok(()));
        assert_eq!(
            socket.send_slice(b"123456", REMOTE_END),
            Err(SendError::BufferFull)
        );
        assert!(!socket.can_send());

        assert_eq!(
            socket.dispatch(cx, |_, _, (ip_repr, udp_repr, payload)| {
                assert_eq!(ip_repr, LOCAL_IP_REPR);
                assert_eq!(udp_repr, LOCAL_UDP_REPR);
                assert_eq!(payload, PAYLOAD);
                Err(())
            }),
            Err(())
        );
        assert!(!socket.can_send());

        assert_eq!(
            socket.dispatch(cx, |_, _, (ip_repr, udp_repr, payload)| {
                assert_eq!(ip_repr, LOCAL_IP_REPR);
                assert_eq!(udp_repr, LOCAL_UDP_REPR);
                assert_eq!(payload, PAYLOAD);
                Ok::<_, ()>(())
            }),
            Ok(())
        );
        assert!(socket.can_send());
    }

    #[rstest]
    #[case::ip(Medium::Ip)]
    #[cfg(feature = "medium-ip")]
    #[case::ethernet(Medium::Ethernet)]
    #[cfg(feature = "medium-ethernet")]
    #[case::ieee802154(Medium::Ieee802154)]
    #[cfg(feature = "medium-ieee802154")]
    fn test_recv_process(#[case] medium: Medium) {
        let (mut iface, _, _) = setup(medium);
        let cx = iface.context();

        let mut socket = socket(buffer(1), buffer(0));

        assert_eq!(socket.bind(LOCAL_PORT), Ok(()));

        assert!(!socket.can_recv());
        assert_eq!(socket.recv(), Err(RecvError::Exhausted));

        assert!(socket.accepts(cx, &REMOTE_IP_REPR, &REMOTE_UDP_REPR));
        socket.process(
            cx,
            PacketMeta::default(),
            &REMOTE_IP_REPR,
            &REMOTE_UDP_REPR,
            PAYLOAD,
        );
        assert!(socket.can_recv());

        assert!(socket.accepts(cx, &REMOTE_IP_REPR, &REMOTE_UDP_REPR));
        socket.process(
            cx,
            PacketMeta::default(),
            &REMOTE_IP_REPR,
            &REMOTE_UDP_REPR,
            PAYLOAD,
        );

        assert_eq!(socket.recv(), Ok((&b"abcdef"[..], REMOTE_END.into())));
        assert!(!socket.can_recv());
    }

    #[rstest]
    #[case::ip(Medium::Ip)]
    #[cfg(feature = "medium-ip")]
    #[case::ethernet(Medium::Ethernet)]
    #[cfg(feature = "medium-ethernet")]
    #[case::ieee802154(Medium::Ieee802154)]
    #[cfg(feature = "medium-ieee802154")]
    fn test_peek_process(#[case] medium: Medium) {
        let (mut iface, _, _) = setup(medium);
        let cx = iface.context();

        let mut socket = socket(buffer(1), buffer(0));

        assert_eq!(socket.bind(LOCAL_PORT), Ok(()));

        assert_eq!(socket.peek(), Err(RecvError::Exhausted));

        socket.process(
            cx,
            PacketMeta::default(),
            &REMOTE_IP_REPR,
            &REMOTE_UDP_REPR,
            PAYLOAD,
        );
        assert_eq!(socket.peek(), Ok((&b"abcdef"[..], &REMOTE_END.into(),)));
        assert_eq!(socket.recv(), Ok((&b"abcdef"[..], REMOTE_END.into(),)));
        assert_eq!(socket.peek(), Err(RecvError::Exhausted));
    }

    #[rstest]
    #[case::ip(Medium::Ip)]
    #[cfg(feature = "medium-ip")]
    #[case::ethernet(Medium::Ethernet)]
    #[cfg(feature = "medium-ethernet")]
    #[case::ieee802154(Medium::Ieee802154)]
    #[cfg(feature = "medium-ieee802154")]
    fn test_recv_truncated_slice(#[case] medium: Medium) {
        let (mut iface, _, _) = setup(medium);
        let cx = iface.context();

        let mut socket = socket(buffer(1), buffer(0));

        assert_eq!(socket.bind(LOCAL_PORT), Ok(()));

        assert!(socket.accepts(cx, &REMOTE_IP_REPR, &REMOTE_UDP_REPR));
        socket.process(
            cx,
            PacketMeta::default(),
            &REMOTE_IP_REPR,
            &REMOTE_UDP_REPR,
            PAYLOAD,
        );

        let mut slice = [0; 4];
        assert_eq!(socket.recv_slice(&mut slice[..]), Err(RecvError::Truncated));
    }

    #[rstest]
    #[case::ip(Medium::Ip)]
    #[cfg(feature = "medium-ip")]
    #[case::ethernet(Medium::Ethernet)]
    #[cfg(feature = "medium-ethernet")]
    #[case::ieee802154(Medium::Ieee802154)]
    #[cfg(feature = "medium-ieee802154")]
    fn test_peek_truncated_slice(#[case] medium: Medium) {
        let (mut iface, _, _) = setup(medium);
        let cx = iface.context();

        let mut socket = socket(buffer(1), buffer(0));

        assert_eq!(socket.bind(LOCAL_PORT), Ok(()));

        socket.process(
            cx,
            PacketMeta::default(),
            &REMOTE_IP_REPR,
            &REMOTE_UDP_REPR,
            PAYLOAD,
        );

        let mut slice = [0; 4];
        assert_eq!(socket.peek_slice(&mut slice[..]), Err(RecvError::Truncated));
        assert_eq!(socket.recv_slice(&mut slice[..]), Err(RecvError::Truncated));
        assert_eq!(socket.peek_slice(&mut slice[..]), Err(RecvError::Exhausted));
    }

    #[rstest]
    #[case::ip(Medium::Ip)]
    #[cfg(feature = "medium-ip")]
    #[case::ethernet(Medium::Ethernet)]
    #[cfg(feature = "medium-ethernet")]
    #[case::ieee802154(Medium::Ieee802154)]
    #[cfg(feature = "medium-ieee802154")]
    fn test_set_hop_limit(#[case] medium: Medium) {
        let (mut iface, _, _) = setup(medium);
        let cx = iface.context();

        let mut s = socket(buffer(0), buffer(1));

        assert_eq!(s.bind(LOCAL_END), Ok(()));

        s.set_hop_limit(Some(0x2a));
        assert_eq!(s.send_slice(b"abcdef", REMOTE_END), Ok(()));
        assert_eq!(
            s.dispatch(cx, |_, _, (ip_repr, _, _)| {
                assert_eq!(
                    ip_repr,
                    IpReprIpvX(IpvXRepr {
                        src_addr: LOCAL_ADDR,
                        dst_addr: REMOTE_ADDR,
                        next_header: IpProtocol::Udp,
                        payload_len: 8 + 6,
                        hop_limit: 0x2a,
                    })
                );
                Ok::<_, ()>(())
            }),
            Ok(())
        );
    }

    #[rstest]
    #[case::ip(Medium::Ip)]
    #[cfg(feature = "medium-ip")]
    #[case::ethernet(Medium::Ethernet)]
    #[cfg(feature = "medium-ethernet")]
    #[case::ieee802154(Medium::Ieee802154)]
    #[cfg(feature = "medium-ieee802154")]
    fn test_doesnt_accept_wrong_port(#[case] medium: Medium) {
        let (mut iface, _, _) = setup(medium);
        let cx = iface.context();

        let mut socket = socket(buffer(1), buffer(0));

        assert_eq!(socket.bind(LOCAL_PORT), Ok(()));

        let mut udp_repr = REMOTE_UDP_REPR;
        assert!(socket.accepts(cx, &REMOTE_IP_REPR, &udp_repr));
        udp_repr.dst_port += 1;
        assert!(!socket.accepts(cx, &REMOTE_IP_REPR, &udp_repr));
    }

    #[rstest]
    #[case::ip(Medium::Ip)]
    #[cfg(feature = "medium-ip")]
    #[case::ethernet(Medium::Ethernet)]
    #[cfg(feature = "medium-ethernet")]
    #[case::ieee802154(Medium::Ieee802154)]
    #[cfg(feature = "medium-ieee802154")]
    fn test_doesnt_accept_wrong_ip(#[case] medium: Medium) {
        let (mut iface, _, _) = setup(medium);
        let cx = iface.context();

        let mut port_bound_socket = socket(buffer(1), buffer(0));
        assert_eq!(port_bound_socket.bind(LOCAL_PORT), Ok(()));
        assert!(port_bound_socket.accepts(cx, &BAD_IP_REPR, &REMOTE_UDP_REPR));

        let mut ip_bound_socket = socket(buffer(1), buffer(0));
        assert_eq!(ip_bound_socket.bind(LOCAL_END), Ok(()));
        assert!(!ip_bound_socket.accepts(cx, &BAD_IP_REPR, &REMOTE_UDP_REPR));
    }

    #[test]
    fn test_send_large_packet() {
        // buffer(4) creates a payload buffer of size 16*4
        let mut socket = socket(buffer(0), buffer(4));
        assert_eq!(socket.bind(LOCAL_END), Ok(()));

        let too_large = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdefx";
        assert_eq!(
            socket.send_slice(too_large, REMOTE_END),
            Err(SendError::BufferFull)
        );
        assert_eq!(socket.send_slice(&too_large[..16 * 4], REMOTE_END), Ok(()));
    }

    #[rstest]
    #[case::ip(Medium::Ip)]
    #[cfg(feature = "medium-ip")]
    #[case::ethernet(Medium::Ethernet)]
    #[cfg(feature = "medium-ethernet")]
    #[case::ieee802154(Medium::Ieee802154)]
    #[cfg(feature = "medium-ieee802154")]
    fn test_process_empty_payload(#[case] medium: Medium) {
        let meta = Box::leak(Box::new([PacketMetadata::EMPTY]));
        let recv_buffer = PacketBuffer::new(&mut meta[..], vec![]);
        let mut socket = socket(recv_buffer, buffer(0));

        let (mut iface, _, _) = setup(medium);
        let cx = iface.context();

        assert_eq!(socket.bind(LOCAL_PORT), Ok(()));

        let repr = UdpRepr {
            src_port: REMOTE_PORT,
            dst_port: LOCAL_PORT,
        };
        socket.process(cx, PacketMeta::default(), &REMOTE_IP_REPR, &repr, &[]);
        assert_eq!(socket.recv(), Ok((&[][..], REMOTE_END.into())));
    }

    #[test]
    fn test_closing() {
        let meta = Box::leak(Box::new([PacketMetadata::EMPTY]));
        let recv_buffer = PacketBuffer::new(&mut meta[..], vec![]);
        let mut socket = socket(recv_buffer, buffer(0));
        assert_eq!(socket.bind(LOCAL_PORT), Ok(()));

        assert!(socket.is_open());
        socket.close();
        assert!(!socket.is_open());
    }
}
