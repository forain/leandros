//! bthid — Bluetooth HID host (classic HIDP) on top of btd.
//!
//! ```text
//! bthid [daemon]          run the host: listen on PSM 0x11/0x13 for paired
//!                         devices reconnecting, connect listed ones
//! bthid connect ADDR      remember ADDR and ask the daemon to connect it now
//! bthid forget ADDR       stop connecting ADDR (does not unpair: btctl remove)
//! bthid list              remembered devices
//! ```
//!
//! Each connected device gets one evdev node per role (gamepad / keyboard /
//! mouse) through `/dev/uinput`, so a Bluetooth gamepad looks exactly like a
//! wired one to MAME, SDL and libinput. Report parsing, the gamepad
//! normalisation and the per-controller quirks live in `report.rs`
//! (host-tested in drivers/bthid-tests).
//!
//! Flow (Bluetooth HID profile 1.1): SDP on PSM 1 for the PnP record
//! (vendor/product) and the HID record (report descriptor), cached in
//! `/var/lib/bluetooth/hid-<ADDR>`; then L2CAP control (0x11) and interrupt
//! (0x13). A device that reconnects by itself (most gamepads, keyboards and
//! mice) opens 0x11/0x13 towards us; btd hands those channels to the client
//! that LISTENs on the PSMs, which is this daemon. btd authenticates and
//! encrypts the link before any HID channel opens (docs/servers/bluetooth.md).

#![no_std]
#![no_main]

extern crate alloc;
extern crate leandros_libc;

#[macro_use]
#[path = "../../netctl/src/common.rs"]
#[allow(unused_macros)]
mod common;
pub mod report;

use alloc::string::String;
use alloc::vec::Vec;
use common::*;
use leandros_libc::syscall::{syscall3, syscall4};
use report::{HidDevice, Role, Ev};

// ── btd client (docs/servers/bluetooth.md section B) ───────────────────────
const BTD_SOCK: &[u8] = b"/run/bluetooth/btd.sock";
const OP_CONNECT: u8 = 0x01;
const OP_LISTEN: u8 = 0x02;
const OP_SEND: u8 = 0x03;
const OP_CLOSE: u8 = 0x04;
const OP_DEVICES: u8 = 0x11;
const EV_CONNECT_RESULT: u8 = 0x81;
const EV_RECV: u8 = 0x83;
const EV_CLOSED: u8 = 0x84;
const EV_INCOMING: u8 = 0x85;
const EV_DEVICE: u8 = 0x90;
const EV_RESULT: u8 = 0x91;

#[cfg(target_arch = "aarch64")] const SYS_SOCKET: usize = 198;
#[cfg(target_arch = "x86_64")]  const SYS_SOCKET: usize = 41;
#[cfg(target_arch = "aarch64")] const SYS_CONNECT: usize = 203;
#[cfg(target_arch = "x86_64")]  const SYS_CONNECT: usize = 42;

const LIST: &[u8] = b"/var/lib/bluetooth/hid-devices\0";
const REQ: &[u8] = b"/run/bluetooth/bthid.connect\0";
const O_CREAT: i32 = 0o100;
const O_APPEND: i32 = 0o2000;

struct Btd { fd: i32, buf: Vec<u8> }

