//! Tests for the 802.11 frame module (drivers/src/wifi_frame.rs).

use crate::wifi_frame::*;

const BSSID: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
const OWN: [u8; 6] = [0x00, 0xaa, 0xbb, 0xcc, 0xdd, 0xee];
const PEER: [u8; 6] = [0x10, 0x20, 0x30, 0x40, 0x50, 0x60];

fn ie(id: u8, data: &[u8]) -> Vec<u8> {
    let mut v = vec![id, data.len() as u8];
    v.extend_from_slice(data);
    v
}

fn rsn_ie(group: u32, pairwise: &[u32], akm: &[u32]) -> Vec<u8> {
    let mut d = vec![1, 0];
    d.extend_from_slice(&group.to_be_bytes());
    d.extend_from_slice(&(pairwise.len() as u16).to_le_bytes());
    for p in pairwise {
        d.extend_from_slice(&p.to_be_bytes());
    }
    d.extend_from_slice(&(akm.len() as u16).to_le_bytes());
    for a in akm {
        d.extend_from_slice(&a.to_be_bytes());
    }
    d.extend_from_slice(&[0x0c, 0x00]);
    ie(48, &d)
}

fn beacon_with(subtype_fc: u16, cap: u16, ies: &[u8]) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&subtype_fc.to_le_bytes());
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(&[0xff; 6]);
    f.extend_from_slice(&BSSID);
    f.extend_from_slice(&BSSID);
    f.extend_from_slice(&[0x10, 0x20]);
    f.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
    f.extend_from_slice(&100u16.to_le_bytes());
    f.extend_from_slice(&cap.to_le_bytes());
    f.extend_from_slice(ies);
    f
}

fn beacon(cap: u16, ies: &[u8]) -> Vec<u8> {
    beacon_with(0x0080, cap, ies)
}

const CCMP: u32 = 0x000f_ac04;
const TKIP: u32 = 0x000f_ac02;
const PSK: u32 = 0x000f_ac02;
const EAP: u32 = 0x000f_ac01;
const SAE: u32 = 0x000f_ac08;

fn full_beacon() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend(ie(0, b"TestNet"));
    b.extend(ie(1, &[0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24]));
    b.extend(ie(3, &[6]));
    b.extend(ie(5, &[0, 2, 0, 0]));
    b.extend(ie(42, &[0]));
    b.extend(ie(50, &[0x30, 0x48, 0x60, 0x6c]));
    b.extend(rsn_ie(CCMP, &[CCMP], &[PSK]));
    b.extend(ie(45, &[0x6f; 26]));
    let mut ht_op = vec![6u8];
    ht_op.extend_from_slice(&[0u8; 21]);
    b.extend(ie(61, &ht_op));
    beacon(0x0411, &b)
}

#[test]
fn beacon_full() {
    let f = full_beacon();
    let b = parse_beacon(&f, 1).unwrap();
    assert_eq!(b.bssid, BSSID);
    assert_eq!(b.ssid, b"TestNet");
    assert_eq!(b.channel, 6);
    assert_eq!(b.beacon_interval, 100);
    assert_eq!(b.capability, 0x0411);
    assert_eq!(b.tsf, 0x0102_0304_0506_0708);
    assert_eq!(
        b.rates,
        vec![0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24, 0x30, 0x48, 0x60, 0x6c]
    );
    let r = b.rsn.as_ref().unwrap();
    assert_eq!(r.version, 1);
    assert_eq!(r.group, CCMP);
    assert_eq!(r.pairwise, vec![CCMP]);
    assert_eq!(r.akm, vec![PSK]);
    assert_eq!(r.caps, 0x000c);
    assert!(!b.wpa1);
    assert!(b.ht);
    assert!(!b.vht);
    assert_eq!(b.dtim_period, 2);
    assert_eq!(b.rssi, 0);
    assert_eq!(security_summary(&b), "WPA2-PSK");
    assert!(supports_wpa2_psk_ccmp(&b));
    assert_eq!(ssid_display(&b.ssid), "TestNet");
}

#[test]
fn probe_response_parses_like_beacon() {
    let f = beacon_with(0x0050, 0x0401, &ie(0, b"x"));
    let b = parse_beacon(&f, 11).unwrap();
    assert_eq!(b.ssid, b"x");
    assert_eq!(b.channel, 11);
    // Not a beacon / probe response.
    let f = beacon_with(0x00b0, 0, &[]);
    assert!(parse_beacon(&f, 1).is_none());
}

