//! The sans-IO Bluetooth host: controller init, discovery, ACL links, L2CAP
//! signalling + basic-mode channels, pairing (SSP just-works / numeric
//! comparison auto-accept, legacy PIN 0000), LE ATT/SMP stubs.
//!
//! Drive it with [`Host::on_packet`] (H4 packets read from /dev/hci0),
//! [`Host::request`] (client messages) and [`Host::tick`] (monotonic ms);
//! drain [`Host::take_out`] (H4 packets to write), [`Host::take_event`]
//! (client messages, tagged with the owning client), [`Host::take_log`] and
//! [`Host::take_keys_dirty`] (link-key file contents to persist).
use crate::api::{dev_flags, op as aop, status, DeviceInfo, Msg};
use crate::hci::{self, ev, op};
use crate::keys::{Key, KeyStore};
use crate::{le16, Addr};
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// A client connection of btd (any non-zero id; tests use small integers).
pub type Owner = u32;

pub const OUR_MTU: u16 = 672;
pub const PSM_SDP: u16 = 0x0001;
const CID_SIG: u16 = 0x0001;
const CID_ATT: u16 = 0x0004;
const CID_LE_SIG: u16 = 0x0005;
const CID_SMP: u16 = 0x0006;
const FIRST_DYN_CID: u16 = 0x0040;
pub const IDLE_DISCONNECT_MS: u64 = 5000;
const CMD_TIMEOUT_MS: u64 = 4000;
const NOBODY: Owner = 0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State { Init, Ready, Dead }

#[derive(Default, Clone, Debug)]
pub struct Info {
    pub addr: Addr,
    pub hci_ver: u8,
    pub lmp_subver: u16,
    pub manufacturer: u16,
    pub features: [u8; 8],
    pub acl_mtu: u16,
    pub acl_pkts: u16,
    pub le_mtu: u16,
    pub le_pkts: u16,
}

impl Info {
    pub fn le(&self) -> bool { self.features[4] & 0x40 != 0 }
    pub fn ssp(&self) -> bool { self.features[6] & 0x08 != 0 }
}

#[derive(Clone, Debug, Default)]
pub struct Device {
    pub addr: Addr,
    pub le: bool,
    pub random: bool,
    pub name: String,
    pub class: u32,
    pub rssi: i8,
    pub psrm: u8,
    pub clock: Option<u16>,
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum LinkState { Connecting, Up }

struct Link {
    addr: Addr,
    handle: u16,
    le: bool,
    state: LinkState,
    since: u64,
    authenticated: bool,
    encrypted: bool,
    auth_pending: bool,
    enc_pending: bool,
    auth_retried: bool,
    rx: Vec<u8>,
    last_activity: u64,
    outstanding: u16,
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum ChanState { WaitLink, WaitConnRsp, Config { ours_ok: bool, theirs_ok: bool }, Open, Closing }

struct Chan {
    cid: u16,
    rcid: u16,
    psm: u16,
    addr: Addr,
    owner: Owner,
    req: Option<u16>,
    state: ChanState,
    peer_mtu: u16,
    deadline: u64,
    ident: u8,
}

struct Scan {
    owner: Owner,
    until: u64,
    inquiry: bool,
    le: bool,
    names: Vec<Addr>,
    name_pending: bool,
}

const INIT: &[u16] = &[
    op::RESET, op::READ_LOCAL_VERSION, op::READ_LOCAL_FEATURES, op::READ_BD_ADDR, op::READ_BUFFER_SIZE,
    op::LE_READ_BUFFER_SIZE, op::SET_EVENT_MASK, op::WRITE_SSP_MODE, op::WRITE_INQUIRY_MODE,
    op::WRITE_LE_HOST_SUPPORTED, op::LE_SET_EVENT_MASK, op::WRITE_CLASS_OF_DEVICE, op::WRITE_LOCAL_NAME,
    op::WRITE_PAGE_TIMEOUT, op::WRITE_SCAN_ENABLE,
];

pub struct Host {
    pub state: State,
    pub info: Info,
    pub name: String,
    pub devices: Vec<Device>,
    pub keys: KeyStore,
    now: u64,
    init_step: usize,
    cmdq: VecDeque<Vec<u8>>,
    inflight: Option<(u16, u64, Vec<u8>)>,
    credits: u8,
    out: VecDeque<Vec<u8>>,
    events: VecDeque<(Owner, Msg)>,
    logs: VecDeque<String>,
    links: Vec<Link>,
    chans: Vec<Chan>,
    listens: Vec<(u16, Owner)>,
    keys_dirty: bool,
    scan: Option<Scan>,
    pairs: Vec<(Owner, Addr)>,
    next_cid: u16,
    next_ident: u8,
    acl_txq: VecDeque<(u16, bool, Vec<u8>)>,
    acl_avail: u16,
    le_avail: u16,
}

impl Host {
    pub fn new(name: &str, keys: KeyStore) -> Host {
        let mut h = Host {
            state: State::Init, info: Info::default(), name: String::from(name), devices: Vec::new(), keys,
            now: 0, init_step: 0, cmdq: VecDeque::new(), inflight: None, credits: 1, out: VecDeque::new(),
            events: VecDeque::new(), logs: VecDeque::new(), links: Vec::new(), chans: Vec::new(),
            listens: Vec::new(), keys_dirty: false, scan: None, pairs: Vec::new(), next_cid: FIRST_DYN_CID,
            next_ident: 1, acl_txq: VecDeque::new(), acl_avail: 0, le_avail: 0,
        };
        for k in h.keys.keys.clone() {
            h.devices.push(Device { addr: k.addr, name: k.name, psrm: 1, ..Default::default() });
        }
        h.init_next();
        h
    }

    // ── outputs ────────────────────────────────────────────────────────────
    pub fn take_out(&mut self) -> Option<Vec<u8>> { self.out.pop_front() }
    pub fn take_event(&mut self) -> Option<(Owner, Msg)> { self.events.pop_front() }
    pub fn take_log(&mut self) -> Option<String> { self.logs.pop_front() }
    /// The link-key file contents when they changed since the last call.
    pub fn take_keys_dirty(&mut self) -> Option<String> {
        if !self.keys_dirty { return None; }
        self.keys_dirty = false;
        Some(self.keys.serialize())
    }
    /// True while timers matter (btd polls with a short timeout only then).
    pub fn busy(&self) -> bool {
        self.inflight.is_some() || self.scan.is_some()
            || self.chans.iter().any(|c| c.state != ChanState::Open)
            || self.links.iter().any(|l| l.state == LinkState::Connecting
                || (!l.le && !self.chans.iter().any(|c| c.addr == l.addr)))
    }
    pub fn channel_count(&self) -> usize { self.chans.len() }

    fn log(&mut self, s: String) { self.logs.push_back(s); }
    fn emit(&mut self, owner: Owner, m: Msg) { if owner != NOBODY { self.events.push_back((owner, m)); } }
    fn result(&mut self, owner: Owner, o: u8, st: u8) { self.emit(owner, Msg::Result { op: o, status: st }); }