impl Btd {
    fn open() -> Result<Btd, i32> {
        let fd = unsafe { syscall3(SYS_SOCKET, 1, 1 | 0o2000000, 0) };
        if fd < 0 { return Err(-fd as i32); }
        let fd = fd as i32;
        let mut sa = [0u8; 110];
        sa[0] = 1; // AF_UNIX
        sa[2..2 + BTD_SOCK.len()].copy_from_slice(BTD_SOCK);
        let r = unsafe { syscall3(SYS_CONNECT, fd as usize, sa.as_ptr() as usize, 2 + BTD_SOCK.len() + 1) };
        if r < 0 { sys_close(fd); return Err(-r as i32); }
        Ok(Btd { fd, buf: Vec::new() })
    }
    fn frame(&self, op: u8, payload: &[u8]) -> Result<(), i32> {
        let mut f = Vec::with_capacity(3 + payload.len());
        f.extend_from_slice(&((1 + payload.len()) as u16).to_le_bytes());
        f.push(op);
        f.extend_from_slice(payload);
        let mut off = 0;
        while off < f.len() { off += sys_write(self.fd, &f[off..])?; }
        Ok(())
    }
    fn connect(&self, req: u16, addr: &[u8; 6], psm: u16) -> Result<(), i32> {
        let mut p = Vec::new();
        p.extend_from_slice(&req.to_le_bytes()); p.extend_from_slice(addr); p.extend_from_slice(&psm.to_le_bytes());
        self.frame(OP_CONNECT, &p)
    }
    fn send(&self, cid: u16, data: &[u8]) -> Result<(), i32> {
        let mut p = Vec::new();
        p.extend_from_slice(&cid.to_le_bytes()); p.extend_from_slice(data);
        self.frame(OP_SEND, &p)
    }
    fn close(&self, cid: u16) { let _ = self.frame(OP_CLOSE, &cid.to_le_bytes()); }
    /// Next whole frame (op, payload) if one is buffered.
    fn next(&mut self) -> Option<(u8, Vec<u8>)> {
        if self.buf.len() < 3 { return None; }
        let n = u16::from_le_bytes([self.buf[0], self.buf[1]]) as usize;
        if n == 0 { self.buf.drain(..2); return None; }
        if self.buf.len() < 2 + n { return None; }
        let f: Vec<u8> = self.buf.drain(..2 + n).collect();
        Some((f[2], f[3..].to_vec()))
    }
    /// Read whatever is available; Err on EOF/error.
    fn fill(&mut self) -> Result<(), i32> {
        let mut b = [0u8; 4096];
        let n = sys_read(self.fd, &mut b)?;
        if n == 0 { return Err(EIO); }
        self.buf.extend_from_slice(&b[..n]);
        Ok(())
    }
}
const EIO: i32 = 5;

// ── uinput ─────────────────────────────────────────────────────────────────
const UI_DEV_CREATE: u32 = 0x5501;
const UI_DEV_DESTROY: u32 = 0x5502;
const UI_DEV_SETUP: u32 = 0x405c_5503;
const UI_ABS_SETUP: u32 = 0x401c_5504;
const UI_SET_EVBIT: u32 = 0x4004_5564;
const UI_SET_KEYBIT: u32 = 0x4004_5565;
const UI_SET_RELBIT: u32 = 0x4004_5566;
const UI_SET_ABSBIT: u32 = 0x4004_5567;
const BUS_BLUETOOTH: u16 = 0x05;

fn uinput_create(role: Role, name: &str, vid: u16, pid: u16) -> Result<i32, i32> {
    let fd = sys_open(b"/dev/uinput\0", O_RDWR | O_CLOEXEC)?;
    let io = |cmd: u32, arg: usize| sys_ioctl(fd, cmd, arg);
    let r = (|| -> Result<(), i32> {
        match role {
            Role::Gamepad => {
                io(UI_SET_EVBIT, 1)?; io(UI_SET_EVBIT, 3)?;
                for k in 0x130..=0x13e { io(UI_SET_KEYBIT, k)?; }
                for k in 0x2c0..=0x2c3 { io(UI_SET_KEYBIT, k)?; }
                for (code, min, max, fuzz, flat) in [(0u16, -32768, 32767, 16, 128), (1, -32768, 32767, 16, 128),
                        (3, -32768, 32767, 16, 128), (4, -32768, 32767, 16, 128), (2, 0, 255, 0, 0), (5, 0, 255, 0, 0),
                        (0x10, -1, 1, 0, 0), (0x11, -1, 1, 0, 0)] {
                    io(UI_SET_ABSBIT, code as usize)?;
                    let mut s = [0u8; 28];
                    s[0..2].copy_from_slice(&code.to_le_bytes());
                    for (i, v) in [0i32, min, max, fuzz, flat, 0].iter().enumerate() {
                        s[4 + 4 * i..8 + 4 * i].copy_from_slice(&v.to_le_bytes());
                    }
                    io(UI_ABS_SETUP, s.as_ptr() as usize)?;
                }
            }
            Role::Keyboard => {
                io(UI_SET_EVBIT, 1)?;
                for k in 1..=255 { io(UI_SET_KEYBIT, k)?; }
            }
            Role::Mouse => {
                io(UI_SET_EVBIT, 1)?; io(UI_SET_EVBIT, 2)?;
                for k in 0x110..=0x117 { io(UI_SET_KEYBIT, k)?; }
                for r in [0, 1, 6, 8] { io(UI_SET_RELBIT, r)?; }
            }
        }
        let mut s = [0u8; 92];
        for (i, v) in [BUS_BLUETOOTH, vid, pid, 1].iter().enumerate() { s[2 * i..2 * i + 2].copy_from_slice(&v.to_le_bytes()); }
        let nb = name.as_bytes();
        let n = nb.len().min(79);
        s[8..8 + n].copy_from_slice(&nb[..n]);
        io(UI_DEV_SETUP, s.as_ptr() as usize)?;
        io(UI_DEV_CREATE, 0)?;
        Ok(())
    })();
    match r { Ok(()) => Ok(fd), Err(e) => { sys_close(fd); Err(e) } }
}

