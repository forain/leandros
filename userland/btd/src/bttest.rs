//! bttest — in-guest end-to-end test of btd + btctl's client library.
//!
//! Runs the real /bin/btd with `--fd` on one end of a socketpair; a forked
//! child plays a BCM4345C0-like controller (and a remote gamepad behind it)
//! on the other end. Then, through the client API: adapter ready, scan finds
//! the pad, SSP pairing stores the link key on disk, the device list shows it
//! paired + connected, remove forgets it. Also: no controller => btd exits 1.
//! Output: "<case>: PASS|FAIL [detail]", then "--- bttest done: N failure(s)".
#![no_std]
#![no_main]

extern crate alloc;
#[macro_use]
extern crate btd;

use alloc::vec::Vec;
use btd::bt_host::api::op as aop;
use btd::bt_host::hci::{self, ev, op};
use btd::client::Bt;
use btd::sys;
use btd::{Addr, Msg};
use leandros_libc::syscall::{nr, syscall3};

static mut FAILS: u32 = 0;
fn check(name: &str, ok: bool, detail: &str) {
    if ok { sayln!("{}: PASS", name); } else { sayln!("{}: FAIL {}", name, detail); unsafe { FAILS += 1; } }
}

const SOCK: &[u8] = b"/tmp/bttest.sock\0";
const KEYS: &[u8] = b"/tmp/bttest.keys\0";
const PAD: [u8; 6] = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];

fn cc(o: u16, ret: &[u8]) -> Vec<u8> {
    let mut p = alloc::vec![1u8];
    p.extend_from_slice(&o.to_le_bytes());
    p.extend_from_slice(ret);
    hci::event(ev::CMD_COMPLETE, &p)
}
fn cs(o: u16) -> Vec<u8> { let b = o.to_le_bytes(); hci::event(ev::CMD_STATUS, &[0, 1, b[0], b[1]]) }
fn with_pad(code: u8, pre: &[u8], post: &[u8]) -> Vec<u8> {
    let mut p = pre.to_vec();
    p.extend_from_slice(&PAD);
    p.extend_from_slice(post);
    hci::event(code, &p)
}

/// The scripted controller: answers one H4 packet.
fn respond(pkt: &[u8]) -> Vec<Vec<u8>> {
    if pkt[0] == hci::H4_ACL { return respond_acl(pkt); }
    if pkt[0] != hci::H4_CMD { return Vec::new(); }
    let o = u16::from_le_bytes([pkt[1], pkt[2]]);
    let h = [0x0b, 0x00];
    match o {
        op::READ_LOCAL_VERSION => alloc::vec![cc(o, &[0, 9, 0, 0, 9, 0x0f, 0, 0x19, 0x61])],
        op::READ_LOCAL_FEATURES => alloc::vec![cc(o, &[0, 0xbf, 0xfe, 0xcf, 0xfe, 0xdb, 0xff, 0x7b, 0x87])],
        op::READ_BD_ADDR => alloc::vec![cc(o, &[0, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11])],
        op::READ_BUFFER_SIZE => alloc::vec![cc(o, &[0, 0xfd, 0x03, 0x40, 8, 0, 0, 0])],
        op::LE_READ_BUFFER_SIZE => alloc::vec![cc(o, &[0, 0, 0, 0])],
        op::INQUIRY => {
            let mut e = alloc::vec![1u8];
            e.extend_from_slice(&PAD);
            e.extend_from_slice(&[1, 0, 0x08, 0x05, 0x00, 0x34, 0x12, 0xc4, 18, 0x09]);
            e.extend_from_slice(b"LeandrOS Test Pad");
            e.resize(255, 0);
            alloc::vec![cs(o), hci::event(ev::EXT_INQUIRY_RESULT, &e), hci::event(ev::INQUIRY_COMPLETE, &[0])]
        }
        op::CREATE_CONN => alloc::vec![cs(o), with_pad(ev::CONN_COMPLETE, &[0, h[0], h[1]], &[1, 0])],
        op::AUTH_REQUESTED => alloc::vec![cs(o), with_pad(ev::LINK_KEY_REQUEST, &[], &[])],
        op::LINK_KEY_NEG_REPLY => alloc::vec![cc(o, &[0]), with_pad(ev::IO_CAP_REQUEST, &[], &[])],
        op::IO_CAP_REPLY => alloc::vec![cc(o, &[0]), with_pad(ev::IO_CAP_RESPONSE, &[], &[3, 0, 4]),
                                        with_pad(ev::USER_CONFIRM_REQUEST, &[], &[0x40, 0xe2, 1, 0])],
        op::USER_CONFIRM_REPLY => {
            let mut k = [0xa5u8; 17];
            k[16] = 4;
            alloc::vec![cc(o, &[0]), with_pad(ev::SSP_COMPLETE, &[0], &[]), with_pad(ev::LINK_KEY_NOTIFY, &[], &k),
                        hci::event(ev::AUTH_COMPLETE, &[0, h[0], h[1]])]
        }
        op::SET_CONN_ENCRYPT => alloc::vec![cs(o), hci::event(ev::ENCRYPT_CHANGE, &[0, h[0], h[1], 1])],
        op::DISCONNECT => alloc::vec![cs(o), hci::event(ev::DISCONN_COMPLETE, &[0, h[0], h[1], 0x16])],
        op::REMOTE_NAME_REQ | op::ACCEPT_CONN => alloc::vec![cs(o)],
        _ => alloc::vec![cc(o, &[0])],
    }
}