#[test]
fn security_variants() {
    let mut open = Vec::new();
    open.extend(ie(0, b"cafe"));
    let b = parse_beacon(&beacon(0x0401, &open), 1).unwrap();
    assert_eq!(security_summary(&b), "open");
    assert!(!supports_wpa2_psk_ccmp(&b));

    let b = parse_beacon(&beacon(0x0411, &open), 1).unwrap();
    assert_eq!(security_summary(&b), "WEP");

    let mut wpa = open.clone();
    wpa.extend(ie(221, &[0x00, 0x50, 0xf2, 0x01, 0x01, 0x00]));
    let b = parse_beacon(&beacon(0x0411, &wpa), 1).unwrap();
    assert!(b.wpa1);
    assert_eq!(security_summary(&b), "WPA");
    assert!(!supports_wpa2_psk_ccmp(&b));
    assert_eq!(find_vendor_ie(&wpa, [0x00, 0x50, 0xf2, 0x01]), Some(&[0x01, 0x00][..]));
    assert_eq!(find_vendor_ie(&wpa, [0x00, 0x50, 0xf2, 0x02]), None);

    let mut eap = open.clone();
    eap.extend(rsn_ie(CCMP, &[CCMP], &[EAP]));
    let b = parse_beacon(&beacon(0x0411, &eap), 1).unwrap();
    assert_eq!(security_summary(&b), "WPA2-EAP");
    assert!(!supports_wpa2_psk_ccmp(&b));

    let mut sae = open.clone();
    sae.extend(rsn_ie(CCMP, &[CCMP], &[SAE]));
    let b = parse_beacon(&beacon(0x0411, &sae), 1).unwrap();
    assert_eq!(security_summary(&b), "WPA3-SAE");
    assert!(!supports_wpa2_psk_ccmp(&b));

    let mut mixed = open.clone();
    mixed.extend(rsn_ie(CCMP, &[CCMP], &[PSK, SAE]));
    let b = parse_beacon(&beacon(0x0411, &mixed), 1).unwrap();
    assert_eq!(security_summary(&b), "WPA2/WPA3-PSK");
    assert!(supports_wpa2_psk_ccmp(&b));

    let mut tkip = open.clone();
    tkip.extend(rsn_ie(TKIP, &[TKIP], &[PSK]));
    let b = parse_beacon(&beacon(0x0411, &tkip), 1).unwrap();
    assert_eq!(security_summary(&b), "WPA2-PSK");
    assert!(!supports_wpa2_psk_ccmp(&b));
}

#[test]
fn hidden_ssid_and_display() {
    let b = parse_beacon(&beacon(0x0401, &ie(0, &[])), 1).unwrap();
    assert!(b.ssid.is_empty());
    assert_eq!(ssid_display(&b.ssid), "<hidden>");
    assert_eq!(ssid_display(b"a\x00b\xffc"), "a\\x00b\\xffc");
}

#[test]
fn channel_sources() {
    let mut ht_op = vec![36u8];
    ht_op.extend_from_slice(&[0u8; 21]);
    let mut ies5 = ie(0, b"five");
    ies5.extend(ie(61, &ht_op));
    let b = parse_beacon(&beacon(0x0401, &ies5), 1).unwrap();
    assert_eq!(b.channel, 36);
    assert!(is_5ghz_channel(b.channel));

    let b = parse_beacon(&beacon(0x0401, &ie(0, b"none")), 44).unwrap();
    assert_eq!(b.channel, 44);

    let mut vht = ie(0, b"v");
    vht.extend(ie(191, &[0; 12]));
    let b = parse_beacon(&beacon(0x0401, &vht), 1).unwrap();
    assert!(b.vht);
}

#[test]
fn rsn_defaults() {
    let r = parse_rsn(&[1, 0, 0x00, 0x0f, 0xac, 0x02]).unwrap();
    assert_eq!(r.version, 1);
    assert_eq!(r.group, TKIP);
    assert_eq!(r.pairwise, vec![CCMP]);
    assert_eq!(r.akm, vec![EAP]);
    assert_eq!(r.caps, 0);
    let r = parse_rsn(&[1, 0]).unwrap();
    assert_eq!(r.group, CCMP);
    assert!(parse_rsn(&[1]).is_none());
    // Pairwise count claims more suites than present.
    assert!(parse_rsn(&[1, 0, 0, 0x0f, 0xac, 4, 2, 0, 0, 0x0f, 0xac, 4]).is_none());
}