fn uinput_write(fd: i32, evs: &[Ev]) {
    let mut b = Vec::with_capacity(evs.len() * 24);
    for e in evs {
        b.extend_from_slice(&[0u8; 16]);
        b.extend_from_slice(&e.ty.to_le_bytes());
        b.extend_from_slice(&e.code.to_le_bytes());
        b.extend_from_slice(&e.value.to_le_bytes());
    }
    let _ = sys_write(fd, &b);
}

fn uinput_destroy(fd: i32) {
    let _ = sys_ioctl(fd, UI_DEV_DESTROY, 0);
    sys_close(fd);
}

// ── addresses and files ────────────────────────────────────────────────────
/// "AA:BB:CC:DD:EE:FF" -> HCI order (LSB first).
fn parse_addr(s: &[u8]) -> Option<[u8; 6]> {
    let s = s.trim_ascii();
    if s.len() != 17 { return None; }
    let mut a = [0u8; 6];
    for i in 0..6 {
        let h = core::str::from_utf8(&s[3 * i..3 * i + 2]).ok()?;
        a[5 - i] = u8::from_str_radix(h, 16).ok()?;
        if i < 5 && s[3 * i + 2] != b':' { return None; }
    }
    Some(a)
}

struct A(pub [u8; 6]);
impl core::fmt::Display for A {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        let a = self.0;
        write!(f, "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}", a[5], a[4], a[3], a[2], a[1], a[0])
    }
}

fn read_file(path: &[u8]) -> Option<Vec<u8>> {
    let fd = sys_open(path, O_RDONLY | O_CLOEXEC).ok()?;
    let mut v = Vec::new();
    let mut b = [0u8; 1024];
    loop {
        match sys_read(fd, &mut b) { Ok(0) | Err(_) => break, Ok(n) => v.extend_from_slice(&b[..n]) }
    }
    sys_close(fd);
    Some(v)
}

fn write_file(path: &[u8], data: &[u8], flags: i32) -> bool {
    let Ok(fd) = sys_open(path, O_WRONLY | O_CREAT | O_CLOEXEC | flags) else { return false };
    let ok = sys_write(fd, data).map(|n| n == data.len()).unwrap_or(false);
    let _ = sys_fsync(fd);
    sys_close(fd);
    ok
}

fn listed() -> Vec<[u8; 6]> {
    let mut v = Vec::new();
    if let Some(t) = read_file(LIST) {
        for l in t.split(|&c| c == b'\n') {
            if let Some(a) = parse_addr(l.split(|&c| c == b' ').next().unwrap_or(b"")) {
                if !v.contains(&a) { v.push(a); }
            }
        }
    }
    v
}

fn cache_path(a: &[u8; 6]) -> Vec<u8> {
    let mut p = Vec::from(&b"/var/lib/bluetooth/hid-"[..]);
    for i in (0..6).rev() { p.extend_from_slice(alloc::format!("{:02X}", a[i]).as_bytes()); }
    p.push(0);
    p
}

