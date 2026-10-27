//! btctl — Bluetooth CLI (bluetoothctl verbs), a client of btd.
//!
//! ```text
//! btctl show                      adapter address, version, state
//! btctl scan [SECS]               inquiry + LE scan (default 10 s), prints devices as found
//! btctl devices                   known devices      (alias: list)
//! btctl paired-devices            bonded devices only
//! btctl pair ADDR                 bond (SSP just-works / PIN 0000), stores the link key
//! btctl connect ADDR [PSM]        no PSM: bring the link up + authenticate (pair if needed);
//!                                 with PSM (e.g. 0x13 HID interrupt): open the channel and hex-dump
//!                                 what arrives until the peer closes it
//! btctl disconnect ADDR
//! btctl remove ADDR               forget the link key
//! ```
//! Needs root (btd.sock is 0600). Exit status: 0 ok, 1 error, 2 usage.
#![no_std]
#![no_main]

extern crate alloc;
#[macro_use]
extern crate btd;

use btd::client::{Bt, Error};
use btd::{status, Addr, DeviceInfo, Msg};

fn usage() -> i32 {
    eprintln!("usage: btctl show|scan [SECS]|devices|list|paired-devices|pair ADDR|connect ADDR [PSM]|disconnect ADDR|remove ADDR");
    2
}

fn fail(what: &str, e: Error) -> i32 {
    match e {
        Error::Io(2) | Error::Io(111) => eprintln!("btctl: {}: btd is not running", what),
        Error::Io(e) => eprintln!("btctl: {}: errno {}", what, e),
        Error::Status(s) => eprintln!("btctl: {}: {}", what, status::name(s)),
    }
    1
}

fn print_dev(prefix: &str, d: &DeviceInfo) {
    say!("{}Device {} {}", prefix, d.addr, if d.name.is_empty() { "(unknown)" } else { d.name.as_str() });
    if d.flags & 4 != 0 { say!(" [LE]"); }
    if d.flags & 1 != 0 { say!(" [paired]"); }
    if d.flags & 2 != 0 { say!(" [connected]"); }
    if d.class != 0 { say!(" class 0x{:06x}", d.class); }
    if d.rssi != 0 { say!(" rssi {}", d.rssi); }
    sayln!("");
}

unsafe fn arg(argv: *const *const u8, i: i32) -> &'static str {
    let p = *argv.offset(i as isize);
    let mut n = 0;
    while *p.add(n) != 0 { n += 1; }
    core::str::from_utf8(core::slice::from_raw_parts(p, n)).unwrap_or("")
}

fn parse_num(s: &str) -> Option<u16> {
    match s.strip_prefix("0x") { Some(h) => u16::from_str_radix(h, 16).ok(), None => s.parse().ok() }
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let cmd = if argc > 1 { arg(argv, 1) } else { "show" };
    let a2 = if argc > 2 { Some(arg(argv, 2)) } else { None };
    let a3 = if argc > 3 { Some(arg(argv, 3)) } else { None };
    let addr = a2.and_then(Addr::parse);
    let needs_addr = matches!(cmd, "pair" | "connect" | "disconnect" | "remove");
    if needs_addr && addr.is_none() { return usage(); }
    let mut bt = match Bt::open() { Ok(b) => b, Err(e) => return fail("connect to btd", e) };
    match cmd {
        "show" => match bt.info() {
            Ok(Msg::InfoRsp { addr, hci_ver, lmp_subver, manufacturer, state, name }) => {
                sayln!("Controller {} {}", addr, name);
                sayln!("\tHCI version: {}  LMP subversion: 0x{:04x}  Manufacturer: {}", hci_ver, lmp_subver, manufacturer);
                sayln!("\tState: {}", ["initialising", "ready", "dead"].get(state as usize).unwrap_or(&"?"));
                0
            }
            Ok(_) => 1,
            Err(e) => fail("show", e),
        },
        "scan" => {
            let secs = a2.and_then(parse_num).unwrap_or(10).clamp(1, 60) as u8;
            sayln!("Discovery started ({} s)", secs);
            match bt.devices(Msg::Scan { secs }, btd::bt_host::api::op::SCAN, |d| print_dev("[NEW] ", d)) {
                Ok(()) => { sayln!("Discovery stopped"); 0 }
                Err(e) => fail("scan", e),
            }
        }
        "devices" | "list" | "paired-devices" => {
            let only_paired = cmd == "paired-devices";
            match bt.devices(Msg::Devices, btd::bt_host::api::op::DEVICES, |d| if !only_paired || d.flags & 1 != 0 { print_dev("", d) }) {
                Ok(()) => 0,
                Err(e) => fail(cmd, e),
            }
        }
        "pair" => match bt.pair(addr.unwrap()) {
            Ok(()) => { sayln!("Pairing successful"); 0 }
            Err(e) => fail("pair", e),
        },
        "remove" => match bt.remove(addr.unwrap()) {
            Ok(()) => { sayln!("Device has been removed"); 0 }
            Err(e) => fail("remove", e),
        },
        "disconnect" => match bt.disconnect(addr.unwrap()) {
            Ok(()) => { sayln!("Successful disconnected"); 0 }
            Err(e) => fail("disconnect", e),
        },
        "connect" => {
            let a = addr.unwrap();
            let Some(psm) = a3.map(parse_num) else {
                return match bt.pair(a) {
                    Ok(()) => { sayln!("Connection successful"); 0 }
                    Err(e) => fail("connect", e),
                };
            };
            let Some(psm) = psm else { return usage() };
            let ch = match bt.connect(a, psm) { Ok(c) => c, Err(e) => return fail("connect", e) };
            sayln!("Channel psm 0x{:04x} open: cid 0x{:04x} peer mtu {}", psm, ch.cid, ch.peer_mtu);
            loop {
                match bt.next() {
                    Ok(Msg::Recv { cid, data }) if cid == ch.cid => {
                        for b in &data { say!("{:02x} ", b); }
                        sayln!("");
                    }
                    Ok(Msg::Closed { cid, reason }) if cid == ch.cid => { sayln!("Channel closed: {}", status::name(reason)); return 0; }
                    Ok(_) => {}
                    Err(e) => return fail("connect", e),
                }
            }
        }
        _ => usage(),
    }
}