    // ── HCI commands ───────────────────────────────────────────────────────
    fn send_cmd(&mut self, o: u16, params: &[u8]) {
        self.cmdq.push_back(hci::cmd(o, params));
        self.pump_cmds();
    }

    fn pump_cmds(&mut self) {
        while self.credits > 0 && self.inflight.is_none() {
            let Some(pkt) = self.cmdq.pop_front() else { break };
            self.inflight = Some((le16(&pkt, 1), self.now, pkt[4..].to_vec()));
            self.credits = 0;
            self.out.push_back(pkt);
        }
    }

    fn init_next(&mut self) {
        loop {
            let Some(&o) = INIT.get(self.init_step) else {
                self.state = State::Ready;
                let i = &self.info;
                let s = format!("ready bdaddr={} hci_ver={} lmp_subver=0x{:04x} manufacturer={} acl={}x{} le={}",
                    i.addr, i.hci_ver, i.lmp_subver, i.manufacturer, i.acl_mtu, i.acl_pkts, i.le());
                self.log(s);
                return;
            };
            let le = self.info.le();
            let skip = match o {
                op::LE_READ_BUFFER_SIZE | op::WRITE_LE_HOST_SUPPORTED | op::LE_SET_EVENT_MASK => !le,
                op::WRITE_SSP_MODE => !self.info.ssp(),
                _ => false,
            };
            if skip { self.init_step += 1; continue; }
            let p: Vec<u8> = match o {
                op::SET_EVENT_MASK => hci::EVENT_MASK.to_le_bytes().to_vec(),
                op::WRITE_SSP_MODE => vec![1],
                op::WRITE_INQUIRY_MODE => vec![2],
                op::WRITE_LE_HOST_SUPPORTED => vec![1, 0],
                op::LE_SET_EVENT_MASK => 0x1fu64.to_le_bytes().to_vec(),
                op::WRITE_CLASS_OF_DEVICE => hci::CLASS_OF_DEVICE.to_vec(),
                op::WRITE_LOCAL_NAME => {
                    let mut n = vec![0u8; 248];
                    let b = self.name.as_bytes();
                    let l = b.len().min(247);
                    n[..l].copy_from_slice(&b[..l]);
                    n
                }
                op::WRITE_PAGE_TIMEOUT => 0x2000u16.to_le_bytes().to_vec(),
                op::WRITE_SCAN_ENABLE => vec![3],
                _ => Vec::new(),
            };
            self.send_cmd(o, &p);
            return;
        }
    }

    fn init_result(&mut self, o: u16, st: u8, r: &[u8]) {
        if INIT.get(self.init_step) != Some(&o) { return; }
        if st != 0 {
            self.log(format!("init: command 0x{:04x} failed status 0x{:02x}", o, st));
            if matches!(o, op::RESET | op::READ_BD_ADDR | op::READ_BUFFER_SIZE) {
                self.state = State::Dead;
                return;
            }
        } else {
            match o {
                op::READ_LOCAL_VERSION if r.len() >= 8 => {
                    self.info.hci_ver = r[0];
                    self.info.manufacturer = le16(r, 4);
                    self.info.lmp_subver = le16(r, 6);
                }
                op::READ_LOCAL_FEATURES if r.len() >= 8 => self.info.features.copy_from_slice(&r[..8]),
                op::READ_BD_ADDR if r.len() >= 6 => self.info.addr = Addr::from_slice(r),
                op::READ_BUFFER_SIZE if r.len() >= 5 => {
                    self.info.acl_mtu = le16(r, 0);
                    self.info.acl_pkts = le16(r, 3);
                    self.acl_avail = self.info.acl_pkts;
                }
                op::LE_READ_BUFFER_SIZE if r.len() >= 3 => {
                    self.info.le_mtu = le16(r, 0);
                    self.info.le_pkts = r[2] as u16;
                    self.le_avail = self.info.le_pkts;
                }
                _ => {}
            }
        }
        self.init_step += 1;
        self.init_next();
    }

    fn cmd_done(&mut self, o: u16, st: u8, r: &[u8]) {
        let mut params = Vec::new();
        if let Some((io, _, p)) = &self.inflight {
            if *io == o { params = p.clone(); self.inflight = None; }
        }
        if self.state == State::Init {
            self.init_result(o, st, r);
            return;
        }
        match o {
            op::CREATE_CONN if st != 0 && params.len() >= 6 => {
                let a = Addr::from_slice(&params);
                if let Some(i) = self.links.iter().position(|l| l.addr == a && l.state == LinkState::Connecting) {
                    self.log(format!("connect {} failed status 0x{:02x}", a, st));
                    self.link_down(i, hci::status_of(st));
                }
            }
            op::AUTH_REQUESTED | op::SET_CONN_ENCRYPT if st != 0 && params.len() >= 2 => {
                self.security_failed(le16(&params, 0), st);
            }
            op::INQUIRY if st != 0 => {
                if let Some(s) = self.scan.as_mut() { s.inquiry = false; }
            }
            op::INQUIRY_CANCEL => self.inquiry_ended(),
            op::REMOTE_NAME_REQ if st != 0 => {
                if let Some(s) = self.scan.as_mut() { s.name_pending = false; }
                self.scan_tick();
            }
            _ => {}
        }
    }

    // ── HCI input ──────────────────────────────────────────────────────────
    /// One whole H4 packet from the controller.
    pub fn on_packet(&mut self, pkt: &[u8]) {
        match pkt.first() {
            Some(&hci::H4_EVT) if pkt.len() >= 3 && pkt.len() >= 3 + pkt[2] as usize => {
                let p = pkt[3..3 + pkt[2] as usize].to_vec();
                self.on_event(pkt[1], &p);
            }
            Some(&hci::H4_ACL) if pkt.len() >= 5 => self.on_acl(pkt),
            _ => {}
        }
        self.pump_cmds();
        self.pump_acl();
    }