#[test]
fn ie_iteration() {
    let mut b = ie(0, b"ab");
    b.extend(ie(3, &[9]));
    b.extend_from_slice(&[7, 10, 1]); // truncated
    let v: Vec<_> = ies(&b).collect();
    assert_eq!(v.len(), 2);
    assert_eq!(find_ie(&b, 3), Some(&[9u8][..]));
    assert_eq!(find_ie(&b, 7), None);
}

#[test]
fn auth_roundtrip() {
    let f = build_auth_open(&BSSID, &OWN);
    assert_eq!(f.len(), 30);
    assert_eq!(frame_control(&f), Some(0x00b0));
    assert_eq!(addr1(&f), Some(BSSID));
    assert_eq!(addr2(&f), Some(OWN));
    assert_eq!(addr3(&f), Some(BSSID));
    assert_eq!(seq_ctrl(&f), Some(0));
    assert_eq!(parse_auth(&f), Some(Auth { alg: 0, seq: 1, status: 0 }));
    assert!(parse_auth(&build_deauth(&BSSID, &OWN, 3)).is_none());
}

#[test]
fn assoc_req_layout() {
    let rates = [0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24, 0x30, 0x48];
    let rsn = rsn_ie(CCMP, &[CCMP], &[PSK]);
    let extra = ie(45, &[1, 2, 3]);
    let f = build_assoc_req(&BSSID, &OWN, 0x0431, 10, b"Net", &rates, Some(&rsn), Some(&extra));
    let mut exp = vec![0x00, 0x00, 0x00, 0x00];
    exp.extend_from_slice(&BSSID);
    exp.extend_from_slice(&OWN);
    exp.extend_from_slice(&BSSID);
    exp.extend_from_slice(&[0x00, 0x00]);
    exp.extend_from_slice(&[0x31, 0x04, 0x0a, 0x00]);
    exp.extend_from_slice(&[0, 3, b'N', b'e', b't']);
    exp.extend_from_slice(&[1, 8, 0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24]);
    exp.extend_from_slice(&[50, 2, 0x30, 0x48]);
    exp.extend_from_slice(&rsn);
    exp.extend_from_slice(&extra);
    assert_eq!(f, exp);

    // Without optional IEs and with <= 8 rates there is no extended IE.
    let f = build_assoc_req(&BSSID, &OWN, 0x0001, 5, b"", &rates[..4], None, None);
    assert_eq!(&f[28..], &[0, 0, 1, 4, 0x82, 0x84, 0x8b, 0x96]);
}

#[test]
fn assoc_resp_parse() {
    let mut f = vec![0x10, 0x00, 0, 0];
    f.extend_from_slice(&OWN);
    f.extend_from_slice(&BSSID);
    f.extend_from_slice(&BSSID);
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(&[0x31, 0x04, 0x00, 0x00, 0x01, 0xc0]);
    f.extend(ie(1, &[0x82, 0x84]));
    f.extend(ie(50, &[0x30]));
    f.extend(ie(45, &[0; 26]));
    let r = parse_assoc_resp(&f).unwrap();
    assert_eq!(r.capability, 0x0431);
    assert_eq!(r.status, 0);
    assert_eq!(r.aid, 1);
    assert_eq!(r.rates, vec![0x82, 0x84, 0x30]);
    assert!(r.ht);
    f[0] = 0x30;
    assert!(parse_assoc_resp(&f).is_some());
    f[0] = 0x50;
    assert!(parse_assoc_resp(&f).is_none());
}

#[test]
fn deauth_roundtrip() {
    let f = build_deauth(&BSSID, &OWN, 3);
    assert_eq!(frame_control(&f), Some(0x00c0));
    assert_eq!(parse_deauth_reason(&f), Some(3));
    let mut d = f.clone();
    d[0] = 0xa0;
    assert_eq!(parse_deauth_reason(&d), Some(3));
    assert_eq!(parse_deauth_reason(&build_auth_open(&BSSID, &OWN)), None);
}

#[test]
fn probe_req_layout() {
    let f = build_probe_req(&OWN, b"Net", &[0x82, 0x84]);
    assert_eq!(frame_control(&f), Some(0x0040));
    assert_eq!(addr1(&f), Some([0xff; 6]));
    assert_eq!(addr2(&f), Some(OWN));
    assert_eq!(addr3(&f), Some([0xff; 6]));
    assert_eq!(&f[24..], &[0, 3, b'N', b'e', b't', 1, 2, 0x82, 0x84]);
}

