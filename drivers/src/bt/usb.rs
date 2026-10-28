//! Bluetooth over USB (btusb): the laptop's Intel 8265 (8087:0a2b) and any
//! other USB HCI controller. A kernel thread claims the function the xHCI
//! enumeration registered (usb::xhci::btusb), runs the Intel firmware
//! set-up (btintel.rs) for Intel controllers, then registers `/dev/hci0`
//! (hcidev.rs, the same node and semantics as the Pi's UART transport) and
//! becomes the pump: events from the interrupt endpoint and ACL from bulk IN
//! are reassembled (usb_reasm.rs) into H4 packets for btd; btd's commands go
//! out on EP0, its ACL on bulk OUT. SCO is not supported (dropped).
//!
//! Firmware: /lib/firmware/intel/ibt-<variant>-<revid>.{sfi,ddc}
//! (ports/intel-bt/fetch.sh). Command line: `bt=0` skips the driver,
//! `btusb.fw=0` skips the Intel download (controller used as found).
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use usb::xhci::btusb::{self, BtPoll, BtUsb};

use super::btintel;
use super::hcidev;
use super::usb_reasm::{Reasm, H4_ACL, H4_EVENT};

extern "C" { fn serial_print_str_raw(s: *const u8, len: usize); }

pub(crate) fn log(msg: &str) {
    let mut s = String::with_capacity(msg.len() + 10);
    s.push_str("[btusb] ");
    s.push_str(msg);
    s.push('\n');
    unsafe { serial_print_str_raw(s.as_ptr(), s.len()); }
}

fn now_ms() -> u64 { sched::monotonic_ns() / 1_000_000 }

fn opt<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    cmdline.split_whitespace().find_map(|w| w.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

const FW_DIR: &str = "/lib/firmware/intel/";
/// How long the thread waits for a USB Bluetooth function to show up.
const PROBE_WAIT_MS: u64 = 20_000;

fn load_fw(name: &str) -> Option<Vec<u8>> {
    let t0 = now_ms();
    while !crate::firmware::root_ready() {
        if now_ms() - t0 > 120_000 { log("root filesystem not mounted within 120 s"); return None; }
        sched::kthread::msleep(250);
    }
    let path = format!("{}{}", FW_DIR, name);
    let mut f = crate::firmware::request_firmware(&path, 4 << 20).ok()?;
    let va = f.map()?;
    let v = unsafe { core::slice::from_raw_parts(va as *const u8, f.len()) }.to_vec();
    Some(v)
}

struct Link<'a> {
    d: &'a mut BtUsb,
    ev: Reasm,
    bev: Reasm,
    q: VecDeque<Vec<u8>>,
    boot: bool,
    dead: bool,
}

impl Link<'_> {
    fn pull(&mut self) -> bool {
        match self.d.poll() {
            BtPoll::Data(v) => {
                let mut out = Vec::new();
                let any = !v.is_empty();
                for (bulk, c) in v {
                    // In the Intel bootloader, events also arrive on bulk IN.
                    if !bulk { self.ev.push(&c, &mut out) } else if self.boot { self.bev.push(&c, &mut out) }
                }
                self.q.extend(out);
                any
            }
            BtPoll::Busy => false,
            BtPoll::Gone => { self.dead = true; false }
        }
    }
}

impl btintel::Link for Link<'_> {
    fn send_cmd(&mut self, pkt: &[u8], bulk: bool) -> bool {
        let r = if bulk { self.d.send_bulk(pkt) } else { self.d.send_cmd(pkt) };
        if let Err(e) = &r { log(&format!("send {:02x}{:02x} failed: {:?}", pkt[1], pkt[0], e)); }
        r.is_ok()
    }
    fn event(&mut self, timeout_ms: u32) -> Option<Vec<u8>> {
        let (t0, end) = (now_ms(), now_ms() + timeout_ms as u64);
        loop {
            if let Some(e) = self.q.pop_front() { return Some(e[1..].to_vec()); }
            if self.dead { return None; }
            if self.pull() { continue; }
            let now = now_ms();
            if now >= end { return None; }
            if now - t0 < 3 { sched::kthread::yield_now() } else { sched::kthread::msleep(1) }
        }
    }
    fn now_ms(&mut self) -> u64 { now_ms() }
    fn operational(&mut self) { self.boot = false; self.bev.reset(); }
    fn log(&mut self, msg: &str) { log(msg) }
}