    fn on_event(&mut self, code: u8, p: &[u8]) {
        macro_rules! need { ($n:expr) => { if p.len() < $n { return; } } }
        match code {
            ev::CMD_COMPLETE => {
                need!(3);
                self.credits = p[0];
                let st = p.get(3).copied().unwrap_or(0);
                let r = p.get(4..).unwrap_or(&[]).to_vec();
                self.cmd_done(le16(p, 1), st, &r);
            }
            ev::CMD_STATUS => {
                need!(4);
                self.credits = p[1];
                self.cmd_done(le16(p, 2), p[0], &[]);
            }
            ev::HW_ERROR => {
                self.log(format!("controller hardware error 0x{:02x}", p.first().copied().unwrap_or(0)));
                self.state = State::Dead;
            }
            ev::INQUIRY_RESULT | ev::INQUIRY_RESULT_RSSI => {
                need!(1);
                for i in 0..p[0] as usize {
                    let b = 1 + 14 * i;
                    if p.len() < b + 14 { break; }
                    let e = &p[b..b + 14];
                    let rssi = if code == ev::INQUIRY_RESULT_RSSI { e[13] as i8 } else { -127 };
                    let class = e[8] as u32 | (e[9] as u32) << 8 | (e[10] as u32) << 16;
                    self.found(Addr::from_slice(e), false, false, e[6], class, Some(le16(e, 11)), rssi, None);
                }
            }
            ev::EXT_INQUIRY_RESULT => {
                need!(15);
                let e = &p[1..];
                let class = e[8] as u32 | (e[9] as u32) << 8 | (e[10] as u32) << 16;
                let name = hci::eir_name(&p[15..]).map(|n| n.to_vec());
                self.found(Addr::from_slice(e), false, false, e[6], class, Some(le16(e, 11)), e[13] as i8, name.as_deref());
            }
            ev::INQUIRY_COMPLETE => self.inquiry_ended(),
            ev::REMOTE_NAME => {
                need!(7);
                let a = Addr::from_slice(&p[1..]);
                if p[0] == 0 {
                    let n = &p[7..];
                    let n = &n[..n.iter().position(|&c| c == 0).unwrap_or(n.len())];
                    if let Some(d) = self.devices.iter().find(|d| d.addr == a).cloned() {
                        self.found(a, d.le, d.random, d.psrm, d.class, d.clock, d.rssi, Some(n));
                    }
                }
                if let Some(s) = self.scan.as_mut() { s.name_pending = false; }
                self.scan_tick();
            }
            ev::CONN_REQUEST => {
                need!(10);
                let a = Addr::from_slice(p);
                if p[9] == 1 && self.state == State::Ready && !self.links.iter().any(|l| l.addr == a) {
                    self.log(format!("incoming connection from {}", a));
                    self.new_link(a, false, LinkState::Connecting);
                    let mut c = a.0.to_vec();
                    c.push(0x01); // remain peripheral; no role switch demanded
                    self.send_cmd(op::ACCEPT_CONN, &c);
                } else {
                    let mut c = a.0.to_vec();
                    c.push(0x0d);
                    self.send_cmd(op::REJECT_CONN, &c);
                }
            }
            ev::CONN_COMPLETE => {
                need!(11);
                if p[9] != 1 { return; }
                let a = Addr::from_slice(&p[3..]);
                let Some(i) = self.links.iter().position(|l| l.addr == a && l.state == LinkState::Connecting) else { return };
                if p[0] != 0 {
                    self.log(format!("connection to {} failed status 0x{:02x}", a, p[0]));
                    self.link_down(i, hci::status_of(p[0]));
                    return;
                }
                let now = self.now;
                let l = &mut self.links[i];
                l.handle = le16(p, 1) & 0x0fff;
                l.state = LinkState::Up;
                l.encrypted = p[10] != 0;
                l.last_activity = now;
                let h = l.handle;
                self.log(format!("connected {} handle 0x{:03x}", a, h));
                self.advance(a);
            }
            ev::DISCONN_COMPLETE => {
                need!(4);
                if p[0] != 0 { return; }
                let h = le16(p, 1) & 0x0fff;
                if let Some(i) = self.links.iter().position(|l| l.handle == h && l.state == LinkState::Up) {
                    self.log(format!("disconnected {} reason 0x{:02x}", self.links[i].addr, p[3]));
                    self.link_down(i, status::LINK_LOST);
                }
            }
            ev::AUTH_COMPLETE => {
                need!(3);
                let h = le16(p, 1) & 0x0fff;
                let Some(i) = self.link_by_handle(h) else { return };
                self.links[i].auth_pending = false;
                let a = self.links[i].addr;
                if p[0] == 0 {
                    self.links[i].authenticated = true;
                    self.links[i].last_activity = self.now;
                    self.start_security(i);
                    self.advance(a);
                } else if p[0] == 0x06 && !self.links[i].auth_retried && self.keys.get(a).is_some() {
                    // Our stored key is stale (the peer forgot us): drop it and pair afresh.
                    self.log(format!("{}: stale link key, re-pairing", a));
                    self.keys.remove(a);
                    self.keys_dirty = true;
                    self.links[i].auth_retried = true;
                    self.start_security(i);
                } else {
                    self.security_failed(h, p[0]);
                }
            }
            ev::ENCRYPT_CHANGE => {
                need!(4);
                let h = le16(p, 1) & 0x0fff;
                let Some(i) = self.link_by_handle(h) else { return };
                self.links[i].enc_pending = false;
                if p[0] == 0 && p[3] != 0 {
                    let l = &mut self.links[i];
                    l.encrypted = true;
                    l.authenticated = true;
                    let a = l.addr;
                    self.log(format!("{}: encrypted", a));
                    self.advance(a);
                } else if p[0] != 0 {
                    self.security_failed(h, p[0]);
                } else {
                    self.links[i].encrypted = false;
                }
            }
            ev::NUM_COMPLETED => {
                need!(1);
                for i in 0..p[0] as usize {
                    if p.len() < 5 + 4 * i { break; }
                    let h = le16(p, 1 + 4 * i) & 0x0fff;
                    let n = le16(p, 3 + 4 * i);
                    self.credit_back(h, n);
                }
            }
            ev::PIN_REQUEST => {
                need!(6);
                self.log(format!("{}: legacy PIN request, replying 0000", Addr::from_slice(p)));
                let mut c = p[..6].to_vec();
                c.push(4);
                let mut pin = [0u8; 16];
                pin[..4].copy_from_slice(b"0000");
                c.extend_from_slice(&pin);
                self.send_cmd(op::PIN_CODE_REPLY, &c);
            }
            ev::LINK_KEY_REQUEST => {
                need!(6);
                let a = Addr::from_slice(p);
                match self.keys.get(a).map(|k| k.key) {
                    Some(k) => {
                        let mut c = p[..6].to_vec();
                        c.extend_from_slice(&k);
                        self.send_cmd(op::LINK_KEY_REPLY, &c);
                    }
                    None => self.send_cmd(op::LINK_KEY_NEG_REPLY, &p[..6]),
                }
            }
            ev::LINK_KEY_NOTIFY => {
                need!(23);
                let a = Addr::from_slice(p);
                let mut key = [0u8; 16];
                key.copy_from_slice(&p[6..22]);
                let name = self.devices.iter().find(|d| d.addr == a).map(|d| d.name.clone()).unwrap_or_default();
                self.keys.set(Key { addr: a, key, ktype: p[22], name });
                self.keys_dirty = true;
                if !self.devices.iter().any(|d| d.addr == a) {
                    self.devices.push(Device { addr: a, psrm: 1, ..Default::default() });
                }
                self.log(format!("{}: paired (key type {})", a, p[22]));
            }
            ev::IO_CAP_REQUEST => {
                need!(6);
                let mut c = p[..6].to_vec();
                // NoInputNoOutput, no OOB data, general bonding without MITM: just-works.
                c.extend_from_slice(&[0x03, 0x00, 0x04]);
                self.send_cmd(op::IO_CAP_REPLY, &c);
            }
            ev::IO_CAP_RESPONSE => {
                need!(9);
                self.log(format!("{}: remote io_cap={} auth_req=0x{:02x}", Addr::from_slice(p), p[6], p[8]));
            }
            ev::USER_CONFIRM_REQUEST => {
                need!(10);
                let v = u32::from_le_bytes([p[6], p[7], p[8], p[9]]);
                self.log(format!("{}: confirm {:06} auto-accepted", Addr::from_slice(p), v));
                self.send_cmd(op::USER_CONFIRM_REPLY, &p[..6]);
            }
            ev::USER_PASSKEY_REQUEST => {
                need!(6);
                self.log(format!("{}: passkey entry not supported", Addr::from_slice(p)));
                self.send_cmd(op::USER_PASSKEY_NEG_REPLY, &p[..6]);
            }
            ev::SSP_COMPLETE => {
                need!(7);
                if p[0] != 0 {
                    self.log(format!("{}: simple pairing failed 0x{:02x}", Addr::from_slice(&p[1..]), p[0]));
                }
            }
            ev::LE_META => {
                need!(1);
                match p[0] {
                    ev::LE_CONN_COMPLETE if p.len() >= 12 && p[1] == 0 => {
                        let a = Addr::from_slice(&p[6..]);
                        let i = self.new_link(a, true, LinkState::Up);
                        self.links[i].handle = le16(p, 2) & 0x0fff;
                        self.log(format!("LE connection {} handle 0x{:03x}", a, self.links[i].handle));
                    }
                    ev::LE_ADV_REPORT if p.len() >= 2 => {
                        let mut i = 2;
                        for _ in 0..p[1] {
                            if p.len() < i + 10 { break; }
                            let l = p[i + 8] as usize;
                            if p.len() < i + 10 + l { break; }
                            let a = Addr::from_slice(&p[i + 2..]);
                            let name = hci::eir_name(&p[i + 9..i + 9 + l]).map(|n| n.to_vec());
                            let rssi = p[i + 9 + l] as i8;
                            self.found(a, true, p[i + 1] & 1 != 0, 0, 0, None, rssi, name.as_deref());
                            i += 10 + l;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // ── discovery ──────────────────────────────────────────────────────────
    #[allow(clippy::too_many_arguments)]
    fn found(&mut self, a: Addr, le: bool, random: bool, psrm: u8, class: u32, clock: Option<u16>, rssi: i8, name: Option<&[u8]>) {
        let (i, mut changed) = match self.devices.iter().position(|d| d.addr == a) {
            Some(i) => (i, false),
            None => {
                self.devices.push(Device { addr: a, ..Default::default() });
                (self.devices.len() - 1, true)
            }
        };
        let d = &mut self.devices[i];
        d.le = le;
        d.random = random;
        if !le { d.psrm = psrm; d.class = class; d.clock = clock; }
        if rssi != -127 { d.rssi = rssi; }
        if let Some(n) = name {
            let n = String::from_utf8_lossy(n).into_owned();
            if !n.is_empty() && n != d.name { d.name = n; changed = true; }
        }
        let name = d.name.clone();
        if let Some(k) = self.keys.keys.iter_mut().find(|k| k.addr == a) {
            if !name.is_empty() && k.name != name { k.name = name; self.keys_dirty = true; }
        }
        if changed {
            if let Some(o) = self.scan.as_ref().map(|s| s.owner) {
                let di = self.dev_info(i);
                self.emit(o, Msg::Device(di));
            }
        }
    }

    fn dev_info(&self, i: usize) -> DeviceInfo {
        let d = &self.devices[i];
        let mut flags = 0;
        if self.keys.get(d.addr).is_some() { flags |= dev_flags::PAIRED; }
        if self.links.iter().any(|l| l.addr == d.addr && l.state == LinkState::Up) { flags |= dev_flags::CONNECTED; }
        if d.le { flags |= dev_flags::LE; }
        if d.random { flags |= dev_flags::LE_RANDOM; }
        DeviceInfo { addr: d.addr, flags, rssi: d.rssi, class: d.class, name: d.name.clone() }
    }

    fn req_scan(&mut self, owner: Owner, secs: u8) {
        if self.state != State::Ready { return self.result(owner, aop::SCAN, status::NOT_READY); }
        if secs == 0 {
            if let Some(s) = self.scan.take() {
                if s.inquiry { self.send_cmd(op::INQUIRY_CANCEL, &[]); }
                if s.le { self.send_cmd(op::LE_SET_SCAN_ENABLE, &[0, 0]); }
                if s.owner != owner { self.result(s.owner, aop::SCAN, status::OK); }
            }
            return self.result(owner, aop::SCAN, status::OK);
        }
        if self.scan.is_some() { return self.result(owner, aop::SCAN, status::BUSY); }
        let len = ((secs as u32 * 1000).div_ceil(1280)).clamp(1, 0x30) as u8;
        let mut c = hci::GIAC.to_vec();
        c.extend_from_slice(&[len, 0]);
        self.send_cmd(op::INQUIRY, &c);
        let le = self.info.le();
        if le {
            self.send_cmd(op::LE_SET_SCAN_PARAMS, &[1, 0x10, 0, 0x10, 0, 0, 0]);
            self.send_cmd(op::LE_SET_SCAN_ENABLE, &[1, 1]);
        }
        self.scan = Some(Scan { owner, until: self.now + secs as u64 * 1000, inquiry: true, le, names: Vec::new(), name_pending: false });
    }

    fn inquiry_ended(&mut self) {
        let mut names: Vec<Addr> = self.devices.iter().filter(|d| !d.le && d.name.is_empty() && d.clock.is_some()).map(|d| d.addr).collect();
        names.truncate(8);
        if let Some(s) = self.scan.as_mut() {
            if !s.inquiry { return; }
            s.inquiry = false;
            s.names = names;
        }
        self.scan_tick();
    }

    fn scan_tick(&mut self) {
        let now = self.now;
        let Some(s) = self.scan.as_mut() else { return };
        let mut cmds: Vec<(u16, Vec<u8>)> = Vec::new();
        let expired = now >= s.until;
        if expired && s.le { s.le = false; cmds.push((op::LE_SET_SCAN_ENABLE, vec![0, 0])); }
        if expired && s.inquiry { cmds.push((op::INQUIRY_CANCEL, Vec::new())); }
        let mut finish = false;
        if !s.inquiry && !s.name_pending {
            if let Some(a) = s.names.pop() {
                s.name_pending = true;
                let d = self.devices.iter().find(|d| d.addr == a);
                let (psrm, clock) = d.map(|d| (d.psrm, d.clock.unwrap_or(0) | 0x8000)).unwrap_or((1, 0));
                let mut c = a.0.to_vec();
                c.extend_from_slice(&[psrm, 0]);
                c.extend_from_slice(&clock.to_le_bytes());
                cmds.push((op::REMOTE_NAME_REQ, c));
            } else if expired && !s.le {
                finish = true;
            }
        }
        if now >= s.until + 10_000 { finish = true; }
        for (o, c) in cmds { self.send_cmd(o, &c); }
        if finish {
            let s = self.scan.take().unwrap();
            if s.le { self.send_cmd(op::LE_SET_SCAN_ENABLE, &[0, 0]); }
            self.result(s.owner, aop::SCAN, status::OK);
        }
    }

    // ── links and security ─────────────────────────────────────────────────
    fn new_link(&mut self, a: Addr, le: bool, state: LinkState) -> usize {
        self.links.push(Link {
            addr: a, handle: 0xffff, le, state, since: self.now, authenticated: false, encrypted: false,
            auth_pending: false, enc_pending: false, auth_retried: false, rx: Vec::new(),
            last_activity: self.now, outstanding: 0,
        });
        self.links.len() - 1
    }

    fn link_by_handle(&self, h: u16) -> Option<usize> {
        self.links.iter().position(|l| l.handle == h && l.state == LinkState::Up)
    }

    fn up_link(&self, a: Addr) -> Option<usize> {
        self.links.iter().position(|l| l.addr == a && l.state == LinkState::Up)
    }

    /// Finds or pages the BR/EDR link to `a`.
    fn ensure_link(&mut self, a: Addr) -> Option<usize> {
        if let Some(i) = self.links.iter().position(|l| l.addr == a) { return Some(i); }
        let d = self.devices.iter().find(|d| d.addr == a);
        if d.is_some_and(|d| d.le) { return None; }
        let (psrm, clock) = d.map(|d| (d.psrm, d.clock.map(|c| c | 0x8000).unwrap_or(0))).unwrap_or((1, 0));
        let mut c = a.0.to_vec();
        c.extend_from_slice(&0xcc18u16.to_le_bytes());
        c.extend_from_slice(&[psrm, 0]);
        c.extend_from_slice(&clock.to_le_bytes());
        c.push(1);
        self.send_cmd(op::CREATE_CONN, &c);
        Some(self.new_link(a, false, LinkState::Connecting))
    }

    fn start_security(&mut self, i: usize) {
        let l = &mut self.links[i];
        if l.auth_pending || l.enc_pending || l.encrypted { return; }
        let h = l.handle.to_le_bytes();
        if !l.authenticated {
            l.auth_pending = true;
            self.send_cmd(op::AUTH_REQUESTED, &h);
        } else {
            l.enc_pending = true;
            self.send_cmd(op::SET_CONN_ENCRYPT, &[h[0], h[1], 1]);
        }
    }

    /// Moves channels and pairings on `a` forward once the link allows it.
    fn advance(&mut self, a: Addr) {
        let Some(li) = self.up_link(a) else { return };
        let enc = self.links[li].encrypted;
        let handle = self.links[li].handle;
        let needs_sec = self.pairs.iter().any(|p| p.1 == a)
            || self.chans.iter().any(|c| c.addr == a && c.state == ChanState::WaitLink && c.psm != PSM_SDP);
        if needs_sec && !enc { self.start_security(li); }
        let now = self.now;
        let mut reqs = Vec::new();
        for c in self.chans.iter_mut() {
            if c.addr == a && c.state == ChanState::WaitLink && (enc || c.psm == PSM_SDP) {
                c.state = ChanState::WaitConnRsp;
                c.deadline = now + 10_000;
                c.ident = self.next_ident;
                self.next_ident = if self.next_ident == 255 { 1 } else { self.next_ident + 1 };
                let mut d = c.psm.to_le_bytes().to_vec();
                d.extend_from_slice(&c.cid.to_le_bytes());
                reqs.push((c.ident, d));
            }
        }
        for (id, d) in reqs { self.sig_raw(handle, 0x02, id, &d); }
        if enc {
            let done: Vec<Owner> = self.pairs.iter().filter(|p| p.1 == a).map(|p| p.0).collect();
            self.pairs.retain(|p| p.1 != a);
            for o in done { self.result(o, aop::PAIR, status::OK); }
        }
    }

    fn security_failed(&mut self, h: u16, hci_st: u8) {
        let Some(i) = self.link_by_handle(h) else { return };
        let a = self.links[i].addr;
        self.links[i].auth_pending = false;
        self.links[i].enc_pending = false;
        let st = match hci_st { 0x05 | 0x06 | 0x18 | 0x29 | 0x25 | 0x26 => status::AUTH_FAILED, s => hci::status_of(s) };
        self.log(format!("{}: authentication failed 0x{:02x}", a, hci_st));
        let failed: Vec<(Owner, u16, u16)> = self.chans.iter()
            .filter(|c| c.addr == a && c.state == ChanState::WaitLink && c.psm != PSM_SDP)
            .map(|c| (c.owner, c.req.unwrap_or(0), c.cid)).collect();
        self.chans.retain(|c| !(c.addr == a && c.state == ChanState::WaitLink && c.psm != PSM_SDP));
        for (o, r, _) in failed { self.emit(o, Msg::ConnectResult { req: r, status: st, cid: 0, peer_mtu: 0 }); }
        let done: Vec<Owner> = self.pairs.iter().filter(|p| p.1 == a).map(|p| p.0).collect();
        self.pairs.retain(|p| p.1 != a);
        for o in done { self.result(o, aop::PAIR, st); }
    }

    fn link_down(&mut self, i: usize, st: u8) {
        let l = self.links.remove(i);
        let a = l.addr;
        if l.le && self.info.le_pkts > 0 {
            self.le_avail = (self.le_avail + l.outstanding).min(self.info.le_pkts);
        } else {
            self.acl_avail = (self.acl_avail + l.outstanding).min(self.info.acl_pkts);
        }
        if l.state == LinkState::Up { self.acl_txq.retain(|q| q.0 != l.handle); }
        let mut evs = Vec::new();
        for c in self.chans.iter().filter(|c| c.addr == a) {
            match (c.state, c.req) {
                (ChanState::Open, _) => evs.push((c.owner, Msg::Closed { cid: c.cid, reason: st })),
                (ChanState::Closing, _) | (_, None) => {}
                (_, Some(r)) => evs.push((c.owner, Msg::ConnectResult { req: r, status: st, cid: 0, peer_mtu: 0 })),
            }
        }
        self.chans.retain(|c| c.addr != a);
        let done: Vec<Owner> = self.pairs.iter().filter(|p| p.1 == a).map(|p| p.0).collect();
        self.pairs.retain(|p| p.1 != a);
        for o in done { evs.push((o, Msg::Result { op: aop::PAIR, status: st })); }
        for (o, m) in evs { self.emit(o, m); }
    }

    // ── ACL ────────────────────────────────────────────────────────────────
    fn send_l2cap(&mut self, handle: u16, le: bool, cid: u16, payload: &[u8]) {
        let mut pdu = Vec::with_capacity(4 + payload.len());
        pdu.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        pdu.extend_from_slice(&cid.to_le_bytes());
        pdu.extend_from_slice(payload);
        let mtu = if le && self.info.le_mtu > 0 { self.info.le_mtu } else { self.info.acl_mtu };
        let mtu = if mtu == 0 { 27 } else { mtu as usize };
        for (n, chunk) in pdu.chunks(mtu).enumerate() {
            let pb = if n > 0 { 0b01 } else if le { 0b00 } else { 0b10 };
            self.acl_txq.push_back((handle, le, hci::acl(handle, pb, chunk)));
        }
        self.pump_acl();
    }

    fn pump_acl(&mut self) {
        while let Some((h, le, _)) = self.acl_txq.front() {
            let (h, le) = (*h, *le);
            let pool = if le && self.info.le_pkts > 0 { &mut self.le_avail } else { &mut self.acl_avail };
            if *pool == 0 { break; }
            *pool -= 1;
            let (_, _, pkt) = self.acl_txq.pop_front().unwrap();
            if let Some(l) = self.links.iter_mut().find(|l| l.handle == h && l.state == LinkState::Up) { l.outstanding += 1; }
            self.out.push_back(pkt);
        }
    }

    fn credit_back(&mut self, h: u16, n: u16) {
        let Some(l) = self.links.iter_mut().find(|l| l.handle == h && l.state == LinkState::Up) else { return };
        l.outstanding = l.outstanding.saturating_sub(n);
        if l.le && self.info.le_pkts > 0 {
            self.le_avail = (self.le_avail + n).min(self.info.le_pkts);
        } else {
            self.acl_avail = (self.acl_avail + n).min(self.info.acl_pkts);
        }
    }

    fn on_acl(&mut self, pkt: &[u8]) {
        let hraw = le16(pkt, 1);
        let (h, pb) = (hraw & 0x0fff, (hraw >> 12) & 3);
        let len = (le16(pkt, 3) as usize).min(pkt.len() - 5);
        let data = &pkt[5..5 + len];
        let Some(i) = self.link_by_handle(h) else { return };
        let le = self.links[i].le;
        self.links[i].last_activity = self.now;
        let mut rx = core::mem::take(&mut self.links[i].rx);
        if pb == 0b01 { rx.extend_from_slice(data); } else { rx.clear(); rx.extend_from_slice(data); }
        let mut pdus = Vec::new();
        while rx.len() >= 4 {
            let l = le16(&rx, 0) as usize + 4;
            if rx.len() < l { break; }
            pdus.push(rx.drain(..l).collect::<Vec<u8>>());
        }
        if let Some(i) = self.link_by_handle(h) { self.links[i].rx = rx; }
        for p in pdus { self.on_l2cap(h, le, le16(&p, 2), &p[4..]); }
    }

    fn on_l2cap(&mut self, h: u16, le: bool, cid: u16, p: &[u8]) {
        match cid {
            CID_SIG if !le => {
                let mut p = p;
                while p.len() >= 4 {
                    let l = le16(p, 2) as usize;
                    if p.len() < 4 + l { break; }
                    self.on_sig(h, p[0], p[1], &p[4..4 + l]);
                    p = &p[4 + l..];
                }
            }
            CID_LE_SIG if le && p.len() >= 4 => match p[0] {
                0x12 => self.send_l2cap(h, true, CID_LE_SIG, &[0x13, p[1], 2, 0, 1, 0]),
                c if c & 1 == 1 || c == 0x01 => {}
                _ => self.send_l2cap(h, true, CID_LE_SIG, &[0x01, p[1], 2, 0, 0, 0]),
            },
            CID_ATT if le && !p.is_empty() => match p[0] {
                0x02 => self.send_l2cap(h, true, CID_ATT, &[0x03, 23, 0]),
                0x1d => self.send_l2cap(h, true, CID_ATT, &[0x1e]),
                o if o & 1 == 0 && o & 0x40 == 0 && o != 0x1e => self.send_l2cap(h, true, CID_ATT, &[0x01, o, 0, 0, 0x06]),
                _ => {}
            },
            CID_SMP if le && !p.is_empty() => {
                if p[0] == 0x01 || p[0] == 0x0b { self.send_l2cap(h, true, CID_SMP, &[0x05, 0x05]); }
            }
            c if c >= FIRST_DYN_CID => {
                let Some(a) = self.links.iter().find(|l| l.handle == h).map(|l| l.addr) else { return };
                if let Some(ch) = self.chans.iter().find(|ch| ch.cid == c && ch.addr == a && ch.state == ChanState::Open) {
                    let (o, cid) = (ch.owner, ch.cid);
                    self.emit(o, Msg::Recv { cid, data: p.to_vec() });
                }
            }
            _ => {}
        }
    }

    // ── L2CAP signalling (BR/EDR, CID 1) ───────────────────────────────────
    fn sig_raw(&mut self, h: u16, code: u8, ident: u8, d: &[u8]) {
        let mut p = vec![code, ident];
        p.extend_from_slice(&(d.len() as u16).to_le_bytes());
        p.extend_from_slice(d);
        self.send_l2cap(h, false, CID_SIG, &p);
    }

    fn sig(&mut self, h: u16, code: u8, d: &[u8]) -> u8 {
        let id = self.next_ident;
        self.next_ident = if id == 255 { 1 } else { id + 1 };
        self.sig_raw(h, code, id, d);
        id
    }

    fn chan_handle(&self, ci: usize) -> Option<u16> {
        self.up_link(self.chans[ci].addr).map(|i| self.links[i].handle)
    }

    fn send_config_req(&mut self, ci: usize) {
        let Some(h) = self.chan_handle(ci) else { return };
        let mut d = self.chans[ci].rcid.to_le_bytes().to_vec();
        d.extend_from_slice(&[0, 0]); // no options: our MTU is the 672 default
        self.sig(h, 0x04, &d);
    }

    fn check_open(&mut self, ci: usize) {
        let c = &mut self.chans[ci];
        if c.state != (ChanState::Config { ours_ok: true, theirs_ok: true }) { return; }
        c.state = ChanState::Open;
        c.deadline = u64::MAX;
        let m = match c.req {
            Some(req) => Msg::ConnectResult { req, status: status::OK, cid: c.cid, peer_mtu: c.peer_mtu },
            None => Msg::Incoming { cid: c.cid, addr: c.addr, psm: c.psm, peer_mtu: c.peer_mtu },
        };
        let (o, psm, cid, a) = (c.owner, c.psm, c.cid, c.addr);
        self.log(format!("{}: channel psm 0x{:04x} cid 0x{:04x} open", a, psm, cid));
        self.emit(o, m);
    }

    fn fail_chan(&mut self, ci: usize, st: u8) {
        let c = self.chans.remove(ci);
        match (c.state, c.req) {
            (ChanState::Closing, _) => {}
            (ChanState::Open, _) => self.emit(c.owner, Msg::Closed { cid: c.cid, reason: st }),
            (_, Some(r)) => self.emit(c.owner, Msg::ConnectResult { req: r, status: st, cid: 0, peer_mtu: 0 }),
            (_, None) => {}
        }
    }

    fn alloc_cid(&mut self) -> u16 {
        loop {
            let c = self.next_cid;
            self.next_cid = if c >= 0xffff { FIRST_DYN_CID } else { c + 1 };
            if !self.chans.iter().any(|ch| ch.cid == c) { return c; }
        }
    }

    fn on_sig(&mut self, h: u16, code: u8, id: u8, d: &[u8]) {
        let Some(a) = self.links.iter().find(|l| l.handle == h).map(|l| l.addr) else { return };
        let now = self.now;
        let find = |s: &Self, cid: u16| s.chans.iter().position(|c| c.cid == cid && c.addr == a);
        match code {
            0x02 if d.len() >= 4 => {
                let (psm, scid) = (le16(d, 0), le16(d, 2));
                let Some(owner) = self.listens.iter().find(|l| l.0 == psm).map(|l| l.1) else {
                    self.log(format!("{}: refusing psm 0x{:04x}", a, psm));
                    let mut r = vec![0, 0];
                    r.extend_from_slice(&scid.to_le_bytes());
                    r.extend_from_slice(&[2, 0, 0, 0]);
                    return self.sig_raw(h, 0x03, id, &r);
                };
                let cid = self.alloc_cid();
                self.chans.push(Chan {
                    cid, rcid: scid, psm, addr: a, owner, req: None,
                    state: ChanState::Config { ours_ok: false, theirs_ok: false },
                    peer_mtu: OUR_MTU, deadline: now + 10_000, ident: id,
                });
                let mut r = cid.to_le_bytes().to_vec();
                r.extend_from_slice(&scid.to_le_bytes());
                r.extend_from_slice(&[0, 0, 0, 0]);
                self.sig_raw(h, 0x03, id, &r);
                let ci = self.chans.len() - 1;
                self.send_config_req(ci);
            }
            0x03 if d.len() >= 8 => {
                let (dcid, scid, result) = (le16(d, 0), le16(d, 2), le16(d, 4));
                let Some(ci) = find(self, scid) else { return };
                let st = self.chans[ci].state;
                if st == ChanState::Closing {
                    if result == 0 {
                        let mut r = dcid.to_le_bytes().to_vec();
                        r.extend_from_slice(&scid.to_le_bytes());
                        self.sig(h, 0x06, &r);
                    }
                    if result != 1 { self.chans.remove(ci); }
                    return;
                }
                if st != ChanState::WaitConnRsp { return; }
                match result {
                    0 => {
                        let c = &mut self.chans[ci];
                        c.rcid = dcid;
                        c.state = ChanState::Config { ours_ok: false, theirs_ok: false };
                        c.deadline = now + 10_000;
                        self.send_config_req(ci);
                    }
                    1 => self.chans[ci].deadline = now + 30_000,
                    r => {
                        let st = match r { 3 => status::SECURITY_BLOCK, 4 => status::NO_RESOURCES, _ => status::REFUSED };
                        self.log(format!("{}: psm 0x{:04x} refused result {}", a, self.chans[ci].psm, r));
                        self.fail_chan(ci, st);
                    }
                }
            }
            0x04 if d.len() >= 4 => {
                let dcid = le16(d, 0);
                let Some(ci) = find(self, dcid) else {
                    let mut r = vec![2, 0];
                    r.extend_from_slice(&dcid.to_le_bytes());
                    r.extend_from_slice(&[0, 0]);
                    return self.sig_raw(h, 0x01, id, &r);
                };
                let o = &d[4..];
                let (mut result, mut opts, mut mtu, mut i) = (0u16, Vec::new(), None, 0);
                while i + 2 <= o.len() {
                    let (t, l) = (o[i], o[i + 1] as usize);
                    let Some(v) = o.get(i + 2..i + 2 + l) else { break };
                    match t & 0x7f {
                        0x01 if l >= 2 => mtu = Some(le16(v, 0)),
                        0x02 | 0x03 | 0x05 | 0x06 | 0x07 => {}
                        0x04 if l >= 1 && v[0] != 0 => {
                            // Only basic mode: counter-propose it.
                            result = 1;
                            opts.extend_from_slice(&[0x04, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
                        }
                        0x04 => {}
                        _ if t & 0x80 != 0 => {}
                        _ => {
                            if result != 1 { result = 3; opts.extend_from_slice(&o[i..i + 2 + l]); }
                        }
                    }
                    i += 2 + l;
                }
                let c = &mut self.chans[ci];
                if result == 0 {
                    if let Some(m) = mtu { c.peer_mtu = m.max(48); }
                    if let ChanState::Config { ours_ok, .. } = c.state { c.state = ChanState::Config { ours_ok, theirs_ok: true }; }
                }
                let mut r = c.rcid.to_le_bytes().to_vec();
                r.extend_from_slice(&[0, 0]);
                r.extend_from_slice(&result.to_le_bytes());
                r.extend_from_slice(&opts);
                self.sig_raw(h, 0x05, id, &r);
                self.check_open(ci);
            }
            0x05 if d.len() >= 6 => {
                let (scid, result) = (le16(d, 0), le16(d, 4));
                let Some(ci) = find(self, scid) else { return };
                let ChanState::Config { theirs_ok, .. } = self.chans[ci].state else { return };
                match result {
                    0 => {
                        self.chans[ci].state = ChanState::Config { ours_ok: true, theirs_ok };
                        self.check_open(ci);
                    }
                    4 => {}
                    _ => {
                        self.log(format!("{}: config rejected result {}", a, result));
                        self.close_chan(ci);
                        let c = &self.chans[ci];
                        if let Some(r) = c.req {
                            let (o, r) = (c.owner, r);
                            self.emit(o, Msg::ConnectResult { req: r, status: status::REFUSED, cid: 0, peer_mtu: 0 });
                        }
                        self.chans[ci].req = None;
                    }
                }
            }
            0x06 if d.len() >= 4 => {
                let (dcid, scid) = (le16(d, 0), le16(d, 2));
                self.sig_raw(h, 0x07, id, &d[..4]);
                if let Some(ci) = find(self, dcid) {
                    if self.chans[ci].rcid == scid { self.fail_chan(ci, status::LINK_LOST); }
                }
            }
            0x07 if d.len() >= 4 => {
                let scid = le16(d, 2);
                if let Some(ci) = find(self, scid) {
                    if self.chans[ci].state == ChanState::Closing { self.chans.remove(ci); }
                }
            }
            0x08 => self.sig_raw(h, 0x09, id, d),
            0x0a if d.len() >= 2 => {
                let t = le16(d, 0);
                let r: Vec<u8> = match t {
                    1 => vec![1, 0, 0, 0, 0xa0, 0x02],
                    2 => vec![2, 0, 0, 0, 0x80, 0, 0, 0],
                    3 => vec![3, 0, 0, 0, 0x02, 0, 0, 0, 0, 0, 0, 0],
                    _ => { let mut v = t.to_le_bytes().to_vec(); v.extend_from_slice(&[1, 0]); v }
                };
                self.sig_raw(h, 0x0b, id, &r);
            }
            0x01 => {
                if let Some(ci) = self.chans.iter().position(|c| c.addr == a && c.ident == id && c.state == ChanState::WaitConnRsp) {
                    self.fail_chan(ci, status::REFUSED);
                }
            }
            0x09 | 0x0b => {}
            _ => self.sig_raw(h, 0x01, id, &[0, 0]),
        }
    }

    /// Sends a disconnection request and parks the channel in Closing.
    fn close_chan(&mut self, ci: usize) {
        let st = self.chans[ci].state;
        if matches!(st, ChanState::Open | ChanState::Config { .. }) {
            if let Some(h) = self.chan_handle(ci) {
                let mut r = self.chans[ci].rcid.to_le_bytes().to_vec();
                r.extend_from_slice(&self.chans[ci].cid.to_le_bytes());
                self.sig(h, 0x06, &r);
            }
        }
        let c = &mut self.chans[ci];
        c.state = ChanState::Closing;
        c.deadline = self.now + 5000;
    }

    // ── client requests ────────────────────────────────────────────────────
    pub fn request(&mut self, owner: Owner, m: Msg) {
        match m {
            Msg::Connect { req, addr, psm } => {
                let fail = |s: &mut Self, st| s.emit(owner, Msg::ConnectResult { req, status: st, cid: 0, peer_mtu: 0 });
                if self.state != State::Ready { return fail(self, status::NOT_READY); }
                if psm & 0x0101 != 0x0001 { return fail(self, status::BAD_REQUEST); }
                if self.ensure_link(addr).is_none() { return fail(self, status::REFUSED); }
                let cid = self.alloc_cid();
                self.chans.push(Chan {
                    cid, rcid: 0, psm, addr, owner, req: Some(req), state: ChanState::WaitLink,
                    peer_mtu: OUR_MTU, deadline: self.now + 40_000, ident: 0,
                });
                self.advance(addr);
            }
            Msg::Listen { psm } => {
                let st = match self.listens.iter().find(|l| l.0 == psm) {
                    Some(l) if l.1 != owner => status::BUSY,
                    Some(_) => status::OK,
                    None => { self.listens.push((psm, owner)); status::OK }
                };
                self.result(owner, aop::LISTEN, st);
            }
            Msg::Send { cid, data } => {
                let ok = self.chans.iter().position(|c| c.cid == cid && c.owner == owner && c.state == ChanState::Open);
                let Some(ci) = ok else { return self.emit(owner, Msg::Closed { cid, reason: status::BAD_REQUEST }) };
                if data.len() > self.chans[ci].peer_mtu as usize {
                    return self.log(format!("cid 0x{:04x}: dropping {} bytes > peer mtu {}", cid, data.len(), self.chans[ci].peer_mtu));
                }
                if let Some(h) = self.chan_handle(ci) {
                    let r = self.chans[ci].rcid;
                    self.send_l2cap(h, false, r, &data);
                }
            }
            Msg::Close { cid } => {
                let Some(ci) = self.chans.iter().position(|c| c.cid == cid && c.owner == owner && c.state != ChanState::Closing) else { return };
                if self.chans[ci].state == ChanState::WaitLink { self.chans.remove(ci); } else {
                    self.close_chan(ci);
                    self.chans[ci].req = None;
                }
                self.emit(owner, Msg::Closed { cid, reason: status::LOCAL_CLOSE });
            }
            Msg::Scan { secs } => self.req_scan(owner, secs),
            Msg::Devices => {
                for i in 0..self.devices.len() { let d = self.dev_info(i); self.emit(owner, Msg::Device(d)); }
                self.result(owner, aop::DEVICES, status::OK);
            }
            Msg::Pair { addr } => {
                if self.state != State::Ready { return self.result(owner, aop::PAIR, status::NOT_READY); }
                if self.ensure_link(addr).is_none() { return self.result(owner, aop::PAIR, status::REFUSED); }
                self.pairs.push((owner, addr));
                self.advance(addr);
            }
            Msg::Remove { addr } => {
                let had = self.keys.remove(addr);
                if had { self.keys_dirty = true; }
                if let Some(i) = self.up_link(addr) {
                    let h = self.links[i].handle.to_le_bytes();
                    self.send_cmd(op::DISCONNECT, &[h[0], h[1], 0x13]);
                }
                self.devices.retain(|d| d.addr != addr);
                self.result(owner, aop::REMOVE, if had { status::OK } else { status::BAD_REQUEST });
            }
            Msg::Disconnect { addr } => {
                let st = match self.up_link(addr) {
                    Some(i) => {
                        let h = self.links[i].handle.to_le_bytes();
                        self.send_cmd(op::DISCONNECT, &[h[0], h[1], 0x13]);
                        status::OK
                    }
                    None => status::BAD_REQUEST,
                };
                self.result(owner, aop::DISCONNECT, st);
            }
            Msg::Info => {
                let i = &self.info;
                let st = match self.state { State::Init => 0, State::Ready => 1, State::Dead => 2 };
                let m = Msg::InfoRsp { addr: i.addr, hci_ver: i.hci_ver, lmp_subver: i.lmp_subver, manufacturer: i.manufacturer, state: st, name: self.name.clone() };
                self.emit(owner, m);
            }
            _ => self.result(owner, 0, status::BAD_REQUEST),
        }
        self.pump_cmds();
    }

    /// A client went away: close its channels, drop its listens, scans and pairings.
    pub fn drop_owner(&mut self, owner: Owner) {
        self.listens.retain(|l| l.1 != owner);
        self.pairs.retain(|p| p.0 != owner);
        if self.scan.as_ref().is_some_and(|s| s.owner == owner) { self.scan.as_mut().unwrap().owner = NOBODY; }
        let mut i = 0;
        while i < self.chans.len() {
            if self.chans[i].owner == owner {
                if self.chans[i].state == ChanState::WaitLink { self.chans.remove(i); continue; }
                if self.chans[i].state != ChanState::Closing { self.close_chan(i); }
                self.chans[i].owner = NOBODY;
                self.chans[i].req = None;
            }
            i += 1;
        }
    }

    // ── time ───────────────────────────────────────────────────────────────
    pub fn tick(&mut self, now: u64) {
        self.now = now;
        if let Some((o, t, _)) = &self.inflight {
            if now.saturating_sub(*t) > CMD_TIMEOUT_MS {
                let o = *o;
                self.log(format!("command 0x{:04x} timed out", o));
                self.inflight = None;
                self.credits = 1;
                if self.state == State::Init { self.init_result(o, 0xff, &[]); }
            }
        }
        let mut ci = 0;
        while ci < self.chans.len() {
            if now >= self.chans[ci].deadline {
                self.log(format!("cid 0x{:04x}: timeout in {:?}", self.chans[ci].cid, self.chans[ci].state));
                self.fail_chan(ci, status::TIMEOUT);
            } else {
                ci += 1;
            }
        }
        let mut li = 0;
        while li < self.links.len() {
            let l = &self.links[li];
            if l.state == LinkState::Connecting && now.saturating_sub(l.since) > 30_000 {
                self.link_down(li, status::TIMEOUT);
                continue;
            }
            let a = l.addr;
            let idle = l.state == LinkState::Up && !l.le && !l.auth_pending && !l.enc_pending
                && now.saturating_sub(l.last_activity) > IDLE_DISCONNECT_MS
                && !self.chans.iter().any(|c| c.addr == a) && !self.pairs.iter().any(|p| p.1 == a);
            if idle {
                let h = self.links[li].handle.to_le_bytes();
                self.links[li].last_activity = now;
                self.log(format!("{}: idle, disconnecting", a));
                self.send_cmd(op::DISCONNECT, &[h[0], h[1], 0x13]);
            }
            li += 1;
        }
        self.scan_tick();
        self.pump_cmds();
        self.pump_acl();
    }
}