/// Cache: b"BTHID1" vid:u16 pid:u16 descriptor…
fn load_cache(a: &[u8; 6]) -> Option<(u16, u16, Vec<u8>)> {
    let d = read_file(&cache_path(a))?;
    if d.len() < 10 || &d[..6] != b"BTHID1" { return None; }
    Some((u16::from_le_bytes([d[6], d[7]]), u16::from_le_bytes([d[8], d[9]]), d[10..].to_vec()))
}

fn save_cache(a: &[u8; 6], vid: u16, pid: u16, desc: &[u8]) {
    let mut d = Vec::from(&b"BTHID1"[..]);
    d.extend_from_slice(&vid.to_le_bytes()); d.extend_from_slice(&pid.to_le_bytes()); d.extend_from_slice(desc);
    write_file(&cache_path(a), &d, O_TRUNC);
}

// ── per-device state ───────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sdp { Idle, Pnp, Hid }

struct Dev {
    addr: [u8; 6],
    ctrl: Option<u16>,
    intr: Option<u16>,
    sdp: Option<u16>,
    stage: Sdp,
    acc: Vec<u8>,
    vid: u16,
    pid: u16,
    desc: Option<Vec<u8>>,
    hid: Option<HidDevice>,
    nodes: [i32; 3],
    /// We started this connection (and connect 0x11/0x13 after SDP).
    outgoing: bool,
    busy: bool,
}

impl Dev {
    fn new(addr: [u8; 6]) -> Dev {
        let (vid, pid, desc) = match load_cache(&addr) { Some((v, p, d)) => (v, p, Some(d)), None => (0, 0, None) };
        Dev { addr, ctrl: None, intr: None, sdp: None, stage: Sdp::Idle, acc: Vec::new(), vid, pid, desc,
              hid: None, nodes: [-1; 3], outgoing: false, busy: false }
    }
}

struct Host {
    bt: Btd,
    devs: Vec<Dev>,
    names: Vec<([u8; 6], String)>,
    next_req: u16,
    /// Outstanding CONNECT requests: req -> (addr, psm).
    reqs: Vec<(u16, [u8; 6], u16)>,
    tid: u16,
}

impl Host {
    fn dev(&mut self, a: &[u8; 6]) -> usize {
        if let Some(i) = self.devs.iter().position(|d| &d.addr == a) { return i; }
        self.devs.push(Dev::new(*a));
        self.devs.len() - 1
    }
    fn by_cid(&self, cid: u16) -> Option<usize> {
        self.devs.iter().position(|d| d.ctrl == Some(cid) || d.intr == Some(cid) || d.sdp == Some(cid))
    }
    fn connect(&mut self, a: [u8; 6], psm: u16) {
        self.next_req = self.next_req.wrapping_add(1).max(1);
        let req = self.next_req;
        self.reqs.push((req, a, psm));
        let i = self.dev(&a);
        self.devs[i].busy = true;
        if self.bt.connect(req, &a, psm).is_err() { eprintln!("bthid: btd write failed"); }
    }
    fn name(&self, a: &[u8; 6]) -> String {
        self.names.iter().find(|n| &n.0 == a).map(|n| n.1.clone())
            .unwrap_or_else(|| alloc::format!("Bluetooth HID {}", A(*a)))
    }

    /// Outgoing connect: SDP first unless the descriptor is cached.
    fn start(&mut self, a: [u8; 6]) {
        let i = self.dev(&a);
        let d = &mut self.devs[i];
        if d.busy || d.ctrl.is_some() || d.intr.is_some() { return; }
        d.outgoing = true;
        let cached = d.desc.is_some();
        sayln!("bthid: connecting {}{}", A(a), if cached { "" } else { " (SDP first)" });
        self.connect(a, if cached { report::HID_CONTROL_PSM } else { report::SDP_PSM });
    }

    fn sdp_send(&mut self, i: usize, cont: &[u8]) {
        let Some(cid) = self.devs[i].sdp else { return };
        self.tid = self.tid.wrapping_add(1);
        let uuid = if self.devs[i].stage == Sdp::Pnp { 0x1200 } else { 0x1124 };
        let pdu = report::sdp_request(self.tid, uuid, cont);
        let _ = self.bt.send(cid, &pdu);
    }