/// The remote's L2CAP signalling (BR/EDR, cid 1): accepts any channel (as
/// dcid 0x0040+), sends its own config request (MTU 672), answers config and
/// disconnect requests; data on other cids is dropped. Every host ACL packet
/// is acknowledged with Number_Of_Completed_Packets so credits never run out.
static mut HOST_CID: [u8; 2] = [0, 0];
fn respond_acl(pkt: &[u8]) -> Vec<Vec<u8>> {
    let h = [0x0b, 0x00];
    let mut out = alloc::vec![hci::event(ev::NUM_COMPLETED, &[1, h[0], h[1], 1, 0])];
    if pkt.len() < 13 || u16::from_le_bytes([pkt[7], pkt[8]]) != 1 { return out; }
    let (code, id) = (pkt[9], pkt[10]);
    let d = &pkt[13..];
    let sig = |code: u8, id: u8, body: &[u8]| {
        let mut l2 = Vec::new();
        l2.extend_from_slice(&((4 + body.len()) as u16).to_le_bytes());
        l2.extend_from_slice(&[1, 0, code, id]);
        l2.extend_from_slice(&(body.len() as u16).to_le_bytes());
        l2.extend_from_slice(body);
        hci::acl(0x000b, 0b10, &l2)
    };
    match code {
        0x02 if d.len() >= 4 => {
            let scid = [d[2], d[3]];
            unsafe { HOST_CID = scid; }
            out.push(sig(0x03, id, &[0x40, 0x00, scid[0], scid[1], 0, 0, 0, 0]));
            out.push(sig(0x04, 0x77, &[scid[0], scid[1], 0, 0, 0x01, 0x02, 0xa0, 0x02]));
        }
        // Config request from the host: answer success for its (our dcid) channel.
        0x04 if d.len() >= 2 => { let c = unsafe { HOST_CID }; out.push(sig(0x05, id, &[c[0], c[1], 0, 0, 0, 0])) }
        0x06 if d.len() >= 4 => out.push(sig(0x07, id, &d[..4])),
        0x0a if d.len() >= 2 => out.push(sig(0x0b, id, &[d[0], d[1], 1, 0])),
        0x08 => out.push(sig(0x09, id, d)),
        _ => {}
    }
    out
}

fn controller(fd: i32) -> ! {
    let mut rx = Vec::new();
    let mut b = [0u8; 2048];
    loop {
        match sys::read(fd, &mut b) {
            Ok(0) | Err(_) => leandros_libc::exit(0),
            Ok(n) => rx.extend_from_slice(&b[..n]),
        }
        while let Some(Ok(n)) = hci::h4_len(&rx) {
            if rx.len() < n { break; }
            let p: Vec<u8> = rx.drain(..n).collect();
            for r in respond(&p) { let _ = sys::write(fd, &r); }
        }
    }
}

unsafe fn spawn_btd(args: &[&[u8]]) -> i32 {
    let mut argv: Vec<*const u8> = args.iter().map(|a| a.as_ptr()).collect();
    argv.push(core::ptr::null());
    let envp: [*const u8; 1] = [core::ptr::null()];
    let pid = leandros_libc::fork();
    if pid == 0 {
        leandros_libc::execve(b"/bin/btd\0".as_ptr(), argv.as_ptr(), envp.as_ptr());
        leandros_libc::exit(127);
    }
    pid
}

fn read_file(path: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    if let Ok(fd) = sys::open(path, sys::O_RDONLY, 0) {
        let mut b = [0u8; 512];
        while let Ok(n) = sys::read(fd, &mut b) { if n == 0 { break; } v.extend_from_slice(&b[..n]); }
        sys::close(fd);
    }
    v
}
fn contains(h: &[u8], n: &[u8]) -> bool { h.windows(n.len()).any(|w| w == n) }

unsafe fn wait_exit(pid: i32) -> i32 {
    let mut st = 0i32;
    leandros_libc::wait4(pid, &mut st, 0, core::ptr::null_mut());
    (st >> 8) & 0xff
}

