//! kapi exports: Bluetooth HCI transports (W2-6d)
//!
//! `kapi_hci_*` registers a transport with the in-kernel /dev/hci0 node (drivers::bt::hcidev: the
//! queues, the poll key and the btd-facing semantics stay in the kernel for every transport). The
//! node is claimed by exactly one transport. Unlike the design sketch's `send` callback the TX side
//! is polled (`kapi_hci_tx_pop`): the transport's pump thread already owns the controller.
//!
//! `kapi_usb_bt_*` is the USB side: it wraps `usb::xhci::btusb::BtUsb` (event interrupt endpoint, ACL
//! bulk IN/OUT and the class command on EP0 of a function claimed through the class registry) so the
//! btusb module never sees an xhci type.
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use drivers::bt::hcidev;
use usb::xhci::btusb::{BtPoll, BtUsb};

static HCI_OWNER: AtomicU64 = AtomicU64::new(0);
static HCI_NEXT: AtomicU64 = AtomicU64::new(1);

/// True while a module transport owns /dev/hci0 (ktest).
pub fn hci_registered() -> bool { HCI_OWNER.load(Ordering::Acquire) != 0 }

/// Claim /dev/hci0 for a transport: creates the port and registers the node. Returns the transport
/// handle (> 0), -16 when another transport owns it, -5 when no port could be created. Task context.
#[no_mangle]
pub extern "C" fn kapi_hci_register(_owner: kmod_abi::KmodId) -> i64 {
    if !hcidev::claim() { return -16; }
    let Some(port) = hcidev::create_port() else { return -5 };
    vfs_server::register_device_notify_open("/dev/hci0", port, 0, vfs_server::HCI_MAJOR, vfs_server::HCI_MINOR);
    let h = HCI_NEXT.fetch_add(1, Ordering::Relaxed);
    HCI_OWNER.store(h, Ordering::Release);
    h as i64
}

fn owned(h: u64) -> bool { h != 0 && HCI_OWNER.load(Ordering::Acquire) == h }

/// Queue one H4 packet (type byte first) received from the controller for btd. 0 queued, -11 dropped
/// (queue full), -19 bad handle, -22 bad arguments. IRQ-safe apart from the allocation.
#[no_mangle]
pub extern "C" fn kapi_hci_rx(h: u64, pkt: *const u8, len: usize) -> i32 {
    if !owned(h) { return -19; }
    if pkt.is_null() || len == 0 || len > 70_000 { return -22; }
    let v = unsafe { core::slice::from_raw_parts(pkt, len) }.to_vec();
    if hcidev::push_rx(v) { 0 } else { -11 }
}

/// Take the next H4 packet btd wrote. Returns its length, -11 when none is queued, -90 when `cap`
/// is too small (the packet is dropped), -19 bad handle. Never blocks.
#[no_mangle]
pub extern "C" fn kapi_hci_tx_pop(h: u64, buf: *mut u8, cap: usize) -> isize {
    if !owned(h) { return -19; }
    if buf.is_null() && cap != 0 { return -22; }
    let Some(p) = hcidev::pop_tx() else { return -11 };
    if p.len() > cap { return -90; }
    unsafe { core::ptr::copy_nonoverlapping(p.as_ptr(), buf, p.len()); }
    p.len() as isize
}

/// Transport control: op 0 wake readers/pollers, 1 mark the controller dead, 2 is /dev/hci0 open
/// (1/0), 3 count of RX packets dropped. -19 bad handle, -22 bad op.
#[no_mangle]
pub extern "C" fn kapi_hci_ctl(h: u64, op: u32) -> i64 {
    if !owned(h) { return -19; }
    match op {
        0 => { hcidev::notify(); 0 }
        1 => { hcidev::set_dead(); 0 }
        2 => hcidev::is_open() as i64,
        3 => hcidev::rx_drops() as i64,
        _ => -22,
    }
}

/// The transport is going away: /dev/hci0 reports a dead controller to btd. (The node and its port
/// stay; a later transport reclaims nothing until reboot, as with the built-in USB transport.)
#[no_mangle]
pub extern "C" fn kapi_hci_unregister(h: u64) {
    if owned(h) { hcidev::set_dead(); hcidev::notify(); }
}

// ── USB transport pipes ──

/// Flags written by `kapi_usb_bt_poll`.
pub const BT_BULK: u32 = 1;
pub const BT_RESET_EV: u32 = 2;
pub const BT_RESET_ACL: u32 = 4;