    fn sdp_recv(&mut self, i: usize, data: &[u8]) {
        let mut acc = core::mem::take(&mut self.devs[i].acc);
        let cont = report::sdp_response(data, &mut acc);
        self.devs[i].acc = acc;
        match cont {
            Some(c) if !c.is_empty() => { self.sdp_send(i, &c); return; }
            Some(_) => {}
            None => { eprintln!("bthid: {}: bad SDP response", A(self.devs[i].addr)); }
        }
        let acc = core::mem::take(&mut self.devs[i].acc);
        match self.devs[i].stage {
            Sdp::Pnp => {
                if let Some((v, p)) = report::sdp_pnp_ids(&acc) { self.devs[i].vid = v; self.devs[i].pid = p; }
                self.devs[i].stage = Sdp::Hid;
                self.sdp_send(i, &[]);
            }
            Sdp::Hid | Sdp::Idle => {
                let d = &mut self.devs[i];
                d.stage = Sdp::Idle;
                if let Some(desc) = report::sdp_hid_descriptor(&acc) {
                    sayln!("bthid: {}: {:04x}:{:04x}, {}-byte report descriptor", A(d.addr), d.vid, d.pid, desc.len());
                    save_cache(&d.addr, d.vid, d.pid, &desc);
                    d.desc = Some(desc);
                } else if d.desc.is_none() {
                    eprintln!("bthid: {}: no HID descriptor in SDP; quirk-only", A(d.addr));
                    d.desc = Some(Vec::new());
                    save_cache(&d.addr, d.vid, d.pid, &[]);
                }
                if let Some(c) = d.sdp.take() { self.bt.close(c); }
                let a = self.devs[i].addr;
                if self.devs[i].ctrl.is_none() && self.devs[i].outgoing {
                    self.connect(a, report::HID_CONTROL_PSM);
                } else {
                    self.activate(i);
                }
            }
        }
    }

    fn activate(&mut self, i: usize) {
        let (ready, has_desc) = { let d = &self.devs[i]; (d.ctrl.is_some() && d.intr.is_some() && d.hid.is_none(), d.desc.is_some()) };
        if !ready { return; }
        if !has_desc {
            if self.devs[i].sdp.is_none() && !self.devs[i].busy {
                let a = self.devs[i].addr;
                self.devs[i].stage = Sdp::Pnp;
                self.connect(a, report::SDP_PSM);
            }
            return;
        }
        let name = self.name(&self.devs[i].addr);
        let d = &mut self.devs[i];
        let hid = HidDevice::new(d.vid, d.pid, d.desc.as_deref().unwrap_or(&[]));
        let mut kinds = String::new();
        for r in report::ROLES {
            if !hid.has(r) { continue; }
            let (suffix, k) = match r { Role::Gamepad => ("", "gamepad"), Role::Keyboard => (" Keyboard", "keyboard"), Role::Mouse => (" Mouse", "mouse") };
            let nm = alloc::format!("{}{}", name, suffix);
            match uinput_create(r, &nm, d.vid, d.pid) {
                Ok(fd) => { d.nodes[r as usize] = fd; kinds.push_str(k); kinds.push(' '); }
                Err(e) => eprintln!("bthid: {}: uinput {}: {}", A(d.addr), k, errname(e)),
            }
        }
        sayln!("bthid: {} \"{}\" connected: {}({:?})", A(d.addr), name, kinds, hid.quirk);
        let q = hid.quirk;
        d.hid = Some(hid);
        if let (Some(c), Some(init)) = (d.ctrl, report::init_control(q)) { let _ = self.bt.send(c, &init); }
        // Remember it so it is connected again at the next start.
        if !listed().contains(&self.devs[i].addr) {
            write_file(LIST, alloc::format!("{}\n", A(self.devs[i].addr)).as_bytes(), O_APPEND);
        }
    }

    fn teardown(&mut self, i: usize, why: &str) {
        let d = &mut self.devs[i];
        if let Some(h) = d.hid.as_mut() {
            let evs = h.release_all();
            for r in report::ROLES {
                let fd = d.nodes[r as usize];
                if fd >= 0 {
                    let mine: Vec<Ev> = evs.iter().copied().filter(|e| e.role == r).collect();
                    uinput_write(fd, &mine);
                    uinput_destroy(fd);
                }
            }
            sayln!("bthid: {} disconnected ({})", A(d.addr), why);
        }
        d.nodes = [-1; 3];
        d.hid = None;
        for c in [d.ctrl.take(), d.intr.take(), d.sdp.take()].into_iter().flatten() { self.bt.close(c); }
        let d = &mut self.devs[i];
        d.stage = Sdp::Idle; d.outgoing = false; d.busy = false; d.acc.clear();
    }

