//! Host tests: recorded HCI traces (btmon order, exact bytes both ways) and a
//! loopback virtual controller that also plays the remote device.
use crate::api::{op as aop, status, DeviceInfo, FrameBuf, Msg};
use crate::hci::{self, ev, op};
use crate::host::{Host, Owner, State};
use crate::keys::KeyStore;
use crate::{le16, Addr};
use std::collections::HashMap;

const CTRL: Addr = Addr([0x66, 0x55, 0x44, 0x33, 0x22, 0x11]);
const PAD: Addr = Addr([0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
const KEY: [u8; 16] = [0xa5; 16];

fn hx(s: &str) -> Vec<u8> {
    s.split_whitespace().map(|b| u8::from_str_radix(b, 16).unwrap()).collect()
}
fn outs(h: &mut Host) -> Vec<Vec<u8>> { std::iter::from_fn(|| h.take_out()).collect() }
fn evs(h: &mut Host) -> Vec<(Owner, Msg)> { std::iter::from_fn(|| h.take_event()).collect() }
fn feed(h: &mut Host, s: &str) { h.on_packet(&hx(s)); }
fn opcode(p: &[u8]) -> Option<u16> { if p[0] == hci::H4_CMD { Some(le16(p, 1)) } else { None } }

/// The loopback virtual controller: answers every command like a BCM4345C0
/// and, behind it, plays one remote device (`PAD`) with SSP and an L2CAP echo service.
struct Vc {
    acl_mtu: u16,
    peer_has_key: bool,
    peer_psms: Vec<u16>,
    handle: u16,
    rx: HashMap<u16, Vec<u8>>,
    /// host cid -> peer cid
    chans: HashMap<u16, u16>,
    next_peer_cid: u16,
    ident: u8,
    commands: Vec<u16>,
}

impl Vc {
    fn new() -> Vc {
        Vc { acl_mtu: 1021, peer_has_key: false, peer_psms: vec![0x11, 0x13], handle: 0x000b, rx: HashMap::new(),
             chans: HashMap::new(), next_peer_cid: 0x70, ident: 0x80, commands: vec![] }
    }
    fn cc(o: u16, ret: &[u8]) -> Vec<u8> {
        let mut p = vec![1];
        p.extend(o.to_le_bytes());
        p.extend(ret);
        hci::event(ev::CMD_COMPLETE, &p)
    }
    fn cs(o: u16) -> Vec<u8> { let b = o.to_le_bytes(); hci::event(ev::CMD_STATUS, &[0, 1, b[0], b[1]]) }
    fn with_addr(code: u8, pre: &[u8], post: &[u8]) -> Vec<u8> {
        let mut p = pre.to_vec();
        p.extend(PAD.0);
        p.extend(post);
        hci::event(code, &p)
    }
    fn l2(&mut self, cid: u16, payload: &[u8]) -> Vec<u8> {
        let mut d = (payload.len() as u16).to_le_bytes().to_vec();
        d.extend(cid.to_le_bytes());
        d.extend(payload);
        hci::acl(self.handle, 0b10, &d)
    }
    fn sig(&mut self, code: u8, id: u8, d: &[u8]) -> Vec<u8> {
        let mut p = vec![code, id];
        p.extend((d.len() as u16).to_le_bytes());
        p.extend(d);
        self.l2(1, &p)
    }

    fn handle(&mut self, pkt: &[u8]) -> Vec<Vec<u8>> {
        let h = self.handle.to_le_bytes();
        if let Some(o) = opcode(pkt) {
            self.commands.push(o);
            let p = &pkt[4..];
            return match o {
                op::READ_LOCAL_VERSION => vec![Self::cc(o, &hx("00 09 00 00 09 0f 00 19 61"))],
                op::READ_LOCAL_FEATURES => vec![Self::cc(o, &hx("00 bf fe cf fe db ff 7b 87"))],
                op::READ_BD_ADDR => { let mut r = vec![0]; r.extend(CTRL.0); vec![Self::cc(o, &r)] }
                op::READ_BUFFER_SIZE => {
                    let m = self.acl_mtu.to_le_bytes();
                    vec![Self::cc(o, &[0, m[0], m[1], 0x40, 8, 0, 0, 0])]
                }
                op::LE_READ_BUFFER_SIZE => vec![Self::cc(o, &[0, 0, 0, 0])],
                op::CREATE_CONN => vec![Self::cs(o), Self::with_addr(ev::CONN_COMPLETE, &[0, h[0], h[1]], &[1, 0])],
                op::AUTH_REQUESTED => vec![Self::cs(o), Self::with_addr(ev::LINK_KEY_REQUEST, &[], &[])],
                op::LINK_KEY_REPLY => {
                    let mut v = vec![Self::cc(o, &[0])];
                    if self.peer_has_key { v.push(hci::event(ev::AUTH_COMPLETE, &[0, h[0], h[1]])); }
                    else { v.push(hci::event(ev::AUTH_COMPLETE, &[0x06, h[0], h[1]])); }
                    v
                }
                op::LINK_KEY_NEG_REPLY => vec![Self::cc(o, &[0]), Self::with_addr(ev::IO_CAP_REQUEST, &[], &[])],
                op::IO_CAP_REPLY => {
                    assert_eq!(&p[6..9], &[0x03, 0x00, 0x04], "NoInputNoOutput, general bonding");
                    vec![Self::cc(o, &[0]), Self::with_addr(ev::IO_CAP_RESPONSE, &[], &[0x03, 0, 0x04]),
                         Self::with_addr(ev::USER_CONFIRM_REQUEST, &[], &[0x40, 0xe2, 0x01, 0x00])]
                }
                op::USER_CONFIRM_REPLY => {
                    self.peer_has_key = true;
                    let mut k = KEY.to_vec();
                    k.push(4);
                    vec![Self::cc(o, &[0]), Self::with_addr(ev::SSP_COMPLETE, &[0], &[]),
                         Self::with_addr(ev::LINK_KEY_NOTIFY, &[], &k), hci::event(ev::AUTH_COMPLETE, &[0, h[0], h[1]])]
                }
                op::SET_CONN_ENCRYPT => vec![Self::cs(o), hci::event(ev::ENCRYPT_CHANGE, &[0, h[0], h[1], 1])],
                op::DISCONNECT => vec![Self::cs(o), hci::event(ev::DISCONN_COMPLETE, &[0, h[0], h[1], 0x16])],
                op::INQUIRY | op::REMOTE_NAME_REQ | op::ACCEPT_CONN => vec![Self::cs(o)],
                _ => vec![Self::cc(o, &[0])],
            };
        }
        assert_eq!(pkt[0], hci::H4_ACL);
        let hr = le16(pkt, 1);
        let data = &pkt[5..];
        let buf = self.rx.entry(hr & 0xfff).or_default();
        if (hr >> 12) & 3 == 0b01 { buf.extend(data) } else { *buf = data.to_vec() }
        let mut resp = vec![hci::event(ev::NUM_COMPLETED, &[1, h[0], h[1], 1, 0])];
        let buf = self.rx.get(&(hr & 0xfff)).unwrap().clone();
        if buf.len() < 4 || buf.len() < le16(&buf, 0) as usize + 4 { return resp; }
        self.rx.remove(&(hr & 0xfff));
        let (cid, p) = (le16(&buf, 2), &buf[4..]);
        if cid != 1 {
            // Echo service: send the SDU back on the host's channel.
            let host_cid = *self.chans.iter().find(|(_, &pc)| pc == cid).expect("data on unknown cid").0;
            resp.push(self.l2(host_cid, p));
            return resp;
        }
        let (code, id, d) = (p[0], p[1], p[4..].to_vec());
        match code {
            0x02 => {
                let (psm, scid) = (le16(&d, 0), le16(&d, 2));
                if !self.peer_psms.contains(&psm) {
                    let mut r = vec![0, 0];
                    r.extend(scid.to_le_bytes());
                    r.extend([2, 0, 0, 0]);
                    resp.push(self.sig(0x03, id, &r));
                    return resp;
                }
                let pc = self.next_peer_cid;
                self.next_peer_cid += 1;
                self.chans.insert(scid, pc);
                let mut r = pc.to_le_bytes().to_vec();
                r.extend(scid.to_le_bytes());
                r.extend([0, 0, 0, 0]);
                resp.push(self.sig(0x03, id, &r));
                let mut c = scid.to_le_bytes().to_vec();
                c.extend([0, 0, 0x01, 0x02, 0xb9, 0x00]);
                self.ident += 1;
                let i = self.ident;
                resp.push(self.sig(0x04, i, &c));
            }
            0x04 => {
                let pc = le16(&d, 0);
                let hc = *self.chans.iter().find(|(_, &v)| v == pc).unwrap().0;
                let mut r = hc.to_le_bytes().to_vec();
                r.extend([0, 0, 0, 0]);
                resp.push(self.sig(0x05, id, &r));
            }
            0x06 => {
                let (dcid, scid) = (le16(&d, 0), le16(&d, 2));
                self.chans.remove(&scid);
                let mut r = dcid.to_le_bytes().to_vec();
                r.extend(scid.to_le_bytes());
                resp.push(self.sig(0x07, id, &r));
            }
            _ => {}
        }
        resp
    }

    /// Runs the host <-> controller loop until both sides are quiet.
    fn run(&mut self, h: &mut Host) {
        for _ in 0..1000 {
            let o = outs(h);
            if o.is_empty() { return; }
            for p in o { for r in self.handle(&p) { h.on_packet(&r); } }
        }
        panic!("host and virtual controller never settled");
    }
}

fn ready(vc: &mut Vc, keys: KeyStore) -> Host {
    let mut h = Host::new("LeandrOS", keys, 0);
    vc.run(&mut h);
    assert_eq!(h.state, State::Ready);
    while h.take_log().is_some() {}
    h
}

#[test]
fn virtual_controller_init_reaches_ready() {
    let mut vc = Vc::new();
    let h = ready(&mut vc, KeyStore::default());
    assert_eq!(vc.commands.first(), Some(&op::RESET));
    assert_eq!(vc.commands.last(), Some(&op::WRITE_SCAN_ENABLE));
    for o in [op::SET_EVENT_MASK, op::WRITE_SSP_MODE, op::WRITE_LE_HOST_SUPPORTED, op::WRITE_CLASS_OF_DEVICE, op::READ_BD_ADDR] {
        assert!(vc.commands.contains(&o), "missing 0x{:04x}", o);
    }
    assert_eq!(h.info.addr, CTRL);
    assert_eq!(h.info.lmp_subver, 0x6119);
    assert_eq!((h.info.acl_mtu, h.info.acl_pkts), (1021, 8));
    assert!(h.info.le() && h.info.ssp());
    assert_eq!(format!("{}", h.info.addr), "11:22:33:44:55:66");
}

#[test]
fn trace_inquiry_results_and_remote_name() {
    let mut vc = Vc::new();
    let mut h = ready(&mut vc, KeyStore::default());
    h.request(1, Msg::Scan { secs: 3 });
    let o = outs(&mut h);
    assert_eq!(o[0], hx("01 01 04 05 33 8b 9e 03 00"));
    // < HCI Command: Inquiry ... > HCI Event: Command Status
    feed(&mut h, "04 0f 04 00 01 01 04");
    let o = outs(&mut h);
    assert_eq!(o[0], hx("01 0b 20 07 01 10 00 10 00 00 00"));
    feed(&mut h, "04 0e 04 01 0b 20 00");
    assert_eq!(outs(&mut h)[0], hx("01 0c 20 02 01 01"));
    feed(&mut h, "04 0e 04 01 0c 20 00");
    // > HCI Event: Extended Inquiry Result: 06:05:04:03:02:01 class 0x000508 rssi -60 "Wireless Controller"
    let mut eir = hx("04 2f ff 01 01 02 03 04 05 06 01 00 08 05 00 34 12 c4 14 09");
    eir.extend(b"Wireless Controller");
    eir.resize(3 + 255, 0);
    h.on_packet(&eir);
    // > HCI Event: Inquiry Result with RSSI: FF:EE:DD:CC:BB:AA class 0x5a020c rssi -71
    feed(&mut h, "04 22 0f 01 aa bb cc dd ee ff 01 00 0c 02 5a 21 43 b9");
    // > HCI Event: Inquiry Complete
    feed(&mut h, "04 01 01 00");
    let o = outs(&mut h);
    assert_eq!(o, vec![hx("01 19 04 0a aa bb cc dd ee ff 01 00 21 c3")]);
    feed(&mut h, "04 0f 04 00 01 19 04");
    let mut rn = hx("04 07 ff 00 aa bb cc dd ee ff");
    rn.extend(b"Phone");
    rn.resize(3 + 255, 0);
    h.on_packet(&rn);
    h.tick(3_001);
    assert_eq!(outs(&mut h), vec![hx("01 0c 20 02 00 00")]);
    let e: Vec<Msg> = evs(&mut h).into_iter().map(|(o, m)| { assert_eq!(o, 1); m }).collect();
    let phone = Addr::parse("FF:EE:DD:CC:BB:AA").unwrap();
    assert_eq!(e, vec![
        Msg::Device(DeviceInfo { addr: PAD, flags: 0, rssi: -60, class: 0x000508, name: "Wireless Controller".into() }),
        Msg::Device(DeviceInfo { addr: phone, flags: 0, rssi: -71, class: 0x5a020c, name: "".into() }),
        Msg::Device(DeviceInfo { addr: phone, flags: 0, rssi: -71, class: 0x5a020c, name: "Phone".into() }),
        Msg::Result { op: aop::SCAN, status: status::OK },
    ]);
}

/// Connection complete, SSP IO-capability exchange (numeric comparison
/// auto-accept), encryption, then an L2CAP HID-control connect, data both
/// ways and a peer-initiated disconnect, as btmon would show it.
#[test]
fn trace_connect_ssp_l2cap() {
    let mut vc = Vc::new();
    let mut h = ready(&mut vc, KeyStore::default());
    h.request(1, Msg::Connect { req: 7, addr: PAD, psm: 0x11 });
    assert_eq!(outs(&mut h), vec![hx("01 05 04 0d 01 02 03 04 05 06 18 cc 01 00 00 00 01")]);
    feed(&mut h, "04 0f 04 00 01 05 04");
    feed(&mut h, "04 03 0b 00 0b 00 01 02 03 04 05 06 01 00");
    assert_eq!(outs(&mut h), vec![hx("01 11 04 02 0b 00")], "HID needs authentication first");
    feed(&mut h, "04 0f 04 00 01 11 04");
    feed(&mut h, "04 17 06 01 02 03 04 05 06");
    assert_eq!(outs(&mut h), vec![hx("01 0c 04 06 01 02 03 04 05 06")]);
    feed(&mut h, "04 0e 0a 01 0c 04 00 01 02 03 04 05 06");
    feed(&mut h, "04 31 06 01 02 03 04 05 06");
    assert_eq!(outs(&mut h), vec![hx("01 2b 04 09 01 02 03 04 05 06 03 00 04")]);
    feed(&mut h, "04 0e 0a 01 2b 04 00 01 02 03 04 05 06");
    feed(&mut h, "04 32 09 01 02 03 04 05 06 03 00 00");
    feed(&mut h, "04 33 0a 01 02 03 04 05 06 40 e2 01 00");
    assert_eq!(outs(&mut h), vec![hx("01 2c 04 06 01 02 03 04 05 06")]);
    feed(&mut h, "04 0e 0a 01 2c 04 00 01 02 03 04 05 06");
    feed(&mut h, "04 36 07 00 01 02 03 04 05 06");
    feed(&mut h, "04 18 17 01 02 03 04 05 06 a5 a5 a5 a5 a5 a5 a5 a5 a5 a5 a5 a5 a5 a5 a5 a5 04");
    let keys = h.take_keys_dirty().expect("link key must be persisted");
    assert_eq!(keys, "06:05:04:03:02:01 a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5 4 \n");
    feed(&mut h, "04 06 03 00 0b 00");
    assert_eq!(outs(&mut h), vec![hx("01 13 04 03 0b 00 01")]);
    feed(&mut h, "04 0f 04 00 01 13 04");
    feed(&mut h, "04 08 04 00 0b 00 01");
    // < ACL: L2CAP Connection Request psm 0x0011 scid 0x0040
    assert_eq!(outs(&mut h), vec![hx("02 0b 20 0c 00 08 00 01 00 02 01 04 00 11 00 40 00")]);
    // > L2CAP Connection Response dcid 0x0050 scid 0x0040 success
    feed(&mut h, "02 0b 20 10 00 0c 00 01 00 03 01 08 00 50 00 40 00 00 00 00 00");
    assert_eq!(outs(&mut h), vec![hx("02 0b 20 0c 00 08 00 01 00 04 02 04 00 50 00 00 00")]);
    // > L2CAP Configure Request dcid 0x0040 MTU 185
    feed(&mut h, "02 0b 20 10 00 0c 00 01 00 04 05 08 00 40 00 00 00 01 02 b9 00");
    assert_eq!(outs(&mut h), vec![hx("02 0b 20 0e 00 0a 00 01 00 05 05 06 00 50 00 00 00 00 00")]);
    assert!(evs(&mut h).is_empty(), "not open until our config is accepted");
    feed(&mut h, "02 0b 20 0e 00 0a 00 01 00 05 02 06 00 40 00 00 00 00 00");
    assert_eq!(evs(&mut h), vec![(1, Msg::ConnectResult { req: 7, status: 0, cid: 0x40, peer_mtu: 185 })]);
    feed(&mut h, "02 0b 20 07 00 03 00 40 00 a1 01 00");
    assert_eq!(evs(&mut h), vec![(1, Msg::Recv { cid: 0x40, data: vec![0xa1, 0x01, 0x00] })]);
    h.request(1, Msg::Send { cid: 0x40, data: vec![0xa2, 0x11] });
    assert_eq!(outs(&mut h), vec![hx("02 0b 20 06 00 02 00 50 00 a2 11")]);
    feed(&mut h, "02 0b 20 0c 00 08 00 01 00 06 06 04 00 40 00 50 00");
    assert_eq!(outs(&mut h), vec![hx("02 0b 20 0c 00 08 00 01 00 07 06 04 00 40 00 50 00")]);
    assert_eq!(evs(&mut h), vec![(1, Msg::Closed { cid: 0x40, reason: status::LINK_LOST })]);
}

#[test]
fn loopback_pair_connect_echo_fragmented_then_reconnect_with_stored_key() {
    let mut vc = Vc::new();
    vc.acl_mtu = 27;
    let mut h = ready(&mut vc, KeyStore::default());
    h.request(3, Msg::Pair { addr: PAD });
    vc.run(&mut h);
    assert_eq!(evs(&mut h), vec![(3, Msg::Result { op: aop::PAIR, status: 0 })]);
    let file = h.take_keys_dirty().unwrap();
    h.request(3, Msg::Connect { req: 1, addr: PAD, psm: 0x13 });
    vc.run(&mut h);
    let e = evs(&mut h);
    let Msg::ConnectResult { status: 0, cid, peer_mtu: 185, .. } = e[0].1 else { panic!("{:?}", e) };
    let sdu: Vec<u8> = (0..150u8).collect();
    h.request(3, Msg::Send { cid, data: sdu.clone() });
    vc.run(&mut h);
    assert_eq!(evs(&mut h), vec![(3, Msg::Recv { cid, data: sdu })], "6 ACL fragments each way");
    h.request(3, Msg::Close { cid });
    vc.run(&mut h);
    assert_eq!(evs(&mut h), vec![(3, Msg::Closed { cid, reason: status::LOCAL_CLOSE })]);
    assert_eq!(h.channel_count(), 0);
    h.tick(10_000);
    vc.run(&mut h);
    assert!(std::iter::from_fn(|| h.take_log()).any(|l| l.contains("ms, disconnecting")));
    assert!(!h.busy());
    // btd restart: keys come back from the file; the reconnect uses them (no SSP).
    let mut vc2 = Vc::new();
    vc2.peer_has_key = true;
    let mut h2 = ready(&mut vc2, KeyStore::parse(&file));
    h2.request(4, Msg::Connect { req: 2, addr: PAD, psm: 0x11 });
    vc2.run(&mut h2);
    assert!(matches!(evs(&mut h2)[0].1, Msg::ConnectResult { status: 0, .. }));
    assert!(vc2.commands.contains(&op::LINK_KEY_REPLY) && !vc2.commands.contains(&op::IO_CAP_REPLY));
}

#[test]
fn stale_key_falls_back_to_fresh_pairing() {
    let mut vc = Vc::new();
    let ks = KeyStore::parse("06:05:04:03:02:01 00112233445566778899aabbccddeeff 4 Pad\n");
    let mut h = ready(&mut vc, ks);
    h.request(1, Msg::Pair { addr: PAD });
    vc.run(&mut h);
    assert_eq!(evs(&mut h), vec![(1, Msg::Result { op: aop::PAIR, status: 0 })]);
    assert!(vc.commands.contains(&op::IO_CAP_REPLY));
    assert_eq!(h.keys.get(PAD).unwrap().key, KEY);
}

#[test]
fn incoming_channel_on_listened_psm_and_refused_psm() {
    let mut vc = Vc::new();
    let mut h = ready(&mut vc, KeyStore::default());
    h.request(2, Msg::Listen { psm: 0x11 });
    h.request(5, Msg::Listen { psm: 0x11 });
    assert_eq!(evs(&mut h), vec![(2, Msg::Result { op: aop::LISTEN, status: 0 }), (5, Msg::Result { op: aop::LISTEN, status: status::BUSY })]);
    feed(&mut h, "04 04 0a 01 02 03 04 05 06 08 05 00 01");
    assert_eq!(outs(&mut h), vec![hx("01 09 04 07 01 02 03 04 05 06 01")]);
    feed(&mut h, "04 0f 04 00 01 09 04");
    feed(&mut h, "04 03 0b 00 0c 00 01 02 03 04 05 06 01 01");
    assert!(outs(&mut h).is_empty(), "already encrypted, nothing to do");
    feed(&mut h, "02 0c 20 0c 00 08 00 01 00 02 09 04 00 03 00 41 00");
    assert_eq!(outs(&mut h), vec![hx("02 0c 20 10 00 0c 00 01 00 03 09 08 00 00 00 41 00 02 00 00 00")], "RFCOMM not listened");
    feed(&mut h, "02 0c 20 0c 00 08 00 01 00 02 0a 04 00 11 00 42 00");
    assert_eq!(outs(&mut h), vec![
        hx("02 0c 20 10 00 0c 00 01 00 03 0a 08 00 40 00 42 00 00 00 00 00"),
        hx("02 0c 20 0c 00 08 00 01 00 04 01 04 00 42 00 00 00"),
    ]);
    feed(&mut h, "02 0c 20 0e 00 0a 00 01 00 05 01 06 00 40 00 00 00 00 00");
    feed(&mut h, "02 0c 20 0c 00 08 00 01 00 04 0b 04 00 40 00 00 00");
    assert_eq!(outs(&mut h), vec![hx("02 0c 20 0e 00 0a 00 01 00 05 0b 06 00 42 00 00 00 00 00")]);
    assert_eq!(evs(&mut h), vec![
        (2, Msg::Link { addr: PAD, up: true, incoming: true }),
        (2, Msg::Incoming { cid: 0x40, addr: PAD, psm: 0x11, peer_mtu: 672 }),
    ]);
    // The listener goes away: its channel is closed towards the peer.
    h.drop_owner(2);
    assert_eq!(outs(&mut h), vec![hx("02 0c 20 0c 00 08 00 01 00 06 02 04 00 42 00 40 00")]);
}

#[test]
fn legacy_pin_and_passkey_and_link_loss() {
    let mut vc = Vc::new();
    let mut h = ready(&mut vc, KeyStore::default());
    feed(&mut h, "04 16 06 01 02 03 04 05 06");
    assert_eq!(outs(&mut h), vec![hx("01 0d 04 17 01 02 03 04 05 06 04 30 30 30 30 00 00 00 00 00 00 00 00 00 00 00 00")]);
    feed(&mut h, "04 0e 0a 01 0d 04 00 01 02 03 04 05 06");
    feed(&mut h, "04 34 06 01 02 03 04 05 06");
    assert_eq!(outs(&mut h), vec![hx("01 2f 04 06 01 02 03 04 05 06")]);
    feed(&mut h, "04 0e 0a 01 2f 04 00 01 02 03 04 05 06");
    // A connect whose page times out reports status 2.
    h.request(9, Msg::Connect { req: 1, addr: PAD, psm: 0x01 });
    outs(&mut h);
    feed(&mut h, "04 0f 04 00 01 05 04");
    feed(&mut h, "04 03 0b 04 00 00 01 02 03 04 05 06 01 00");
    assert_eq!(evs(&mut h), vec![(9, Msg::ConnectResult { req: 1, status: status::PAGE_TIMEOUT, cid: 0, peer_mtu: 0 })]);
}

#[test]
fn le_adv_report_and_att_smp_stubs() {
    let mut vc = Vc::new();
    let mut h = ready(&mut vc, KeyStore::default());
    h.request(1, Msg::Scan { secs: 2 });
    vc.run(&mut h);
    // > LE Meta: Advertising Report, random addr C0:11:22:33:44:55 "Xbox Wireless Controller" rssi -50
    let mut r = hx("04 3e 00 02 01 00 01 55 44 33 22 11 c0 1d 02 01 06 19 09");
    r.extend(b"Xbox Wireless Controller");
    r.push(0xce);
    r[2] = (r.len() - 3) as u8;
    h.on_packet(&r);
    let e = evs(&mut h);
    assert_eq!(e, vec![(1, Msg::Device(DeviceInfo { addr: Addr::parse("C0:11:22:33:44:55").unwrap(), flags: 4 | 8, rssi: -50, class: 0, name: "Xbox Wireless Controller".into() }))]);
    // An LE central connects to us: ATT MTU / read-by-group and SMP get stubbed answers.
    feed(&mut h, "04 3e 13 01 00 40 00 01 01 55 44 33 22 11 c0 18 00 00 00 48 00 00");
    feed(&mut h, "02 40 00 07 00 03 00 04 00 02 00 02");
    feed(&mut h, "02 40 00 0b 00 07 00 04 00 10 01 00 ff ff 00 28");
    feed(&mut h, "02 40 00 0b 00 07 00 06 00 01 03 00 01 10 07 07");
    assert_eq!(outs(&mut h), vec![
        hx("02 40 00 07 00 03 00 04 00 03 17 00"),
        hx("02 40 00 09 00 05 00 04 00 01 10 00 00 06"),
        hx("02 40 00 06 00 02 00 06 00 05 05"),
    ]);
}

#[test]
fn api_frames_roundtrip_through_a_split_stream() {
    let msgs = vec![
        Msg::Connect { req: 1, addr: PAD, psm: 0x11 }, Msg::Listen { psm: 0x13 }, Msg::Send { cid: 0x40, data: vec![1, 2, 3] },
        Msg::Close { cid: 0x41 }, Msg::Scan { secs: 10 }, Msg::Devices, Msg::Pair { addr: PAD }, Msg::Remove { addr: PAD },
        Msg::Info, Msg::Disconnect { addr: PAD }, Msg::ConnectResult { req: 1, status: 0, cid: 0x40, peer_mtu: 185 },
        Msg::Recv { cid: 0x40, data: vec![9; 300] }, Msg::Closed { cid: 0x40, reason: 10 },
        Msg::Incoming { cid: 0x42, addr: PAD, psm: 0x11, peer_mtu: 672 },
        Msg::Device(DeviceInfo { addr: PAD, flags: 3, rssi: -40, class: 0x2508, name: "Pad".into() }),
        Msg::Result { op: aop::PAIR, status: 3 },
        Msg::InfoRsp { addr: CTRL, hci_ver: 9, lmp_subver: 0x6119, manufacturer: 15, state: 1, name: "LeandrOS".into() },
    ];
    let mut wire = Vec::new();
    for m in &msgs { m.encode(&mut wire); }
    let mut fb = FrameBuf::default();
    let mut got = Vec::new();
    for b in wire.chunks(3) {
        fb.push(b);
        while let Some(m) = fb.next() { got.push(m.unwrap()); }
    }
    assert_eq!(got, msgs);
}

#[test]
fn keys_file_roundtrip_and_h4_reframing() {
    let text = "# bonded devices\n06:05:04:03:02:01 a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5 4 Wireless Controller\nbogus line\n";
    let ks = KeyStore::parse(text);
    assert_eq!(ks.keys.len(), 1);
    assert_eq!(ks.keys[0].name, "Wireless Controller");
    assert_eq!(KeyStore::parse(&ks.serialize()), ks);
    assert_eq!(hci::h4_len(&hx("04 0e 04")), Some(Ok(7)));
    assert_eq!(hci::h4_len(&hx("02 0b 20 0c 00")), Some(Ok(17)));
    assert_eq!(hci::h4_len(&hx("02 0b")), None);
    assert_eq!(hci::h4_len(&hx("07")), Some(Err(())));
}

/// Regression: btd creates the host long after boot; the first tick must not
/// see the initial HCI_Reset as already timed out (uptime > 4 s).
#[test]
fn init_with_late_clock_does_not_time_out_reset() {
    let mut h = Host::new("LeandrOS", KeyStore::default(), 600_000);
    h.tick(600_050);
    assert!(std::iter::from_fn(|| h.take_log()).all(|l| !l.contains("timed out")));
    let mut vc = Vc::new();
    vc.run(&mut h);
    assert_eq!(h.state, State::Ready);
}

const CONN_REQ: &str = "04 04 0a 01 02 03 04 05 06 08 05 00 01";
const CONN_OK_PLAIN: &str = "04 03 0b 00 0b 00 01 02 03 04 05 06 01 00";

fn ops(h: &mut Host) -> Vec<u16> { outs(h).iter().filter_map(|p| opcode(p)).collect() }
fn cs_status(o: u16, st: u8) -> Vec<u8> { let b = o.to_le_bytes(); hci::event(ev::CMD_STATUS, &[st, 1, b[0], b[1]]) }
fn has_connect_ok(e: &[(Owner, Msg)]) -> bool {
    e.iter().any(|(_, m)| matches!(m, Msg::ConnectResult { status: 0, .. }))
}

#[test]
fn crossing_conn_request_while_paging_is_adopted() {
    let mut vc = Vc::new();
    let mut h = ready(&mut vc, KeyStore::default());
    h.request(3, Msg::Connect { req: 1, addr: PAD, psm: 0x13 });
    assert_eq!(ops(&mut h), vec![op::CREATE_CONN]);
    feed(&mut h, CONN_REQ);
    h.on_packet(&cs_status(op::CREATE_CONN, 0x0b));
    let o = ops(&mut h);
    assert!(o.contains(&op::ACCEPT_CONN) && !o.contains(&op::REJECT_CONN), "{:?}", o);
    h.on_packet(&Vc::cs(op::ACCEPT_CONN));
    assert!(evs(&mut h).is_empty(), "the 0x0b must not fail the connect");
    feed(&mut h, CONN_OK_PLAIN);
    vc.run(&mut h);
    let e = evs(&mut h);
    assert!(has_connect_ok(&e), "{:?}", e);
}

#[test]
fn create_conn_0x0b_then_incoming() {
    let mut vc = Vc::new();
    let mut h = ready(&mut vc, KeyStore::default());
    h.request(3, Msg::Connect { req: 1, addr: PAD, psm: 0x13 });
    assert_eq!(ops(&mut h), vec![op::CREATE_CONN]);
    h.on_packet(&cs_status(op::CREATE_CONN, 0x0b));
    assert!(evs(&mut h).is_empty());
    feed(&mut h, CONN_REQ);
    assert!(ops(&mut h).contains(&op::ACCEPT_CONN));
    h.on_packet(&Vc::cs(op::ACCEPT_CONN));
    feed(&mut h, CONN_OK_PLAIN);
    vc.run(&mut h);
    assert!(has_connect_ok(&evs(&mut h)));
    // Variant: no incoming link ever shows up.
    let mut vc = Vc::new();
    let mut h = ready(&mut vc, KeyStore::default());
    h.request(3, Msg::Connect { req: 1, addr: PAD, psm: 0x13 });
    outs(&mut h);
    h.on_packet(&cs_status(op::CREATE_CONN, 0x0b));
    h.tick(5_000);
    assert!(evs(&mut h).is_empty());
    h.tick(11_000);
    assert_eq!(evs(&mut h), vec![(3, Msg::ConnectResult { req: 1, status: status::BUSY, cid: 0, peer_mtu: 0 })]);
}

#[test]
fn incoming_bonded_link_gets_grace_and_security() {
    let mut vc = Vc::new();
    vc.peer_has_key = true;
    let ks = KeyStore::parse("06:05:04:03:02:01 00112233445566778899aabbccddeeff 4 Pad\n");
    let mut h = ready(&mut vc, ks);
    h.request(2, Msg::Listen { psm: 0x11 });
    evs(&mut h);
    feed(&mut h, CONN_REQ);
    h.on_packet(&Vc::cs(op::ACCEPT_CONN));
    outs(&mut h);
    feed(&mut h, CONN_OK_PLAIN);
    vc.run(&mut h);
    assert!(vc.commands.contains(&op::AUTH_REQUESTED), "we secure the incoming bonded link");
    assert!(vc.commands.contains(&op::SET_CONN_ENCRYPT));
    assert_eq!(evs(&mut h), vec![(2, Msg::Link { addr: PAD, up: true, incoming: true })]);
    h.tick(6_000);
    assert!(!ops(&mut h).contains(&op::DISCONNECT), "inside the grace");
    h.tick(21_000);
    assert!(ops(&mut h).contains(&op::DISCONNECT), "grace over");
}

#[test]
fn incoming_hid_psm_pending_until_encrypted() {
    let pending = hx("02 0b 20 10 00 0c 00 01 00 03 0a 08 00 40 00 42 00 01 00 01 00");
    let accepted = hx("02 0b 20 10 00 0c 00 01 00 03 0a 08 00 40 00 42 00 00 00 00 00");
    let blocked = hx("02 0b 20 10 00 0c 00 01 00 03 0a 08 00 40 00 42 00 03 00 00 00");
    let req = "02 0b 20 0c 00 08 00 01 00 02 0a 04 00 11 00 42 00";
    let setup = || {
        let mut vc = Vc::new();
        let mut h = ready(&mut vc, KeyStore::default());
        h.request(2, Msg::Listen { psm: 0x11 });
        feed(&mut h, CONN_REQ);
        h.on_packet(&Vc::cs(op::ACCEPT_CONN));
        feed(&mut h, CONN_OK_PLAIN);
        outs(&mut h);
        evs(&mut h);
        h
    };
    let mut h = setup();
    feed(&mut h, req);
    let o = outs(&mut h);
    assert!(o.contains(&pending), "{:?}", o);
    assert!(o.iter().any(|p| opcode(p) == Some(op::AUTH_REQUESTED)));
    assert!(evs(&mut h).is_empty());
    feed(&mut h, "04 08 04 00 0b 00 01");
    let o = outs(&mut h);
    assert!(o.contains(&accepted), "{:?}", o);
    assert_eq!(o.iter().filter(|p| p[0] == hci::H4_ACL).count(), 2, "response plus our Config Request");
    // Authentication fails: the channel is refused with a security block.
    let mut h = setup();
    feed(&mut h, req);
    outs(&mut h);
    feed(&mut h, "04 06 03 05 0b 00");
    let o = outs(&mut h);
    assert!(o.contains(&blocked), "{:?}", o);
    assert_eq!(h.channel_count(), 0);
}

#[test]
fn unsolicited_pairing_on_incoming_link_is_refused() {
    let mut vc = Vc::new();
    let mut h = ready(&mut vc, KeyStore::default());
    feed(&mut h, CONN_REQ);
    h.on_packet(&Vc::cs(op::ACCEPT_CONN));
    feed(&mut h, CONN_OK_PLAIN);
    outs(&mut h);
    feed(&mut h, "04 31 06 01 02 03 04 05 06");
    let o = ops(&mut h);
    assert!(o.contains(&op::IO_CAP_NEG_REPLY) && !o.contains(&op::IO_CAP_REPLY), "{:?}", o);
}

const CONN_OK_H0C: &str = "04 03 0b 00 0c 00 01 02 03 04 05 06 01 00";

#[test]
fn new_conn_complete_drops_stale_up_link_for_same_addr() {
    let mut vc = Vc::new();
    let mut h = ready(&mut vc, KeyStore::default());
    h.request(2, Msg::Listen { psm: 0x11 });
    evs(&mut h);
    feed(&mut h, CONN_REQ);
    h.on_packet(&Vc::cs(op::ACCEPT_CONN));
    feed(&mut h, CONN_OK_PLAIN);
    // A second Connecting entry for the same address while the old link is still Up.
    h.test_new_connecting(PAD);
    feed(&mut h, CONN_OK_H0C);
    let mut logs = vec![];
    while let Some(l) = h.take_log() { logs.push(l); }
    assert!(logs.iter().any(|l| l.contains("stale link 0x00b dropped for new 0x00c")), "{:?}", logs);
    let e = evs(&mut h);
    assert_eq!(e.iter().filter(|x| matches!(x.1, Msg::Link { up: false, .. })).count(), 1, "{:?}", e);
    assert_eq!(e.iter().filter(|x| matches!(x.1, Msg::Link { up: true, .. })).count(), 2, "{:?}", e);
}

#[test]
fn acl_credit_leak_is_restored_on_channel_timeout() {
    let mut vc = Vc::new();
    let mut h = ready(&mut vc, KeyStore::default());
    h.request(2, Msg::Listen { psm: 0x11 });
    evs(&mut h);
    feed(&mut h, CONN_REQ);
    h.on_packet(&Vc::cs(op::ACCEPT_CONN));
    feed(&mut h, CONN_OK_PLAIN);
    // A leaked pool: nothing outstanding, yet no credit left. Our request sits queued.
    h.test_set_acl_avail(0);
    h.request(2, Msg::Connect { req: 1, addr: PAD, psm: 0x01 });
    assert!(outs(&mut h).iter().all(|p| p[0] != hci::H4_ACL), "blocked");
    h.tick(60_000);
    let mut logs = vec![];
    while let Some(l) = h.take_log() { logs.push(l); }
    assert!(logs.iter().any(|l| l.contains("ACL credit leak, restoring")), "{:?}", logs);
    assert!(outs(&mut h).iter().any(|p| p[0] == hci::H4_ACL), "queued frame flows after recovery");
}
