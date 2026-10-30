//! Bluetooth over USB (btusb) as a loadable module. The Intel set-up (`btintel`) and the H4
//! reassembly (`usb_reasm`) are the SAME source as the built-in (`drivers/src/bt`); the transport is
//! the kapi USB BT pipe and the node is the kernel's /dev/hci0 through `kapi_hci_*`. Bound by the
//! `usb * * e0 01 01` alias through the class registry. HCI core, the Pi UART transport, btd and
//! bthid stay in the kernel. Command line: `bt=0` skips the driver, `btusb.fw=0` skips the Intel
//! download. Firmware: /lib/firmware/intel/ibt-<variant>-<revid>.{sfi,ddc}.
#![no_std]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::format;
use alloc::vec::Vec;
use kmod_abi::KUsbIf;
use kmod_kapi::{hci, usb as kusb};

#[path = "../../../drivers/src/bt/btintel.rs"]
mod btintel;
#[path = "../../../drivers/src/bt/usb_reasm.rs"]
mod usb_reasm;

use usb_reasm::{Reasm, H4_ACL, H4_EVENT};

macro_rules! blog { ($($a:tt)*) => { kmod_rt::kinfo!("[btusb] {}", format!($($a)*)) } }

fn now_ms() -> u64 { kmod_kapi::sched::monotonic_ns() / 1_000_000 }
fn msleep(ms: u64) { kmod_kapi::sched::kthread::msleep(ms) }
fn yield_now() { kmod_kapi::sys_common::yield_now() }

