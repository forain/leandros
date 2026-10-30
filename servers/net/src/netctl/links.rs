//! The interface adapter: what `/dev/netctl` reports for each netif, read
//! from the net server's per-interface state (`crate::iface_snapshot`, the
//! `drivers::netdev` registry; servers/net/src/iface.rs).
//!
//! This is the one place netctl touches the IP stacks. Every registered
//! netif (wired `ethN` and Wi-Fi `wifiN`, ifindex = registry id + 2) plus
//! `lo` (ifindex 1) is listed, with its IPv4 state. For a Wi-Fi netif the
//! oper state and flags are overridden from the Wi-Fi backend's link state
//! in netctl/mod.rs once a `drivers::wifi_api` backend is registered for it.

use alloc::vec::Vec;
use drivers::netdev;
use netctl_abi::*;

/// One interface as the adapter sees it now.
#[derive(Clone, Copy)]
pub struct LinkView {
    pub iface: NcIface,
    pub ipv4: NcIpv4,
}

fn name16(s: &[u8]) -> [u8; IFNAMSIZ] {
    let mut n = [0u8; IFNAMSIZ];
    let k = s.len().min(IFNAMSIZ - 1);
    n[..k].copy_from_slice(&s[..k]);
    n
}

/// Loopback: always present, ifindex 1, 127.0.0.1/8.
fn loopback() -> LinkView {
    let mut i = NcIface::default();
    i.ifindex = IFINDEX_LO;
    i.kind = if_kind::LOOPBACK;
    i.oper = oper::UP;
    i.name = name16(b"lo");
    i.mtu = 65535;
    i.flags = iface_flags::ADMIN_UP | iface_flags::CARRIER | iface_flags::RADIO_ON
        | iface_flags::RADIO_HW_ON | iface_flags::READY;
    i.driver = name16(b"loopback");
    let mut v = NcIpv4::default();
    v.ifindex = IFINDEX_LO;
    v.state = ip_state::STATIC;
    v.method = ip_method::LOOPBACK;
    v.addr = [127, 0, 0, 1];
    v.prefix = 8;
    LinkView { iface: i, ipv4: v }
}

/// Every interface, ifindex ascending (lo first).
pub fn collect(out: &mut Vec<LinkView>) {
    out.push(loopback());
    for s in crate::iface_snapshot() {
        let ifindex = s.id as u32 + 2;
        let wifi = s.kind == netdev::Kind::Wifi;
        let mut i = NcIface::default();
        i.ifindex = ifindex;
        i.kind = if wifi { if_kind::WIFI } else { if_kind::ETHERNET };
        i.name = name16(s.name.as_bytes());
        let mac = s.mac.unwrap_or([0; 6]);
        i.mac = mac;
        i.perm_mac = mac;
        i.mtu = 1500;
        i.flags = iface_flags::RADIO_ON | iface_flags::RADIO_HW_ON
            | if s.admin_up { iface_flags::ADMIN_UP } else { 0 }
            | if wifi { 0 } else { iface_flags::READY }
            | if s.carrier { iface_flags::CARRIER } else { 0 };
        i.oper = if !s.admin_up { oper::DOWN } else if s.carrier { oper::UP } else { oper::LOWER_DOWN };
        i.speed_mbps = s.speed;
        i.metric = s.metric;
        i.link_gen = netdev::get(s.id as usize).map_or(0, |n| n.link_generation());
        i.driver = name16(s.driver.as_bytes());

        let mut v = NcIpv4::default();
        v.ifindex = ifindex;
        v.method = ip_method::AUTO;
        v.metric = s.metric;
        match s.addr {
            Some((addr, prefix)) if s.leased => {
                v.state = ip_state::BOUND;
                v.addr = addr;
                v.prefix = prefix;
                if let Some(g) = s.gateway { v.gw = g; v.flags |= ip_flags::HAS_GW; }
                let n = s.dns.len().min(3);
                v.dns[..n].copy_from_slice(&s.dns[..n]);
                v.n_dns = n as u8;
            }
            _ => v.state = if s.carrier && s.admin_up { ip_state::REQUESTING } else { ip_state::NONE },
        }
        if s.default_route { v.flags |= ip_flags::DEFAULT_ROUTE; }
        out.push(LinkView { iface: i, ipv4: v });
    }
}

/// Wired admin up/down (`NETCTL_CONNECT` / `NETCTL_DISCONNECT` on a wired
/// ifindex). Down stops the interface's DHCP client and removes its address
/// and routes at once (TCP on it is reset); up restarts DHCP. The net daemon
/// applies it on its next pass and reports it through the iface hook, so
/// `IFACE_CHANGED` (ADMIN_UP) and `IPV4` follow within milliseconds. Setting
/// the state it already has is a successful no-op.
pub fn set_admin(ifindex: u32, up: bool) -> Result<(), i32> {
    let ok = ifindex >= 2 && netdev::get(ifindex as usize - 2).map_or(false, |n| n.kind == netdev::Kind::Wired);
    if !ok { return Err(errno::ENODEV); }
    if crate::set_iface_admin((ifindex - 2) as u8, up) { Ok(()) } else { Err(errno::ENODEV) }
}

/// `NETCTL_SET_METRIC`: `metric == 0` restores the default.
pub fn set_metric(ifindex: u32, metric: u32) -> Result<(), i32> {
    if ifindex < 2 { return Err(errno::ENODEV); }
    if crate::iface::set_metric((ifindex - 2) as u8, if metric == 0 { None } else { Some(metric) }) { Ok(()) } else { Err(errno::ENODEV) }
}

/// `NcVersion.features` bits this adapter provides.
pub const FEATURES: u32 = features::WIRED_ADMIN;