#[test]
fn header_helpers() {
    assert_eq!(hdr_len(0x0080), 24);
    assert_eq!(hdr_len(0x0108), 24);
    assert_eq!(hdr_len(0x0188), 26);
    assert_eq!(hdr_len(0x0388), 32);
    assert_eq!(hdr_len(0x0308), 30);
    assert_eq!(hdr_len(0x8388), 36);
    assert!(is_mgmt(0x0080) && !is_data(0x0080));
    assert!(is_data(0x0188));
    assert_eq!(subtype(0x4188), fc::STYPE_QOS_DATA);
    assert_eq!(fc::STYPE_NULLFUNC, 0x48);
}

fn msdu_payload() -> Vec<u8> {
    let mut v = vec![0xaa, 0xaa, 0x03, 0, 0, 0, 0x08, 0x00];
    v.extend_from_slice(b"hello-ip-payload");
    v
}

fn data_hdr(fcv: u16, a1: &[u8; 6], a2: &[u8; 6], a3: &[u8; 6]) -> Vec<u8> {
    let mut v = fcv.to_le_bytes().to_vec();
    v.extend_from_slice(&[0, 0]);
    v.extend_from_slice(a1);
    v.extend_from_slice(a2);
    v.extend_from_slice(a3);
    v.extend_from_slice(&[0, 0]);
    v
}

fn expected_eth(da: &[u8; 6], sa: &[u8; 6], et: [u8; 2], payload: &[u8]) -> Vec<u8> {
    let mut v = da.to_vec();
    v.extend_from_slice(sa);
    v.extend_from_slice(&et);
    v.extend_from_slice(payload);
    v
}

#[test]
fn from_ds_qos_with_ccmp() {
    let mut f = data_hdr(0x0288 | 0x4000, &OWN, &BSSID, &PEER);
    f.extend_from_slice(&[0x05, 0x00]); // QoS control TID 5
    f.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]); // CCMP header
    f.extend(msdu_payload());
    f.extend_from_slice(&[0xee; 8]); // MIC
    let mut out = [0u8; 256];
    let n = data_to_ethernet(&f, 8, 8, &mut out).unwrap();
    assert_eq!(&out[..n], &expected_eth(&OWN, &PEER, [0x08, 0x00], b"hello-ip-payload")[..]);
    // Too-small output buffer.
    let mut small = [0u8; 20];
    assert!(data_to_ethernet(&f, 8, 8, &mut small).is_none());
}

#[test]
fn from_ds_plain_and_ibss() {
    let mut f = data_hdr(0x0208, &OWN, &BSSID, &PEER);
    f.extend(msdu_payload());
    let mut out = [0u8; 256];
    let n = data_to_ethernet(&f, 0, 0, &mut out).unwrap();
    assert_eq!(&out[..n], &expected_eth(&OWN, &PEER, [0x08, 0x00], b"hello-ip-payload")[..]);

    // IBSS / no DS bits: DA=a1, SA=a2.
    let mut g = data_hdr(0x0008, &OWN, &PEER, &BSSID);
    g.extend(msdu_payload());
    let n = data_to_ethernet(&g, 0, 0, &mut out).unwrap();
    assert_eq!(&out[..n], &expected_eth(&OWN, &PEER, [0x08, 0x00], b"hello-ip-payload")[..]);
}

#[test]
fn four_address_frame() {
    let a4 = [0x77u8; 6];
    let mut f = data_hdr(0x0308, &BSSID, &OWN, &PEER);
    f.extend_from_slice(&a4);
    f.extend(msdu_payload());
    let mut out = [0u8; 256];
    let n = data_to_ethernet(&f, 0, 0, &mut out).unwrap();
    assert_eq!(&out[..n], &expected_eth(&PEER, &a4, [0x08, 0x00], b"hello-ip-payload")[..]);
}

