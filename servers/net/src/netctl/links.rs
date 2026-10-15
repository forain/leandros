//! The interface adapter: what `/dev/netctl` reports for the wired NICs and
//! loopback, read from the net server and the NIC registry.
//!
//! This is the one place netctl touches the IP stacks and `drivers::netdev`,
//! so lane L1 (one stack per interface, `drivers/src/netdev.rs` registry)
//! only has to rewrite [`collect`] and [`set_admin`]. Until it lands there is
//! exactly one NIC, `eth0`, behind `drivers::netdev`'s single-backend
//! dispatch, and its IPv4 state is the net server's single DHCP client.
//!
//! Wi-Fi interfaces are not listed here: they come from `drivers::wifi_api`
//! registrations (netctl/mod.rs). Their IPv4 state does come from here once
//! each interface has its own stack (L1); until then a Wi-Fi interface
//! reports `ip_state::NONE`.

use alloc::vec::Vec;
use netctl_abi::*;

/// One interface as the adapter sees it now.
#[derive(Clone, Copy)]
pub struct LinkView {
    pub iface: NcIface,
    pub ipv4: NcIpv4,
}

/// ifindex of the single wired NIC before the L1 registry.
pub const IFINDEX_ETH0: u32 = 2;

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

/// Every non-Wi-Fi interface, ifindex ascending (lo first).
pub fn collect(out: &mut Vec<LinkView>) {
    out.push(loopback());
    if !drivers::netdev::present() { return; }
    let mut i = NcIface::default();
    i.ifindex = IFINDEX_ETH0;
    i.kind = if_kind::ETHERNET;
    i.name = name16(b"eth0");
    let mac = drivers::netdev::mac_address().unwrap_or([0; 6]);
    i.mac = mac;
    i.perm_mac = mac;
    i.mtu = 1500;
    let (up, speed, _fd) = drivers::netdev::link();
    i.flags = iface_flags::ADMIN_UP | iface_flags::RADIO_ON | iface_flags::RADIO_HW_ON
        | iface_flags::READY | if up { iface_flags::CARRIER } else { 0 };
    i.oper = if up { oper::UP } else { oper::LOWER_DOWN };
    i.speed_mbps = speed;
    i.metric = METRIC_WIRED;
    i.link_gen = drivers::netdev::link_generation();
    i.driver = name16(drivers::netdev::driver_name().as_bytes());

    let mut v = NcIpv4::default();
    v.ifindex = IFINDEX_ETH0;
    v.method = ip_method::AUTO;
    v.metric = METRIC_WIRED;
    match crate::procnet::dhcp_lease() {
        Some((addr, prefix, router, dns, n_dns)) => {
            v.state = ip_state::BOUND;
            v.addr = addr;
            v.prefix = prefix;
            if let Some(g) = router { v.gw = g; v.flags |= ip_flags::HAS_GW; }
            v.dns = dns;
            v.n_dns = n_dns;
        }
        None => v.state = if up { ip_state::REQUESTING } else { ip_state::NONE },
    }
    out.push(LinkView { iface: i, ipv4: v });
}

/// Wired admin up/down (`NETCTL_CONNECT` / `NETCTL_DISCONNECT` on a wired
/// ifindex). The single-stack net server cannot take its only NIC down, so
/// "up" is a no-op success and "down" is refused until L1.
pub fn set_admin(ifindex: u32, up: bool) -> Result<(), i32> {
    if ifindex != IFINDEX_ETH0 || !drivers::netdev::present() { return Err(errno::ENODEV); }
    if up { Ok(()) } else { Err(errno::EOPNOTSUPP) }
}

/// `NcVersion.features` bits this adapter provides.
pub const FEATURES: u32 = 0;