unsafe fn arg(argv: *const *const u8, i: usize) -> Vec<u8> {
    let p = *argv.add(i);
    let mut n = 0;
    while *p.add(n) != 0 { n += 1; }
    let mut v = core::slice::from_raw_parts(p, n).to_vec();
    v.push(0);
    v
}

/// `bttest serve SOCK KEYS`: run btd on the scripted controller (with the
/// test pad behind it) at SOCK until stdin reaches EOF, then stop both. For
/// clients that test against a known adapter (the org.bluez shim's test).
unsafe fn serve(sock: &[u8], keys: &[u8]) -> i32 {
    sys::unlink(keys);
    let Ok(sv) = sys::socketpair() else { return 1 };
    let ctl = leandros_libc::fork();
    if ctl == 0 { sys::close(sv[1]); controller(sv[0]); }
    sys::close(sv[0]);
    leandros_libc::dup3(sv[1], 9, 0);
    let btd_pid = spawn_btd(&[b"btd\0", b"--fd\0", b"9\0", b"--sock\0", sock, b"--keys\0", keys, b"--name\0", b"bttest\0"]);
    sys::close(sv[1]);
    sys::close(9);
    sayln!("bttest serve: btd pid {} on the scripted controller", btd_pid);
    let mut b = [0u8; 64];
    while let Ok(n) = sys::read(0, &mut b) { if n == 0 { break; } }
    syscall3(nr::KILL, btd_pid as usize, 15, 0);
    wait_exit(btd_pid);
    syscall3(nr::KILL, ctl as usize, 15, 0);
    wait_exit(ctl);
    sys::unlink(sock);
    0
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    if argc == 4 && arg(argv, 1) == b"serve\0" {
        return serve(&arg(argv, 2), &arg(argv, 3));
    }
    // Case 0: no controller.
    let pid = spawn_btd(&[b"btd\0", b"--dev\0", b"/dev/hci-none\0", b"--sock\0", SOCK, b"--keys\0", KEYS]);
    check("no_controller_exits_1", wait_exit(pid) == 1, "btd without a controller must exit 1");

    sys::unlink(KEYS);
    let Ok(sv) = sys::socketpair() else { check("socketpair", false, ""); return 1; };
    let ctl = leandros_libc::fork();
    if ctl == 0 { sys::close(sv[1]); controller(sv[0]); }
    sys::close(sv[0]);
    leandros_libc::dup3(sv[1], 9, 0);
    let btd_pid = spawn_btd(&[b"btd\0", b"--fd\0", b"9\0", b"--sock\0", SOCK, b"--keys\0", KEYS, b"--name\0", b"bttest\0"]);
    sys::close(sv[1]);
    sys::close(9);

    let t0 = sys::now_ms();
    let mut bt = None;
    while sys::now_ms() - t0 < 20_000 {
        if let Ok(mut b) = Bt::open_at(SOCK) {
            if let Ok(Msg::InfoRsp { state: 1, .. }) = b.info() { bt = Some(b); break; }
        }
        leandros_libc::usleep(50_000);
    }
    let ready = bt.is_some();
    check("btd_ready", ready, "no ready INFO within 20 s");
    if let Some(mut bt) = bt {
        if let Ok(Msg::InfoRsp { addr, lmp_subver, name, .. }) = bt.info() {
            check("info_fields", addr == Addr::parse("11:22:33:44:55:66").unwrap() && lmp_subver == 0x6119 && name == "bttest", "");
        }
        let pad = Addr(PAD);
        let mut seen = false;
        let r = bt.devices(Msg::Scan { secs: 1 }, aop::SCAN, |d| seen |= d.addr == pad && d.name == "LeandrOS Test Pad");
        check("scan_finds_pad", r.is_ok() && seen, "");
        let r = bt.pair(pad);
        let file = read_file(KEYS);
        check("pair_ssp_stores_key", r.is_ok() && contains(&file, b"06:05:04:03:02:01 a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5 4 LeandrOS Test Pad"), "");
        let mut flags = 0;
        let r = bt.devices(Msg::Devices, aop::DEVICES, |d| if d.addr == pad { flags = d.flags });
        check("devices_paired_connected", r.is_ok() && flags & 3 == 3, "");
        let r = bt.connect(pad, 0x0102);
        check("bad_psm_rejected", r == Err(btd::client::Error::Status(btd::status::BAD_REQUEST)), "");
        let r = bt.remove(pad);
        check("remove_forgets_key", r.is_ok() && !contains(&read_file(KEYS), b"06:05:04:03:02:01"), "");
    }
    syscall3(nr::KILL, btd_pid as usize, 15, 0);
    wait_exit(btd_pid);
    syscall3(nr::KILL, ctl as usize, 15, 0);
    wait_exit(ctl);
    sys::unlink(SOCK);
    sys::unlink(KEYS);
    let f = FAILS;
    sayln!("--- bttest done: {} failure(s)", f);
    if f == 0 { 0 } else { 1 }
}