#[test]
fn bridge_tunnel_and_non_snap() {
    let mut f = data_hdr(0x0208, &OWN, &BSSID, &PEER);
    f.extend_from_slice(&[0xaa, 0xaa, 0x03, 0, 0, 0xf8, 0x80, 0xf3, 9, 9]);
    let mut out = [0u8; 64];
    let n = data_to_ethernet(&f, 0, 0, &mut out).unwrap();
    assert_eq!(&out[..n], &expected_eth(&OWN, &PEER, [0x80, 0xf3], &[9, 9])[..]);

    let mut g = data_hdr(0x0208, &OWN, &BSSID, &PEER);
    g.extend_from_slice(&[0x42, 0x42, 0x03, 0, 0, 0, 0x08, 0x00, 1]);
    assert!(data_to_ethernet(&g, 0, 0, &mut out).is_none());

    let h = data_hdr(0x0248, &OWN, &BSSID, &PEER);
    assert!(data_to_ethernet(&h, 0, 0, &mut out).is_none());
    let mut q = data_hdr(0x02c8, &OWN, &BSSID, &PEER);
    q.extend_from_slice(&[0, 0]);
    assert!(data_to_ethernet(&q, 0, 0, &mut out).is_none());
    // Management frame is not data.
    assert!(data_to_ethernet(&full_beacon(), 0, 0, &mut out).is_none());
}

fn eth_frame(dst: &[u8; 6], src: &[u8; 6], et: [u8; 2], payload: &[u8]) -> Vec<u8> {
    expected_eth(dst, src, et, payload)
}

#[test]
fn ethernet_roundtrip() {
    for (et, llc_oui) in [([0x08u8, 0x00], 0x00u8), ([0x88, 0x8e], 0x00), ([0x80, 0xf3], 0xf8)] {
        for qos in [None, Some(5u8)] {
            let eth = eth_frame(&PEER, &OWN, et, b"some payload bytes");
            let mut out = [0u8; 128];
            let n = ethernet_to_data(&eth, &BSSID, &OWN, qos, &mut out).unwrap();
            let f = &out[..n];
            let fcv = frame_control(f).unwrap();
            assert!(fcv & fc::TO_DS != 0 && fcv & fc::FROM_DS == 0);
            assert_eq!(addr1(f), Some(BSSID));
            assert_eq!(addr2(f), Some(OWN));
            assert_eq!(addr3(f), Some(PEER));
            let hl = if qos.is_some() { 26 } else { 24 };
            assert_eq!(hdr_len(fcv), hl);
            if let Some(t) = qos {
                assert_eq!(subtype(fcv), fc::STYPE_QOS_DATA);
                assert_eq!(f[24], t);
                assert_eq!(f[25], 0);
            } else {
                assert_eq!(subtype(fcv), fc::STYPE_DATA);
            }
            assert_eq!(&f[hl..hl + 6], &[0xaa, 0xaa, 0x03, 0, 0, llc_oui]);
            assert_eq!(&f[hl + 6..hl + 8], &et);

            // Simulated AP side: FromDS, a1 = DA, a2 = BSSID, a3 = SA.
            let mut ap = f.to_vec();
            let fc2 = (fcv & !fc::TO_DS) | fc::FROM_DS;
            ap[0..2].copy_from_slice(&fc2.to_le_bytes());
            ap[4..10].copy_from_slice(&PEER);
            ap[10..16].copy_from_slice(&BSSID);
            ap[16..22].copy_from_slice(&OWN);
            let mut back = [0u8; 128];
            let m = data_to_ethernet(&ap, 0, 0, &mut back).unwrap();
            assert_eq!(&back[..m], &eth[..]);
        }
    }
    let mut out = [0u8; 128];
    assert!(ethernet_to_data(&[0u8; 13], &BSSID, &OWN, None, &mut out).is_none());
    let eth = eth_frame(&PEER, &OWN, [0x08, 0x00], &[0u8; 100]);
    assert!(ethernet_to_data(&eth, &BSSID, &OWN, None, &mut out).is_none());
    assert!(is_eapol(&eth_frame(&PEER, &OWN, [0x88, 0x8e], &[1])));
    assert!(!is_eapol(&eth));
    assert!(!is_eapol(&[0x88, 0x8e]));
}

fn amsdu_frame() -> Vec<u8> {
    let mut f = data_hdr(0x0288, &OWN, &BSSID, &PEER);
    f.extend_from_slice(&[0x80, 0x00]); // QoS control, A-MSDU present
    let sub = |da: &[u8; 6], sa: &[u8; 6], et: [u8; 2], p: &[u8]| {
        let mut s = da.to_vec();
        s.extend_from_slice(sa);
        s.extend_from_slice(&((8 + p.len()) as u16).to_be_bytes());
        s.extend_from_slice(&[0xaa, 0xaa, 0x03, 0, 0, 0]);
        s.extend_from_slice(&et);
        s.extend_from_slice(p);
        s
    };
    let mut s1 = sub(&OWN, &PEER, [0x08, 0x00], b"first!");
    while s1.len() % 4 != 0 {
        s1.push(0);
    }
    let s2 = sub(&OWN, &BSSID, [0x86, 0xdd], b"second subframe");
    f.extend(s1);
    f.extend(s2);
    f
}

