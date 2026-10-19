// Heads up! Before working on this file you should read, at least,
// the parts of RFC 1122 that discuss ARP.

use core::sync::atomic::{AtomicU64, Ordering};
use heapless::LinearMap;

use crate::config::IFACE_NEIGHBOR_CACHE_COUNT;
use crate::time::{Duration, Instant};
use crate::wire::{HardwareAddress, IpAddress};

/// A cached neighbor.
///
/// A neighbor mapping translates from a protocol address to a hardware address,
/// and contains the timestamp past which the mapping should be discarded.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Neighbor {
    hardware_addr: HardwareAddress,
    expires_at: Instant,
}

/// An answer to a neighbor cache lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub(crate) enum Answer {
    /// The neighbor address is in the cache and not expired.
    Found(HardwareAddress),
    /// The neighbor address is not in the cache, or has expired.
    NotFound,
    /// The neighbor address is not in the cache, or has expired,
    /// and a lookup has been made recently.
    RateLimited,
}

impl Answer {
    /// Returns whether a valid address was found.
    pub(crate) fn found(&self) -> bool {
        match self {
            Answer::Found(_) => true,
            _ => false,
        }
    }
}

/// LEANDROS: neighbor cache counters over all interfaces: [packets dropped
/// for an unresolved neighbor (NeighborPending), sends through a stale
/// entry, refresh requests sent, entries refreshed by traffic].
pub static NEIGHBOR_STATS: [AtomicU64; 4] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
pub(crate) const NS_PENDING: usize = 0;
pub(crate) const NS_STALE: usize = 1;
pub(crate) const NS_PROBE: usize = 2;
pub(crate) const NS_REFRESH: usize = 3;
pub(crate) fn neighbor_stat(i: usize) {
    NEIGHBOR_STATS[i].fetch_add(1, Ordering::Relaxed);
}

/// A neighbor cache backed by a map.
#[derive(Debug)]
pub struct Cache {
    storage: LinearMap<IpAddress, Neighbor, IFACE_NEIGHBOR_CACHE_COUNT>,
    silent_until: Instant,
    // LEANDROS: an expired entry that was used (see `lookup_stale`) and
    // wants a refresh request: (our source address, the neighbor).
    probe: Option<(IpAddress, IpAddress)>,
    probe_silent_until: Instant,
}

impl Cache {
    /// Minimum delay between discovery requests, in milliseconds.
    pub(crate) const SILENT_TIME: Duration = Duration::from_millis(1_000);

    /// Neighbor entry lifetime, in milliseconds.
    pub(crate) const ENTRY_LIFETIME: Duration = Duration::from_millis(60_000);

    /// LEANDROS: how long past its expiry an entry is still used (Linux's
    /// NUD_STALE) while a refresh request is out. Upstream drops every
    /// packet to a neighbor whose entry expired and sends a request
    /// instead, once a minute for a busy peer: one lost ICMP echo reply
    /// a minute (1-2 % of 1 s pings), one lost TCP segment and an RTO
    /// stall a minute.
    pub(crate) const STALE_LIFETIME: Duration = Duration::from_millis(60_000);

    /// Create a cache.
    pub fn new() -> Self {
        Self {
            storage: LinearMap::new(),
            silent_until: Instant::from_millis(0),
            probe: None,
            probe_silent_until: Instant::from_millis(0),
        }
    }

    /// LEANDROS: a frame came from `hardware_addr`, so every entry that
    /// maps to it is reachable and lives on (upstream's later
    /// `reset_expiry_if_existing`, which only covers on-link sources; this
    /// also covers the router of traffic from off-link hosts). Only ARP /
    /// NDISC replace a mapping.
    pub(crate) fn refresh(&mut self, hardware_addr: HardwareAddress, timestamp: Instant) {
        let e = timestamp + Self::ENTRY_LIFETIME;
        for (_, n) in self.storage.iter_mut() {
            if n.hardware_addr == hardware_addr && e > n.expires_at {
                // Count only refreshes that saved an entry from expiring
                // within the next 30 s (not one per frame).
                if n.expires_at < timestamp + Duration::from_millis(30_000) {
                    neighbor_stat(NS_REFRESH);
                }
                n.expires_at = e;
            }
        }
    }

