//! Host replay of the net daemon's receive side against access-point traffic.
//!
//! The daemon loop is modelled pass by pass (stack lookup, rx policy, smoltcp
//! poll, `rx_rearm`, park) around the real `rxpolicy.rs` and the vendored
//! smoltcp with the kernel's feature set. The frames are what a Wi-Fi AP puts
//! on the air right after association (brcmf hands them up as 802.3).

extern crate alloc;

#[path = "../../src/vifsel.rs"]
mod vifsel;
#[path = "../../src/rxpolicy.rs"]
pub mod rxpolicy;
#[path = "../../src/latestack.rs"]
pub mod latestack;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod latestack_tests;

pub mod frames {
    pub const AP: [u8; 6] = [0x02, 0xa0, 0xb1, 0xc2, 0xd3, 0xe4];
    pub const ME: [u8; 6] = [0xd8, 0x3a, 0xdd, 0x01, 0x02, 0x03];

    fn eth(dst: [u8; 6], src: [u8; 6], ty: u16, payload: &[u8]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&dst);
        f.extend_from_slice(&src);
        f.extend_from_slice(&ty.to_be_bytes());
        f.extend_from_slice(payload);
        f
    }

    fn csum(data: &[u8], init: u32) -> u16 {
        let mut s = init;
        for c in data.chunks(2) {
            s += (u16::from(c[0]) << 8 | c.get(1).map_or(0, |&b| u16::from(b))) as u32;
        }
        while s >> 16 != 0 { s = (s & 0xffff) + (s >> 16); }
        !(s as u16)
    }

    fn ipv4(src: [u8; 4], dst: [u8; 4], proto: u8, ttl: u8, frag: u16, opts: &[u8], payload: &[u8]) -> Vec<u8> {
        let hl = 20 + opts.len();
        let mut h = vec![0x40 | (hl / 4) as u8, 0];
        h.extend_from_slice(&((hl + payload.len()) as u16).to_be_bytes());
        h.extend_from_slice(&[0x12, 0x34]);
        h.extend_from_slice(&frag.to_be_bytes());
        h.extend_from_slice(&[ttl, proto, 0, 0]);
        h.extend_from_slice(&src);
        h.extend_from_slice(&dst);
        h.extend_from_slice(opts);
        let c = csum(&h, 0);
        h[10..12].copy_from_slice(&c.to_be_bytes());
        h.extend_from_slice(payload);
        h
    }

    fn udp4(src: [u8; 4], dst: [u8; 4], sport: u16, dport: u16, data: &[u8]) -> Vec<u8> {
        let mut u = Vec::new();
        u.extend_from_slice(&sport.to_be_bytes());
        u.extend_from_slice(&dport.to_be_bytes());
        u.extend_from_slice(&((8 + data.len()) as u16).to_be_bytes());
        u.extend_from_slice(&[0, 0]);
        u.extend_from_slice(data);
        let mut ph = Vec::new();
        ph.extend_from_slice(&src);
        ph.extend_from_slice(&dst);
        ph.extend_from_slice(&[0, 17]);
        ph.extend_from_slice(&(u.len() as u16).to_be_bytes());
        ph.extend_from_slice(&u);
        let c = csum(&ph, 0);
        u[6..8].copy_from_slice(&c.to_be_bytes());
        ipv4(src, dst, 17, 64, 0, &[], &u)
    }

    /// IPv6 + ICMPv6 with a correct checksum; `hbh` = hop-by-hop router alert.
    fn icmp6(dst_mac: [u8; 6], src: [u8; 16], dst: [u8; 16], hbh: bool, body: &[u8]) -> Vec<u8> {
        let mut icmp = body.to_vec();
        let mut ph = Vec::new();
        ph.extend_from_slice(&src);
        ph.extend_from_slice(&dst);
        ph.extend_from_slice(&(icmp.len() as u32).to_be_bytes());
        ph.extend_from_slice(&[0, 0, 0, 58]);
        ph.extend_from_slice(&icmp);
        let c = csum(&ph, 0);
        icmp[2..4].copy_from_slice(&c.to_be_bytes());
        let mut payload = Vec::new();
        if hbh { payload.extend_from_slice(&[58, 0, 5, 2, 0, 0, 1, 0]); }
        payload.extend_from_slice(&icmp);
        let mut ip = vec![0x60, 0, 0, 0];
        ip.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        ip.push(if hbh { 0 } else { 58 });
        ip.push(if hbh { 1 } else { 255 });
        ip.extend_from_slice(&src);
        ip.extend_from_slice(&dst);
        ip.extend_from_slice(&payload);
        eth(dst_mac, AP, 0x86dd, &ip)
    }

    const LL_AP: [u8; 16] = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0xa0, 0xb1, 0xff, 0xfe, 0xc2, 0xd3, 0xe4];
    const ALL_NODES: [u8; 16] = [0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

    pub fn arp_request() -> Vec<u8> {
        let mut a = vec![0, 1, 8, 0, 6, 4, 0, 1];
        a.extend_from_slice(&AP);
        a.extend_from_slice(&[192, 168, 1, 1]);
        a.extend_from_slice(&[0; 6]);
        a.extend_from_slice(&[192, 168, 1, 50]);
        a.resize(46, 0);
        eth([0xff; 6], AP, 0x0806, &a)
    }
    pub fn ipv6_router_advert() -> Vec<u8> {
        let mut b = vec![134, 0, 0, 0, 64, 0, 0x07, 0x08, 0, 0, 0, 0, 0, 0, 0, 0];
        b.extend_from_slice(&[1, 1]); // source link-layer address option
        b.extend_from_slice(&AP);
        b.extend_from_slice(&[3, 4, 64, 0xc0, 0, 0, 0x0e, 0x10, 0, 0, 0x07, 0x08, 0, 0, 0, 0]); // prefix info
        b.extend_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        icmp6([0x33, 0x33, 0, 0, 0, 1], LL_AP, ALL_NODES, false, &b)
    }
    pub fn mldv2_query() -> Vec<u8> {
        let b = [130, 0, 0, 0, 0x27, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 125, 0, 0];
        icmp6([0x33, 0x33, 0, 0, 0, 1], LL_AP, ALL_NODES, true, &b)
    }
    pub fn ipv6_neighbor_solicit_dad() -> Vec<u8> {
        let mut b = vec![135, 0, 0, 0, 0, 0, 0, 0];
        b.extend_from_slice(&LL_AP);
        let mut sn = [0u8; 16];
        sn[..2].copy_from_slice(&[0xff, 0x02]);
        sn[11..].copy_from_slice(&[1, 0xff, 0xc2, 0xd3, 0xe4]);
        icmp6([0x33, 0x33, 0xff, 0xc2, 0xd3, 0xe4], [0; 16], sn, false, &b)
    }
    pub fn stp_llc_snap() -> Vec<u8> {
        let mut b = vec![0x42, 0x42, 0x03, 0, 0, 0, 0, 0x80, 0];
        b.extend_from_slice(&AP);
        b.resize(38, 0);
        eth([0x01, 0x80, 0xc2, 0, 0, 0], AP, b.len() as u16, &b)
    }
    pub fn snap_cdp() -> Vec<u8> {
        let mut b = vec![0xaa, 0xaa, 0x03, 0x00, 0x00, 0x0c, 0x20, 0x00, 2, 180];
        b.resize(60, 0x11);
        eth([0x01, 0x00, 0x0c, 0xcc, 0xcc, 0xcc], AP, b.len() as u16, &b)
    }
    pub fn vlan_tagged_ipv4() -> Vec<u8> {
        let mut b = vec![0x00, 0x05, 0x08, 0x00];
        b.extend_from_slice(&udp4([192, 168, 5, 9], [255; 4], 5678, 5678, b"vlan"));
        eth([0xff; 6], AP, 0x8100, &b)
    }
    pub fn mdns_multicast() -> Vec<u8> {
        let p = udp4([192, 168, 1, 23], [224, 0, 0, 251], 5353, 5353, &[0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
        eth([0x01, 0x00, 0x5e, 0, 0, 0xfb], AP, 0x0800, &p)
    }
    pub fn ssdp_multicast() -> Vec<u8> {
        let p = udp4([192, 168, 1, 1], [239, 255, 255, 250], 1900, 1900, b"NOTIFY * HTTP/1.1\r\n\r\n");
        eth([0x01, 0x00, 0x5e, 0x7f, 0xff, 0xfa], AP, 0x0800, &p)
    }
    pub fn dhcp_offer_other_client() -> Vec<u8> {
        let mut d = vec![2, 1, 6, 0, 0xde, 0xad, 0xbe, 0xef];
        d.resize(240, 0);
        d[236..240].copy_from_slice(&[99, 130, 83, 99]);
        d.extend_from_slice(&[53, 1, 2, 255]);
        let p = udp4([192, 168, 1, 1], [255; 4], 67, 68, &d);
        eth([0xff; 6], AP, 0x0800, &p)
    }
    pub fn igmp_general_query() -> Vec<u8> {
        let mut q = vec![0x11, 100, 0, 0, 0, 0, 0, 0];
        let c = csum(&q, 0);
        q[2..4].copy_from_slice(&c.to_be_bytes());
        let p = ipv4([192, 168, 1, 1], [224, 0, 0, 1], 2, 1, 0, &[0x94, 4, 0, 0], &q);
        eth([0x01, 0x00, 0x5e, 0, 0, 1], AP, 0x0800, &p)
    }
    pub fn ipv4_first_fragment() -> Vec<u8> {
        let p = ipv4([192, 168, 1, 7], [255; 4], 17, 64, 0x2000, &[], &[0x55; 64]);
        eth([0xff; 6], AP, 0x0800, &p)
    }
    pub fn eapol_key() -> Vec<u8> { let mut b = vec![2, 3, 0, 95, 2]; b.resize(99, 0); eth(ME, AP, 0x888e, &b) }
    pub fn ipv4_truncated_header() -> Vec<u8> { eth([0xff; 6], AP, 0x0800, &[0x45, 0, 0, 20, 0, 0]) }
    pub fn odd_length_unknown() -> Vec<u8> { eth([0xff; 6], AP, 0x88b5, &[1, 2, 3, 4, 5]) }
    pub fn runt() -> Vec<u8> { vec![0xff; 10] }
    pub fn empty() -> Vec<u8> { Vec::new() }

    /// Every frame above, in the order a just-associated AP tends to send them.
    pub fn ap_burst() -> Vec<Vec<u8>> {
        vec![arp_request(), ipv6_router_advert(), mldv2_query(), ipv6_neighbor_solicit_dad(), stp_llc_snap(),
             snap_cdp(), vlan_tagged_ipv4(), mdns_multicast(), ssdp_multicast(), dhcp_offer_other_client(),
             igmp_general_query(), ipv4_first_fragment(), eapol_key(), ipv4_truncated_header(),
             odd_length_unknown(), runt(), empty()]
    }
}