/// Starts the probe thread (kernel init, after the USB enumeration).
/// `on_ready(port)` registers `/dev/hci0` once the controller is set up.
pub fn start(cmdline: &str, on_ready: fn(port: u32)) {
    if opt(cmdline, "bt") == Some("0") { return; }
    let fw = opt(cmdline, "btusb.fw") != Some("0");
    if sched::kthread::spawn("btusb", move || run(fw, on_ready)).is_none() {
        log("thread spawn failed");
    }
}

fn run(fw: bool, on_ready: fn(port: u32)) {
    let t0 = now_ms();
    let f = loop {
        if let Some(f) = btusb::take() { break f; }
        if now_ms() - t0 > PROBE_WAIT_MS { return; }
        sched::kthread::msleep(200);
    };
    let (vid, pid) = usb::devices().iter().find(|d| d.slot == f.slot && d.class == 0xE0)
        .map_or((0, 0), |d| (d.vendor, d.product));
    log(&format!("controller {:04x}:{:04x} on slot {}", vid, pid, f.slot));
    let mut d = match BtUsb::open(&f) {
        Ok(d) => d,
        Err(e) => return log(&format!("fail stage=open {}", e)),
    };
    let mut link = Link { d: &mut d, ev: Reasm::new(H4_EVENT), bev: Reasm::new(H4_EVENT), q: VecDeque::new(), boot: true, dead: false };
    if vid == 0x8087 && fw {
        let t = now_ms();
        match btintel::setup(&mut link, &mut |n| load_fw(n)) {
            Ok(r) => log(&format!("intel set-up done in {} ms ({}{} DDC records)", now_ms() - t,
                                  if r.already_operational { "firmware already running, " } else { "" }, r.ddc_records)),
            Err(e) => return log(&format!("fail stage=intel {}", e.describe())),
        }
    }
    let leftover = link.q.len();
    drop(link);
    let Some(port) = hcidev::create_port() else { return log("fail stage=port") };
    on_ready(port);
    log(&format!("/dev/hci0 ready ({} stale set-up events dropped)", leftover));
    pump(&mut d);
}

fn pump(d: &mut BtUsb) -> ! {
    let (mut ev, mut acl) = (Reasm::new(H4_EVENT), Reasm::new(H4_ACL));
    let (mut idle, mut send_errs, mut sco_drops, mut logged_drops) = (0u32, 0u32, 0u64, 0u64);
    let mut out = Vec::new();
    loop {
        let mut changed = false;
        match d.poll() {
            BtPoll::Data(v) => {
                for (bulk, c) in v {
                    if bulk { acl.push(&c, &mut out) } else { ev.push(&c, &mut out) }
                }
                for p in out.drain(..) { hcidev::push_rx(p); changed = true; }
            }
            BtPoll::Busy => {}
            BtPoll::Gone => {
                log("controller gone: /dev/hci0 dead");
                hcidev::set_dead();
                loop { sched::kthread::msleep(1000); }
            }
        }
        while let Some(p) = hcidev::pop_tx() {
            changed = true;
            let r = match p[0] {
                0x01 => d.send_cmd(&p[1..]),
                0x02 => d.send_bulk(&p[1..]),
                _ => { sco_drops += 1; Ok(()) }
            };
            if let Err(e) = r {
                send_errs += 1;
                if send_errs <= 16 { log(&format!("send type {} failed: {:?}", p[0], e)); }
            }
        }
        let drops = hcidev::rx_drops();
        if drops != logged_drops { log(&format!("rx drop n={} (sco tx dropped {})", drops, sco_drops)); logged_drops = drops; }
        if changed { hcidev::notify(); idle = 0; } else { idle = idle.saturating_add(1); }
        if idle < 64 { sched::kthread::yield_now() } else { sched::kthread::msleep(if idle < 2000 { 1 } else { 4 }) }
    }
}