    /// LEANDROS: the hardware address of an entry that expired less than
    /// STALE_LIFETIME ago. The caller sends with it and asks for a
    /// refresh (`want_probe`).
    pub(crate) fn lookup_stale(
        &self,
        protocol_addr: &IpAddress,
        timestamp: Instant,
    ) -> Option<HardwareAddress> {
        let n = self.storage.get(protocol_addr)?;
        if timestamp < n.expires_at + Self::STALE_LIFETIME {
            Some(n.hardware_addr)
        } else {
            None
        }
    }

    /// LEANDROS: ask for a refresh request to `neighbor` (sent from `src`)
    /// on the next poll; at most one a second.
    pub(crate) fn want_probe(&mut self, src: IpAddress, neighbor: IpAddress, timestamp: Instant) {
        if self.probe.is_none() && timestamp >= self.probe_silent_until {
            self.probe = Some((src, neighbor));
        }
    }

    /// LEANDROS: the pending refresh request, if any (taken).
    pub(crate) fn take_probe(&mut self, timestamp: Instant) -> Option<(IpAddress, IpAddress)> {
        let p = self.probe.take()?;
        self.probe_silent_until = timestamp + Self::SILENT_TIME;
        Some(p)
    }

    /// LEANDROS: put back a refresh request the device had no room for.
    pub(crate) fn return_probe(&mut self, p: (IpAddress, IpAddress)) {
        if self.probe.is_none() {
            self.probe = Some(p);
        }
    }

    pub fn fill(
        &mut self,
        protocol_addr: IpAddress,
        hardware_addr: HardwareAddress,
        timestamp: Instant,
    ) {
        debug_assert!(protocol_addr.is_unicast());
        debug_assert!(hardware_addr.is_unicast());

        let expires_at = timestamp + Self::ENTRY_LIFETIME;
        self.fill_with_expiration(protocol_addr, hardware_addr, expires_at);
    }

    pub fn fill_with_expiration(
        &mut self,
        protocol_addr: IpAddress,
        hardware_addr: HardwareAddress,
        expires_at: Instant,
    ) {
        debug_assert!(protocol_addr.is_unicast());
        debug_assert!(hardware_addr.is_unicast());

        let neighbor = Neighbor {
            expires_at,
            hardware_addr,
        };
        match self.storage.insert(protocol_addr, neighbor) {
            Ok(Some(old_neighbor)) => {
                if old_neighbor.hardware_addr != hardware_addr {
                    net_trace!(
                        "replaced {} => {} (was {})",
                        protocol_addr,
                        hardware_addr,
                        old_neighbor.hardware_addr
                    );
                }
            }
            Ok(None) => {
                net_trace!("filled {} => {} (was empty)", protocol_addr, hardware_addr);
            }
            Err((protocol_addr, neighbor)) => {
                // If we're going down this branch, it means the cache is full, and we need to evict an entry.
                let old_protocol_addr = *self
                    .storage
                    .iter()
                    .min_by_key(|(_, neighbor)| neighbor.expires_at)
                    .expect("empty neighbor cache storage")
                    .0;

                let _old_neighbor = self.storage.remove(&old_protocol_addr).unwrap();
                match self.storage.insert(protocol_addr, neighbor) {
                    Ok(None) => {
                        net_trace!(
                            "filled {} => {} (evicted {} => {})",
                            protocol_addr,
                            hardware_addr,
                            old_protocol_addr,
                            _old_neighbor.hardware_addr
                        );
                    }
                    // We've covered everything else above.
                    _ => unreachable!(),
                }
            }
        }
    }

    pub(crate) fn lookup(&self, protocol_addr: &IpAddress, timestamp: Instant) -> Answer {
        assert!(protocol_addr.is_unicast());

        if let Some(&Neighbor {
            expires_at,
            hardware_addr,
        }) = self.storage.get(protocol_addr)
        {
            if timestamp < expires_at {
                return Answer::Found(hardware_addr);
            }
        }

        if timestamp < self.silent_until {
            Answer::RateLimited
        } else {
            Answer::NotFound
        }
    }

    pub(crate) fn limit_rate(&mut self, timestamp: Instant) {
        self.silent_until = timestamp + Self::SILENT_TIME;
    }

    pub(crate) fn flush(&mut self) {
        self.storage.clear();
        self.probe = None;
    }
}

#[cfg(feature = "medium-ethernet")]
#[cfg(test)]
mod test {
    use super::*;
    use crate::wire::ip::test::{MOCK_IP_ADDR_1, MOCK_IP_ADDR_2, MOCK_IP_ADDR_3, MOCK_IP_ADDR_4};