#[test]
fn amsdu_two_subframes() {
    let f = amsdu_frame();
    let mut got: Vec<Vec<u8>> = Vec::new();
    let n = amsdu_to_ethernet(&f, 0, 0, &mut |e| got.push(e.to_vec()));
    assert_eq!(n, 2);
    assert_eq!(got[0], expected_eth(&OWN, &PEER, [0x08, 0x00], b"first!"));
    assert_eq!(got[1], expected_eth(&OWN, &BSSID, [0x86, 0xdd], b"second subframe"));
    // Not valid through the single-MSDU path.
    let mut out = [0u8; 128];
    assert!(data_to_ethernet(&f, 0, 0, &mut out).is_none());
    // A normal frame yields no subframes.
    let mut g = data_hdr(0x0208, &OWN, &BSSID, &PEER);
    g.extend(msdu_payload());
    assert_eq!(amsdu_to_ethernet(&g, 0, 0, &mut |_| {}), 0);
}

#[test]
fn channel_mapping() {
    assert_eq!(channel_to_freq(1), 2412);
    assert_eq!(channel_to_freq(13), 2472);
    assert_eq!(channel_to_freq(14), 2484);
    assert_eq!(channel_to_freq(36), 5180);
    assert_eq!(channel_to_freq(149), 5745);
    assert_eq!(channel_to_freq(165), 5825);
    assert_eq!(channel_to_freq(0), 0);
    assert_eq!(channel_to_freq(20), 0);
    for ch in (1..=14).chain(32..=177) {
        assert_eq!(freq_to_channel(channel_to_freq(ch)), ch);
    }
    assert!(!is_5ghz_channel(6));
    assert!(is_5ghz_channel(149));
}

#[test]
fn truncation_never_panics() {
    let mut frames: Vec<Vec<u8>> = vec![
        full_beacon(),
        build_auth_open(&BSSID, &OWN),
        build_deauth(&BSSID, &OWN, 7),
        build_probe_req(&OWN, b"Net", &[1, 2, 3]),
        build_assoc_req(&BSSID, &OWN, 0x31, 1, b"Net", &[1; 12], Some(&rsn_ie(CCMP, &[CCMP], &[PSK])), None),
        amsdu_frame(),
    ];
    let mut ar = vec![0x10, 0x00, 0, 0];
    ar.extend_from_slice(&[0u8; 20]);
    ar.extend_from_slice(&[0x31, 0x04, 0, 0, 1, 0xc0]);
    ar.extend(ie(1, &[0x82]));
    frames.push(ar);
    let mut d = data_hdr(0x0388 | 0x8000, &OWN, &BSSID, &PEER);
    d.extend_from_slice(&[0x11; 14]);
    d.extend(msdu_payload());
    frames.push(d);
    let mut d = data_hdr(0x0288, &OWN, &BSSID, &PEER);
    d.extend_from_slice(&[0, 0]);
    d.extend(msdu_payload());
    frames.push(d);
    let mut rsn = rsn_ie(CCMP, &[CCMP, TKIP], &[PSK, SAE]);
    rsn.extend(ie(221, &[0, 0x50, 0xf2, 1]));
    frames.push(rsn);

    for f in &frames {
        for len in 0..=f.len() {
            let p = &f[..len];
            let _ = frame_control(p);
            let _ = addr1(p);
            let _ = addr2(p);
            let _ = addr3(p);
            let _ = seq_ctrl(p);
            let _ = parse_beacon(p, 1);
            let _ = parse_auth(p);
            let _ = parse_assoc_resp(p);
            let _ = parse_deauth_reason(p);
            let _ = parse_rsn(p);
            let _ = ies(p).count();
            let _ = find_ie(p, 48);
            let _ = find_vendor_ie(p, [0, 0x50, 0xf2, 1]);
            let _ = is_eapol(p);
            let mut out = [0u8; 256];
            for (ch, tr) in [(0, 0), (8, 8), (100, 0), (0, 300)] {
                let _ = data_to_ethernet(p, ch, tr, &mut out);
                let _ = amsdu_to_ethernet(p, ch, tr, &mut |_| {});
            }
            let _ = ethernet_to_data(p, &BSSID, &OWN, Some(1), &mut out);
            let _ = ethernet_to_data(p, &BSSID, &OWN, None, &mut out[..10]);
        }
    }
}
