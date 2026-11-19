use super::*;

use crate::phy::{Device, PacketMeta};
use crate::time::{Duration, Instant};
use crate::wire::*;

use core::result::Result;

/// Error type for `join_multicast_group`, `leave_multicast_group`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum MulticastError {
    /// The hardware device transmit buffer is full. Try again later.
    Exhausted,
    /// The table of joined multicast groups is already full.
    GroupTableFull,
    /// IPv6 multicast is not yet supported.
    Ipv6NotSupported,
}

impl core::fmt::Display for MulticastError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            MulticastError::Exhausted => write!(f, "Exhausted"),
            MulticastError::GroupTableFull => write!(f, "GroupTableFull"),
            MulticastError::Ipv6NotSupported => write!(f, "Ipv6NotSupported"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for MulticastError {}

/// LEANDROS: the net server's IGMPv3 query handler (0 = none):
/// `fn(dst: [u8; 4], igmp: &[u8]) -> bool`, called under the stack lock, so
/// it may only take leaf locks.
pub static IGMP_QUERY_HOOK: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Querier-present timeout of RFC 3376 8.12 with the default robustness
/// (2), query interval (125 s) and response interval (10 s).
const IGMP_COMPAT_MS: u64 = 260_000;

impl InterfaceInner {
    /// LEANDROS: does this interface speak IGMPv3 right now (no older
    /// querier heard)?
    pub(crate) fn igmp_v3_active(&self) -> bool {
        self.igmp_v3 && self.igmp_compat.map_or(true, |(_, until)| self.now >= until)
    }
}

impl Interface {
    /// LEANDROS: speak IGMPv3 through the net server's hook (default off).
    pub fn set_igmp_v3(&mut self, on: bool) {
        self.inner.igmp_v3 = on;
        self.inner.igmp_compat = None;
    }

    /// LEANDROS: the IGMP version in force (3, 2 or 1; 2 when the stack
    /// does not speak v3 at all).
    pub fn igmp_version(&mut self, now: Instant) -> u8 {
        self.inner.now = now;
        if !self.inner.igmp_v3 { return 2; }
        match self.inner.igmp_compat {
            Some((v, until)) if now < until => if v == IgmpVersion::Version1 { 1 } else { 2 },
            _ => 3,
        }
    }

    /// Add an address to a list of subscribed multicast IP addresses.
    ///
    /// Returns `Ok(announce_sent)` if the address was added successfully, where `announce_sent`
    /// indicates whether an initial immediate announcement has been sent.
    pub fn join_multicast_group<D, T: Into<IpAddress>>(
        &mut self,
        device: &mut D,
        addr: T,
        timestamp: Instant,
    ) -> Result<bool, MulticastError>
    where
        D: Device + ?Sized,
    {
        self.inner.now = timestamp;

        match addr.into() {
            IpAddress::Ipv4(addr) => {
                let is_not_new = self
                    .inner
                    .ipv4_multicast_groups
                    .insert(addr, ())
                    .map_err(|_| MulticastError::GroupTableFull)?
                    .is_some();
                if is_not_new {
                    Ok(false)
                } else if self.inner.igmp_v3_active() {
                    Ok(false)
                } else if let Some(pkt) = self.inner.igmp_report_packet(IgmpVersion::Version2, addr)
                {
                    // Send initial membership report
                    let tx_token = device
                        .transmit(timestamp)
                        .ok_or(MulticastError::Exhausted)?;

                    // NOTE(unwrap): packet destination is multicast, which is always routable and doesn't require neighbor discovery.
                    self.inner
                        .dispatch_ip(tx_token, PacketMeta::default(), pkt, &mut self.fragmenter)
                        .unwrap();

                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            #[cfg(feature = "proto-ipv6")]
            IpAddress::Ipv6(addr) => {
                self.inner
                    .ipv6_multicast_groups
                    .insert(addr, ())
                    .map_err(|_| MulticastError::GroupTableFull)?;
                Ok(false)
            }
            // Multicast is not yet implemented for other address families
            #[allow(unreachable_patterns)]
            _ => Err(MulticastError::Ipv6NotSupported),
        }
    }

    /// Remove an address from the subscribed multicast IP addresses.
    ///
    /// Returns `Ok(leave_sent)` if the address was removed successfully, where `leave_sent`
    /// indicates whether an immediate leave packet has been sent.
    pub fn leave_multicast_group<D, T: Into<IpAddress>>(
        &mut self,
        device: &mut D,
        addr: T,
        timestamp: Instant,
    ) -> Result<bool, MulticastError>
    where
        D: Device + ?Sized,
    {
        self.inner.now = timestamp;

        match addr.into() {
            IpAddress::Ipv4(addr) => {
                let was_not_present = self.inner.ipv4_multicast_groups.remove(&addr).is_none();
                if was_not_present {
                    Ok(false)
                } else if self.inner.igmp_v3_active() {
                    Ok(false)
                } else if let Some(pkt) = self.inner.igmp_leave_packet(addr) {
                    // Send group leave packet
                    let tx_token = device
                        .transmit(timestamp)
                        .ok_or(MulticastError::Exhausted)?;

                    // NOTE(unwrap): packet destination is multicast, which is always routable and doesn't require neighbor discovery.
                    self.inner
                        .dispatch_ip(tx_token, PacketMeta::default(), pkt, &mut self.fragmenter)
                        .unwrap();

                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            #[cfg(feature = "proto-ipv6")]
            IpAddress::Ipv6(addr) => {
                self.inner.ipv6_multicast_groups.remove(&addr);
                Ok(false)
            }
            // Multicast is not yet implemented for other address families
            #[allow(unreachable_patterns)]
            _ => Err(MulticastError::Ipv6NotSupported),
        }
    }

    /// Check whether the interface listens to given destination multicast IP address.
    pub fn has_multicast_group<T: Into<IpAddress>>(&self, addr: T) -> bool {
        self.inner.has_multicast_group(addr)
    }

    /// Depending on `igmp_report_state` and the therein contained
    /// timeouts, send IGMP membership reports.
    pub(crate) fn igmp_egress<D>(&mut self, device: &mut D) -> bool
    where
        D: Device + ?Sized,
    {
        match self.inner.igmp_report_state {
            IgmpReportState::ToSpecificQuery {
                version,
                timeout,
                group,
            } if self.inner.now >= timeout => {
                if let Some(pkt) = self.inner.igmp_report_packet(version, group) {
                    // Send initial membership report
                    if let Some(tx_token) = device.transmit(self.inner.now) {
                        // NOTE(unwrap): packet destination is multicast, which is always routable and doesn't require neighbor discovery.
                        self.inner
                            .dispatch_ip(tx_token, PacketMeta::default(), pkt, &mut self.fragmenter)
                            .unwrap();
                    } else {
                        return false;
                    }
                }

                self.inner.igmp_report_state = IgmpReportState::Inactive;
                true
            }
            IgmpReportState::ToGeneralQuery {
                version,
                timeout,
                interval,
                next_index,
            } if self.inner.now >= timeout => {
                let addr = self
                    .inner
                    .ipv4_multicast_groups
                    .iter()
                    .nth(next_index)
                    .map(|(addr, ())| *addr);

                match addr {
                    Some(addr) => {
                        if let Some(pkt) = self.inner.igmp_report_packet(version, addr) {
                            // Send initial membership report
                            if let Some(tx_token) = device.transmit(self.inner.now) {
                                // NOTE(unwrap): packet destination is multicast, which is always routable and doesn't require neighbor discovery.
                                self.inner
                                    .dispatch_ip(
                                        tx_token,
                                        PacketMeta::default(),
                                        pkt,
                                        &mut self.fragmenter,
                                    )
                                    .unwrap();
                            } else {
                                return false;
                            }
                        }

                        let next_timeout = (timeout + interval).max(self.inner.now);
                        self.inner.igmp_report_state = IgmpReportState::ToGeneralQuery {
                            version,
                            timeout: next_timeout,
                            interval,
                            next_index: next_index + 1,
                        };
                        true
                    }

                    None => {
                        self.inner.igmp_report_state = IgmpReportState::Inactive;
                        false
                    }
                }
            }
            _ => false,
        }
    }
}

impl InterfaceInner {
    /// Host duties of the **IGMPv2** protocol.
    ///
    /// Sets up `igmp_report_state` for responding to IGMP general/specific membership queries.
    /// Membership must not be reported immediately in order to avoid flooding the network
    /// after a query is broadcasted by a router; this is not currently done.
    pub(super) fn process_igmp<'frame>(
        &mut self,
        ipv4_repr: Ipv4Repr,
        ip_payload: &'frame [u8],
    ) -> Option<Packet<'frame>> {
        // LEANDROS: an IGMPv3 query (12 bytes or more) is the net server's.
        if self.igmp_v3_active() && ip_payload.len() >= 12 && ip_payload[0] == 0x11 {
            let h = IGMP_QUERY_HOOK.load(core::sync::atomic::Ordering::Relaxed);
            if h != 0 {
                // SAFETY: only ever stored from a `fn([u8; 4], &[u8]) -> bool`.
                #[allow(unsafe_code)]
                let f: fn([u8; 4], &[u8]) -> bool = unsafe { core::mem::transmute(h) };
                if f(ipv4_repr.dst_addr.0, ip_payload) { return None; }
            }
        }
        let igmp_packet = check!(IgmpPacket::new_checked(ip_payload));
        let igmp_repr = check!(IgmpRepr::parse(&igmp_packet));

        // FIXME: report membership after a delay
        match igmp_repr {
            IgmpRepr::MembershipQuery {
                group_addr,
                version,
                max_resp_time,
            } => {
                if self.igmp_v3 {
                    // An older querier: report its way until it is silent
                    // for the querier-present timeout (RFC 3376 8.12).
                    let until = self.now + Duration::from_millis(IGMP_COMPAT_MS);
                    let v = match self.igmp_compat {
                        Some((IgmpVersion::Version1, u)) if self.now < u => IgmpVersion::Version1,
                        _ => version,
                    };
                    self.igmp_compat = Some((v, until));
                }
                // General query
                if group_addr.is_unspecified()
                    && ipv4_repr.dst_addr == Ipv4Address::MULTICAST_ALL_SYSTEMS
                {
                    // Are we member in any groups?
                    if self.ipv4_multicast_groups.iter().next().is_some() {
                        let interval = match version {
                            IgmpVersion::Version1 => Duration::from_millis(100),
                            IgmpVersion::Version2 => {
                                // No dependence on a random generator
                                // (see [#24](https://github.com/m-labs/smoltcp/issues/24))
                                // but at least spread reports evenly across max_resp_time.
                                let intervals = self.ipv4_multicast_groups.len() as u32 + 1;
                                max_resp_time / intervals
                            }
                        };
                        self.igmp_report_state = IgmpReportState::ToGeneralQuery {
                            version,
                            timeout: self.now + interval,
                            interval,
                            next_index: 0,
                        };
                    }
                } else {
                    // Group-specific query
                    if self.has_multicast_group(group_addr) && ipv4_repr.dst_addr == group_addr {
                        // Don't respond immediately
                        let timeout = max_resp_time / 4;
                        self.igmp_report_state = IgmpReportState::ToSpecificQuery {
                            version,
                            timeout: self.now + timeout,
                            group: group_addr,
                        };
                    }
                }
            }
            // Ignore membership reports
            IgmpRepr::MembershipReport { .. } => (),
            // Ignore hosts leaving groups
            IgmpRepr::LeaveGroup { .. } => (),
        }

        None
    }
}