    fn event(&mut self, op: u8, p: &[u8]) {
        let u16at = |o: usize| p.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0);
        match op {
            EV_CONNECT_RESULT if p.len() >= 5 => {
                let (req, status, cid) = (u16at(0), p[2], u16at(3));
                let Some(k) = self.reqs.iter().position(|r| r.0 == req) else { return };
                let (_, a, psm) = self.reqs.remove(k);
                let i = self.dev(&a);
                self.devs[i].busy = false;
                if status != 0 {
                    eprintln!("bthid: {}: connect PSM 0x{:02x} failed (status {})", A(a), psm, status);
                    self.teardown(i, "connect failed");
                    return;
                }
                match psm {
                    report::SDP_PSM => {
                        self.devs[i].sdp = Some(cid);
                        if self.devs[i].stage == Sdp::Idle { self.devs[i].stage = Sdp::Pnp; }
                        self.sdp_send(i, &[]);
                    }
                    report::HID_CONTROL_PSM => {
                        self.devs[i].ctrl = Some(cid);
                        let _ = self.bt.frame(OP_DEVICES, &[]);
                        self.connect(a, report::HID_INTERRUPT_PSM);
                    }
                    _ => { self.devs[i].intr = Some(cid); self.activate(i); }
                }
            }
            EV_INCOMING if p.len() >= 10 => {
                let cid = u16at(0);
                let mut a = [0u8; 6];
                a.copy_from_slice(&p[2..8]);
                let psm = u16at(8);
                let i = self.dev(&a);
                if psm == report::HID_CONTROL_PSM {
                    if self.devs[i].hid.is_some() { self.teardown(i, "reconnected"); }
                    self.devs[i].ctrl = Some(cid);
                    let _ = self.bt.frame(OP_DEVICES, &[]);
                    sayln!("bthid: {} reconnecting", A(a));
                } else if psm == report::HID_INTERRUPT_PSM {
                    self.devs[i].intr = Some(cid);
                    self.activate(i);
                } else { self.bt.close(cid); }
            }
            EV_RECV if p.len() >= 2 => {
                let cid = u16at(0);
                let data = &p[2..];
                let Some(i) = self.by_cid(cid) else { return };
                if self.devs[i].sdp == Some(cid) { self.sdp_recv(i, data); return; }
                if self.devs[i].intr == Some(cid) || self.devs[i].ctrl == Some(cid) {
                    // Reports arrive on the interrupt channel; a GET_REPORT
                    // answer (DATA|Feature) on control is not input.
                    let d = &mut self.devs[i];
                    let (Some(h), Some(r)) = (d.hid.as_mut(), report::hidp_input(data)) else { return };
                    let evs = h.input(r);
                    for role in report::ROLES {
                        let fd = d.nodes[role as usize];
                        if fd < 0 { continue; }
                        let mine: Vec<Ev> = evs.iter().copied().filter(|e| e.role == role).collect();
                        if !mine.is_empty() { uinput_write(fd, &mine); }
                    }
                    if data.first() == Some(&report::HIDP_VC_UNPLUG) { self.teardown(i, "virtual cable unplug"); }
                }
            }
            EV_CLOSED if p.len() >= 2 => {
                let cid = u16at(0);
                let Some(i) = self.by_cid(cid) else { return };
                if self.devs[i].sdp == Some(cid) {
                    self.devs[i].sdp = None;
                    if self.devs[i].stage != Sdp::Idle {
                        self.devs[i].stage = Sdp::Idle;
                        if self.devs[i].desc.is_none() { self.devs[i].desc = Some(Vec::new()); }
                        let a = self.devs[i].addr;
                        if self.devs[i].ctrl.is_none() && self.devs[i].outgoing { self.connect(a, report::HID_CONTROL_PSM); }
                        else { self.activate(i); }
                    }
                    return;
                }
                self.teardown(i, "link closed");
            }
            EV_DEVICE if p.len() >= 12 => {
                let mut a = [0u8; 6];
                a.copy_from_slice(&p[0..6]);
                let name = String::from_utf8_lossy(&p[12..]).into_owned();
                let name = String::from(name.trim_end_matches('\0'));
                if name.is_empty() { return; }
                match self.names.iter_mut().find(|n| n.0 == a) { Some(n) => n.1 = name, None => self.names.push((a, name)) }
            }
            EV_RESULT if p.len() >= 2 => {
                if p[0] == OP_LISTEN && p[1] != 0 { eprintln!("bthid: LISTEN refused (status {}): another HID host running?", p[1]); }
            }
            _ => {}
        }
    }
}