struct BtEntry { d: BtUsb, q: VecDeque<(bool, Vec<u8>)>, resets: u32 }

/// (pipe handle, function handle, boxed `BtEntry`)
static BTS: spin::Mutex<Vec<(u64, u64, usize)>> = spin::Mutex::new(Vec::new());
static BT_NEXT: AtomicU64 = AtomicU64::new(1);

/// Number of open BT USB pipes held by modules (ktest).
pub fn usb_bt_pipes() -> usize { BTS.lock().len() }

fn bt_ptr(p: u64) -> Option<*mut BtEntry> { BTS.lock().iter().find(|e| e.0 == p).map(|e| e.2 as *mut BtEntry) }

/// Configure the HCI interface (event interrupt-IN, ACL bulk IN/OUT) of the class-claimed function
/// and start receiving. Returns the pipe handle (> 0) or -errno. Task context.
#[no_mangle]
pub extern "C" fn kapi_usb_bt_open(handle: u64) -> i64 {
    let Some(f) = super::usbfn::func(handle) else { return -19 };
    match BtUsb::open(&f) {
        Ok(d) => {
            let p = BT_NEXT.fetch_add(1, Ordering::Relaxed);
            let ptr = Box::into_raw(Box::new(BtEntry { d, q: VecDeque::new(), resets: 0 })) as usize;
            BTS.lock().push((p, handle, ptr));
            p as i64
        }
        Err(_) => -5,
    }
}

/// Take the next received chunk (raw USB data: the module reassembles H4 packets). Returns its
/// length, writing BT_BULK (ACL, else event) and the endpoint-reset flags to `*flags`; -11 when none
/// is queued (flags may still carry a reset), -16 controller busy, -19 device gone or bad handle.
/// Never blocks. A chunk longer than `cap` is truncated.
#[no_mangle]
pub extern "C" fn kapi_usb_bt_poll(pipe: u64, buf: *mut u8, cap: usize, flags: *mut u32) -> isize {
    if buf.is_null() || flags.is_null() { return -22; }
    let Some(p) = bt_ptr(pipe) else { return -19 };
    let e = unsafe { &mut *p };
    let mut rc = 0isize;
    if e.q.is_empty() {
        match e.d.poll() {
            BtPoll::Data(v) => e.q.extend(v),
            BtPoll::Busy => rc = -16,
            BtPoll::Gone => rc = -19,
        }
        let (re, rb) = e.d.take_resets();
        if re { e.resets |= BT_RESET_EV; }
        if rb { e.resets |= BT_RESET_ACL; }
    }
    unsafe { *flags = core::mem::take(&mut e.resets); }
    if rc != 0 { return rc; }
    let Some((bulk, c)) = e.q.pop_front() else { return -11 };
    let k = c.len().min(cap);
    unsafe { core::ptr::copy_nonoverlapping(c.as_ptr(), buf, k); *flags |= if bulk { BT_BULK } else { 0 }; }
    c.len() as isize
}

/// Send one packet: kind 0 = HCI command (class request on EP0), 1 = ACL (bulk OUT). `buf` has no H4
/// type byte. Sleeps (up to 2 s). 0 or -errno (-19 gone, -5 other).
#[no_mangle]
pub extern "C" fn kapi_usb_bt_send(pipe: u64, kind: u32, buf: *const u8, len: usize) -> i32 {
    if buf.is_null() || len == 0 { return -22; }
    let Some(p) = bt_ptr(pipe) else { return -19 };
    let e = unsafe { &mut *p };
    let s = unsafe { core::slice::from_raw_parts(buf, len) };
    let r = match kind { 0 => e.d.send_cmd(s), 1 => e.d.send_bulk(s), _ => return -22 };
    match r { Ok(()) => 0, Err(x) => super::usbfn::usb_errno(x) }
}

/// Forget the pipe (the endpoints stay configured until the device leaves).
#[no_mangle]
pub extern "C" fn kapi_usb_bt_close(pipe: u64) {
    let mut found = None;
    BTS.lock().retain(|e| { if e.0 == pipe { found = Some(e.2); false } else { true } });
    if let Some(p) = found { drop(unsafe { Box::from_raw(p as *mut BtEntry) }); }
}

/// Close the pipes of function `h` (device left).
pub(super) fn drop_bt(h: u64) {
    let mut gone = Vec::new();
    BTS.lock().retain(|e| { if e.1 == h { gone.push(e.2); false } else { true } });
    for p in gone { drop(unsafe { Box::from_raw(p as *mut BtEntry) }); }
}