    use crate::wire::EthernetAddress;

    const HADDR_A: HardwareAddress = HardwareAddress::Ethernet(EthernetAddress([0, 0, 0, 0, 0, 1]));
    const HADDR_B: HardwareAddress = HardwareAddress::Ethernet(EthernetAddress([0, 0, 0, 0, 0, 2]));
    const HADDR_C: HardwareAddress = HardwareAddress::Ethernet(EthernetAddress([0, 0, 0, 0, 0, 3]));
    const HADDR_D: HardwareAddress = HardwareAddress::Ethernet(EthernetAddress([0, 0, 0, 0, 0, 4]));

    #[test]
    fn test_fill() {
        let mut cache = Cache::new();

        assert!(!cache
            .lookup(&MOCK_IP_ADDR_1, Instant::from_millis(0))
            .found());
        assert!(!cache
            .lookup(&MOCK_IP_ADDR_2, Instant::from_millis(0))
            .found());

        cache.fill(MOCK_IP_ADDR_1, HADDR_A, Instant::from_millis(0));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(0)),
            Answer::Found(HADDR_A)
        );
        assert!(!cache
            .lookup(&MOCK_IP_ADDR_2, Instant::from_millis(0))
            .found());
        assert!(!cache
            .lookup(
                &MOCK_IP_ADDR_1,
                Instant::from_millis(0) + Cache::ENTRY_LIFETIME * 2
            )
            .found(),);

        cache.fill(MOCK_IP_ADDR_1, HADDR_A, Instant::from_millis(0));
        assert!(!cache
            .lookup(&MOCK_IP_ADDR_2, Instant::from_millis(0))
            .found());
    }

    #[test]
    fn test_expire() {
        let mut cache = Cache::new();

        cache.fill(MOCK_IP_ADDR_1, HADDR_A, Instant::from_millis(0));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(0)),
            Answer::Found(HADDR_A)
        );
        assert!(!cache
            .lookup(
                &MOCK_IP_ADDR_1,
                Instant::from_millis(0) + Cache::ENTRY_LIFETIME * 2
            )
            .found(),);
    }

    #[test]
    fn test_replace() {
        let mut cache = Cache::new();

        cache.fill(MOCK_IP_ADDR_1, HADDR_A, Instant::from_millis(0));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(0)),
            Answer::Found(HADDR_A)
        );
        cache.fill(MOCK_IP_ADDR_1, HADDR_B, Instant::from_millis(0));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(0)),
            Answer::Found(HADDR_B)
        );
    }

    #[test]
    fn test_evict() {
        let mut cache = Cache::new();

        cache.fill(MOCK_IP_ADDR_1, HADDR_A, Instant::from_millis(100));
        cache.fill(MOCK_IP_ADDR_2, HADDR_B, Instant::from_millis(50));
        cache.fill(MOCK_IP_ADDR_3, HADDR_C, Instant::from_millis(200));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_2, Instant::from_millis(1000)),
            Answer::Found(HADDR_B)
        );
        assert!(!cache
            .lookup(&MOCK_IP_ADDR_4, Instant::from_millis(1000))
            .found());

        cache.fill(MOCK_IP_ADDR_4, HADDR_D, Instant::from_millis(300));
        assert!(!cache
            .lookup(&MOCK_IP_ADDR_2, Instant::from_millis(1000))
            .found());
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_4, Instant::from_millis(1000)),
            Answer::Found(HADDR_D)
        );
    }

    #[test]
    fn test_hush() {
        let mut cache = Cache::new();

        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(0)),
            Answer::NotFound
        );

        cache.limit_rate(Instant::from_millis(0));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(100)),
            Answer::RateLimited
        );
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(2000)),
            Answer::NotFound
        );
    }

    #[test]
    fn test_flush() {
        let mut cache = Cache::new();

        cache.fill(MOCK_IP_ADDR_1, HADDR_A, Instant::from_millis(0));
        assert_eq!(
            cache.lookup(&MOCK_IP_ADDR_1, Instant::from_millis(0)),
            Answer::Found(HADDR_A)
        );
        assert!(!cache
            .lookup(&MOCK_IP_ADDR_2, Instant::from_millis(0))
            .found());

        cache.flush();
        assert!(!cache
            .lookup(&MOCK_IP_ADDR_1, Instant::from_millis(0))
            .found());
        assert!(!cache
            .lookup(&MOCK_IP_ADDR_1, Instant::from_millis(0))
            .found());
    }
}