fn opt<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    cmdline.split_whitespace().find_map(|w| w.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

const FW_DIR: &str = "/lib/firmware/intel/";

fn load_fw(name: &str) -> Option<Vec<u8>> {
    let t0 = now_ms();
    while !kmod_kapi::sys_common::fw_root_ready() {
        if now_ms() - t0 > 120_000 { blog!("root filesystem not mounted within 120 s"); return None; }
        msleep(250);
    }
    let (h, p, n) = kmod_kapi::sys_common::fw_request(&format!("{}{}", FW_DIR, name), 4 << 20)?;
    let v = unsafe { core::slice::from_raw_parts(p, n) }.to_vec();
    kmod_kapi::sys_common::fw_release(h);
    Some(v)
}

/// Receive side shared by the set-up link and the pump: raw chunks -> H4 packets.
struct Rx { pipe: u64, ev: Reasm, bev: Reasm, acl: Reasm, buf: [u8; 1024], gone: bool }

impl Rx {
    /// Poll once; completed H4 packets go to `out` as (is_acl, packet). false = nothing/busy.
    fn pull(&mut self, boot: bool, out: &mut Vec<Vec<u8>>) -> bool {
        match kusb::bt_poll(self.pipe, &mut self.buf) {
            Ok((n, fl)) => {
                if fl & kusb::BT_RESET_EV != 0 { self.ev.reset(); }
                if fl & kusb::BT_RESET_ACL != 0 { self.bev.reset(); self.acl.reset(); }
                if n == 0 { return false; }
                let c = &self.buf[..n.min(self.buf.len())];
                if fl & kusb::BT_BULK == 0 { self.ev.push(c, out) }
                else if boot { self.bev.push(c, out) }
                else { self.acl.push(c, out) }
                true
            }
            Err(-19) => { self.gone = true; false }
            Err(_) => false,
        }
    }
}

struct Link { rx: Rx, q: VecDeque<Vec<u8>>, boot: bool }

impl btintel::Link for Link {
    fn send_cmd(&mut self, pkt: &[u8], bulk: bool) -> bool {
        let r = kusb::bt_send(self.rx.pipe, bulk as u32, pkt);
        if let Err(e) = r { blog!("send {:02x}{:02x} failed: {}", pkt[1], pkt[0], e); }
        r.is_ok()
    }
    fn event(&mut self, timeout_ms: u32) -> Option<Vec<u8>> {
        let (t0, end) = (now_ms(), now_ms() + timeout_ms as u64);
        loop {
            if let Some(e) = self.q.pop_front() { return Some(e[1..].to_vec()); }
            if self.rx.gone { return None; }
            let mut out = Vec::new();
            let any = self.rx.pull(self.boot, &mut out);
            self.q.extend(out);
            if any { continue; }
            let now = now_ms();
            if now >= end { return None; }
            if now - t0 < 3 { yield_now() } else { msleep(1) }
        }
    }
    fn now_ms(&mut self) -> u64 { now_ms() }
    fn operational(&mut self) { self.boot = false; self.rx.bev.reset(); }
    fn bootloader(&mut self) { self.boot = true; self.rx.bev.reset(); self.rx.ev.reset(); }
    fn log(&mut self, msg: &str) { blog!("{}", msg) }
}

fn park() -> ! { loop { msleep(3_600_000); } }

fn run(pipe: u64, vid: u16, fw: bool) {
    let rx = Rx { pipe, ev: Reasm::new(H4_EVENT), bev: Reasm::new(H4_EVENT), acl: Reasm::new(H4_ACL), buf: [0; 1024], gone: false };
    let mut link = Link { rx, q: VecDeque::new(), boot: true };
    if vid == 0x8087 && fw {
        let t = now_ms();
        match btintel::setup(&mut link, &mut |n| load_fw(n)) {
            Ok(r) => blog!("intel set-up done in {} ms ({}{} DDC records)", now_ms() - t,
                           if r.already_operational { "firmware already running, " } else { "" }, r.ddc_records),
            Err(e) => { blog!("fail stage=intel {}", e.describe()); return park(); }
        }
    }
    let leftover = link.q.len();
    let mut rx = link.rx;
    rx.ev.reset(); rx.acl.reset();
    let h = match hci::register() {
        Ok(h) => h,
        Err(e) => { blog!("/dev/hci0 not available ({})", e); return park(); }
    };
    blog!("/dev/hci0 ready ({} stale set-up events dropped)", leftover);
    pump(&mut rx, h)
}

fn pump(rx: &mut Rx, h: u64) -> ! {
    let (mut idle, mut send_errs, mut sco_drops, mut logged) = (0u32, 0u32, 0u64, 0u64);
    let mut out = Vec::new();
    let mut tx = alloc::vec![0u8; 70_000];
    loop {
        let mut changed = false;
        rx.pull(false, &mut out);
        for p in out.drain(..) { hci::rx(h, &p); changed = true; }
        if rx.gone {
            blog!("controller gone: /dev/hci0 dead");
            hci::unregister(h);
            loop { msleep(1000); }
        }
        while let Some(n) = hci::tx_pop(h, &mut tx) {
            changed = true;
            let r = match tx[0] {
                0x01 => kusb::bt_send(rx.pipe, 0, &tx[1..n]),
                0x02 => kusb::bt_send(rx.pipe, 1, &tx[1..n]),
                _ => { sco_drops += 1; Ok(()) }
            };
            if let Err(e) = r {
                send_errs += 1;
                if send_errs <= 16 { blog!("send type {} failed: {}", tx[0], e); }
            }
        }
        let drops = hci::rx_drops(h);
        if drops != logged { blog!("rx drop n={} (sco tx dropped {})", drops, sco_drops); logged = drops; }
        if changed { hci::notify(h); idle = 0; } else { idle = idle.saturating_add(1); }
        if idle < 64 { yield_now() } else { msleep(if !hci::is_open(h) { 100 } else if idle < 2000 { 1 } else { 20 }) }
    }
}

extern "C" fn probe(intf: *const KUsbIf, _ctx: usize) -> i32 {
    let i = unsafe { &*intf };
    let cl = kmod_kapi::cmdline();
    if opt(cl, "bt") == Some("0") { return -19; }
    let fw = opt(cl, "btusb.fw") != Some("0");
    let pipe = match kusb::bt_open(i.handle) {
        Ok(p) => p,
        Err(e) => { blog!("slot {}: no usable HCI interface ({})", i.slot, e); return -19; }
    };
    blog!("controller {:04x}:{:04x} on slot {}", i.vid, i.pid, i.slot);
    let vid = i.vid;
    if kmod_kapi::sched::kthread::spawn("btusb", move || run(pipe, vid, fw)).is_none() {
        blog!("thread spawn failed");
        kusb::bt_close(pipe);
        return -5;
    }
    0
}

/// The controller left the bus: the pump sees `Gone` on its next poll and marks /dev/hci0 dead.
extern "C" fn detach(_handle: u64, _ctx: usize) {}

const IDS: [kmod_abi::KmodDevId; 1] = [kmod_rt::usb(0xffff, 0xffff, 0xE0_01_01, 0xFF_FF_FF)];

kmod_rt::module! {
    name: "bt",
    init: init,
    ids: [kmod_rt::usb(0xffff, 0xffff, 0xE0_01_01, 0xFF_FF_FF)],
}

fn init() -> i32 {
    if let Err(e) = kusb::class_register(&IDS, probe, Some(detach), 0) {
        kmod_rt::kerr!("usb_class_register failed: {}", e);
        return e;
    }
    0
}