fn daemon() -> i32 {
    let mut bt = loop {
        match Btd::open() {
            Ok(b) => break b,
            Err(_) => sleep_ms(2000), // btd not up yet
        }
    };
    for psm in [report::HID_CONTROL_PSM, report::HID_INTERRUPT_PSM] {
        let _ = bt.frame(OP_LISTEN, &psm.to_le_bytes());
    }
    let _ = bt.frame(OP_DEVICES, &[]);
    sayln!("bthid: listening for HID devices");
    let mut h = Host { bt, devs: Vec::new(), names: Vec::new(), next_req: 0, reqs: Vec::new(), tid: 0 };
    for a in listed() { h.start(a); }
    loop {
        match poll1(h.bt.fd, 2000) {
            Ok(r) if r != 0 => {
                if h.bt.fill().is_err() {
                    eprintln!("bthid: btd went away; restarting");
                    for i in 0..h.devs.len() { h.teardown(i, "btd gone"); }
                    sys_close(h.bt.fd);
                    return daemon();
                }
                while let Some((op, p)) = h.bt.next() { h.event(op, &p); }
            }
            Ok(_) => {}
            Err(_) => sleep_ms(100),
        }
        // `bthid connect ADDR` requests.
        if let Some(t) = read_file(REQ) {
            if !t.is_empty() {
                write_file(REQ, b"", O_TRUNC);
                for l in t.split(|&c| c == b'\n') { if let Some(a) = parse_addr(l) { h.start(a); } }
            }
        }
    }
}

fn cli(args: &[&[u8]]) -> i32 {
    match args {
        [] | [b"daemon"] => daemon(),
        [b"connect", a] => {
            let Some(addr) = parse_addr(a) else { eprintln!("bthid: bad address"); return 2 };
            if !listed().contains(&addr) { write_file(LIST, alloc::format!("{}\n", A(addr)).as_bytes(), O_APPEND); }
            if !write_file(REQ, alloc::format!("{}\n", A(addr)).as_bytes(), O_APPEND) {
                eprintln!("bthid: cannot write the request (root needed?)"); return 1;
            }
            sayln!("bthid: {} queued; the daemon connects within 2 s (watch its log)", A(addr));
            0
        }
        [b"forget", a] => {
            let Some(addr) = parse_addr(a) else { eprintln!("bthid: bad address"); return 2 };
            let keep: Vec<[u8; 6]> = listed().into_iter().filter(|x| *x != addr).collect();
            let mut t = String::new();
            for k in keep { t.push_str(&alloc::format!("{}\n", A(k))); }
            write_file(LIST, t.as_bytes(), O_TRUNC);
            0
        }
        [b"list"] => {
            for a in listed() {
                let info = load_cache(&a).map(|(v, p, d)| alloc::format!("{:04x}:{:04x} desc {} B", v, p, d.len()))
                    .unwrap_or_else(|| String::from("not yet queried"));
                sayln!("{} {}", A(a), info);
            }
            0
        }
        _ => { eprintln!("usage: bthid [daemon] | connect ADDR | forget ADDR | list"); 2 }
    }
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let mut args: Vec<&[u8]> = Vec::new();
    for i in 1..argc as usize {
        let p = *argv.add(i);
        let mut n = 0;
        while *p.add(n) != 0 { n += 1; }
        args.push(core::slice::from_raw_parts(p, n));
    }
    let _ = (syscall4 as usize, Role::Gamepad);
    cli(&args)
}
