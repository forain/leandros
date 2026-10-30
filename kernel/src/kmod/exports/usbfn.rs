//! kapi exports: USB class drivers and function handles
use super::*;

// ── USB function and isochronous handle tables ──
//
// A module never sees an xhci type. A claimed interface is a `u64` handle into FUNCS; an open
// isochronous endpoint is a `u64` handle into PIPES. Handles count up and are never reused, so a
// stale one fails with -19 instead of hitting somebody else's function. Locks are never held across
// a call into xhci or into a module.

use usb::xhci::isoch::{IsochPipe, Poll as IsoPoll, StreamFormat, UsbFunction};

/// (handle, owning driver's registration pointer, function)
pub(super) static FUNCS: spin::Mutex<alloc::vec::Vec<(u64, usize, Arc<UsbFunction>)>> = spin::Mutex::new(alloc::vec::Vec::new());
/// (pipe handle, function handle, `*mut IsochPipe`). The module serialises use of one pipe.
pub(super) static PIPES: spin::Mutex<alloc::vec::Vec<(u64, u64, usize)>> = spin::Mutex::new(alloc::vec::Vec::new());
pub(super) static NEXT_HANDLE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// Number of USB functions held by module class drivers (ktest).
pub fn usb_claimed() -> usize { FUNCS.lock().len() }
/// Number of open isochronous pipes held by modules (ktest).
pub fn usb_pipes() -> usize { PIPES.lock().len() }

pub(super) fn func(h: u64) -> Option<Arc<UsbFunction>> { FUNCS.lock().iter().find(|e| e.0 == h).map(|e| e.2.clone()) }

pub(super) fn usb_errno(e: usb::UsbErr) -> i32 {
    match e {
        usb::UsbErr::Stall => -32,
        usb::UsbErr::Timeout => -110,
        usb::UsbErr::Babble => -75,
        usb::UsbErr::Disconnected => -19,
        usb::UsbErr::Other(_) => -5,
    }
}

/// Forget function `h` and close the pipes still open on it.
pub(super) fn drop_func(h: u64) {
    FUNCS.lock().retain(|e| e.0 != h);
    let mut dead = alloc::vec::Vec::new();
    PIPES.lock().retain(|e| { if e.1 == h { dead.push(e.2); false } else { true } });
    for p in dead { unsafe { Box::from_raw(p as *mut IsochPipe) }.close(); }
}

/// USB control transfer on the claimed function. The direction is bit 7 of `setup.request_type`;
/// `buf` must hold `setup.length` bytes. Returns the bytes transferred (IN) / 0 (OUT), or -errno.
/// Task context.
#[no_mangle]
pub extern "C" fn kapi_usb_ctrl(handle: u64, setup: *const kmod_abi::KUsbSetup, buf: *mut u8, len: usize,
                                timeout_ms: u32) -> i32 {
    if setup.is_null() { return -22; }
    let s = unsafe { *setup };
    let n = s.length as usize;
    if n > len || (n != 0 && buf.is_null()) { return -22; }
    let Some(f) = func(handle) else { return -19 };
    let to = if timeout_ms == 0 { 5000 } else { timeout_ms };
    let (rt, rq, v, i) = (s.request_type, s.request, s.value, s.index);
    if rt & 0x80 != 0 {
        let b: &mut [u8] = if n == 0 { &mut [] } else { unsafe { core::slice::from_raw_parts_mut(buf, n) } };
        match f.control_in_t(rt, rq, v, i, b, to) { Ok(k) => k as i32, Err(e) => usb_errno(e) }
    } else {
        let b: &[u8] = if n == 0 { &[] } else { unsafe { core::slice::from_raw_parts(buf, n) } };
        match f.control_out_t(rt, rq, v, i, b, to) { Ok(()) => 0, Err(e) => usb_errno(e) }
    }
}

/// SET_INTERFACE on a claimed function. 0 or -errno. Task context.
#[no_mangle]
pub extern "C" fn kapi_usb_set_alt(handle: u64, iface: u8, alt: u8) -> i32 {
    let Some(f) = func(handle) else { return -19 };
    match f.set_interface(iface, alt) { Ok(()) => 0, Err(e) => usb_errno(e) }
}

/// Copy the function's configuration descriptor into `buf` (up to `len` bytes). Returns the full
/// descriptor length (call with `len` 0 to size it), or -errno.
#[no_mangle]
pub extern "C" fn kapi_usb_cfg(handle: u64, buf: *mut u8, len: usize) -> i32 {
    let Some(f) = func(handle) else { return -19 };
    let k = f.cfg.len().min(len);
    if k != 0 {
        if buf.is_null() { return -22; }
        unsafe { core::ptr::copy_nonoverlapping(f.cfg.as_ptr(), buf, k); }
    }
    f.cfg.len() as i32
}

/// Open the isochronous endpoint `ep_addr` of alternate setting `alt` of interface `iface` (it must
/// be in the configuration descriptor): bandwidth is configured and SET_INTERFACE is issued. `framed`
/// keeps one FIFO record per service interval (read back with `kapi_usb_isoch_reap`). Returns the pipe
/// handle (> 0) or -errno. Task context.
#[no_mangle]
pub extern "C" fn kapi_usb_isoch_open(handle: u64, iface: u8, alt: u8, ep_addr: u8, fifo_bytes: usize,
                                      ntds: usize, framed: bool) -> i64 {
    let Some(f) = func(handle) else { return -19 };
    let Some(d) = usb::isocdesc::scan_isoch(&f.cfg).into_iter()
        .find(|d| d.iface == iface && d.alt == alt && d.ep_addr == ep_addr) else { return -22 };
    match IsochPipe::open_ex(&f, &d, StreamFormat { rate: 0, frame_bytes: 0 }, fifo_bytes, ntds, framed) {
        Ok(p) => {
            let ph = NEXT_HANDLE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let ptr = Box::into_raw(Box::new(p)) as usize;
            PIPES.lock().push((ph, handle, ptr));
            ph as i64
        }
        Err(_) => -5,
    }
}

pub(super) fn pipe_ptr(ph: u64) -> Option<*mut IsochPipe> {
    PIPES.try_lock()?.iter().find(|e| e.0 == ph).map(|e| e.2 as *mut IsochPipe)
}

/// Retire completed transfers and keep the endpoint's ring full (IRQ-safe: never blocks).
/// 0 ok, 1 controller busy (try next tick), 2 device gone (reported once), -19 bad handle.
#[no_mangle]
pub extern "C" fn kapi_usb_isoch_submit(pipe: u64) -> i32 {
    let Some(p) = pipe_ptr(pipe) else { return 1 };
    match unsafe { (*p).poll() } { IsoPoll::Ok => 0, IsoPoll::Busy => 1, IsoPoll::Gone => 2 }
}

/// Take the oldest framed packet. Returns its length (it may exceed `len`; the excess is dropped),
/// -11 when none is queued, -19 for a bad handle. Never blocks.
#[no_mangle]
pub extern "C" fn kapi_usb_isoch_reap(pipe: u64, buf: *mut u8, len: usize) -> i32 {
    let Some(p) = pipe_ptr(pipe) else { return -11 };
    if buf.is_null() && len != 0 { return -22; }
    let out: &mut [u8] = if len == 0 { &mut [] } else { unsafe { core::slice::from_raw_parts_mut(buf, len) } };
    match unsafe { (*p).read_packet(out) } { Some(n) => n as i32, None => -11 }
}

/// Stop the endpoint, select alternate setting 0 and free the pipe. Task context.
#[no_mangle]
pub extern "C" fn kapi_usb_isoch_close(pipe: u64) {
    let mut found = None;
    PIPES.lock().retain(|e| { if e.0 == pipe { found = Some(e.2); false } else { true } });
    if let Some(p) = found { unsafe { Box::from_raw(p as *mut IsochPipe) }.close(); }
}

/// Remove a camera registered with `kapi_v4l2_register` (the device went away).
#[no_mangle]
pub extern "C" fn kapi_v4l2_unregister(ctx: usize) { crate::syscall::v4l2::unregister_camera(ctx); }

/// What a registered class driver's `ctx` points at.
pub(super) struct UsbCls { probe: KUsbProbeFn, detach: Option<KUsbDetachFn>, ctx: usize }

pub(super) fn usb_probe_tramp(f: UsbFunction, m: &usb::class::Match, p: usize) -> Result<(), UsbFunction> {
    let b = unsafe { &*(p as *const UsbCls) };
    let h = NEXT_HANDLE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let f = Arc::new(f);
    // The probe talks to the device through the handle, so it must exist before the call.
    FUNCS.lock().push((h, p, f.clone()));
    let intf = KUsbIf {
        handle: h, vid: m.vid, pid: m.pid, slot: f.slot,
        class: m.class, subclass: m.subclass, protocol: m.protocol, iface: m.iface, _pad: [0; 7],
    };
    if (b.probe)(&intf, b.ctx) == 0 { return Ok(()); }
    drop_func(h);
    match Arc::try_unwrap(f) { Ok(f) => Err(f), Err(_) => Ok(()) }
}

/// The device on (`hc`, `slot`) left: tell the driver, then drop its handles.
pub(super) fn usb_detach_tramp(hc: &usb::xhci::Hc, slot: u8, p: usize) {
    let b = unsafe { &*(p as *const UsbCls) };
    let hs: alloc::vec::Vec<u64> = FUNCS.lock().iter()
        .filter(|e| e.1 == p && core::ptr::eq(&*e.2.hc, hc) && e.2.slot == slot).map(|e| e.0).collect();
    for h in hs {
        if let Some(d) = b.detach { d(h, b.ctx); }
        drop_func(h);
    }
}

/// Register a USB class driver for the given USB id-table entries. Interfaces that were
/// enumerated earlier and left unclaimed are offered to it before this returns. `detach` runs when
/// a claimed device leaves the bus. 0 ok, -22 bad arguments.
#[no_mangle]
pub extern "C" fn kapi_usb_class_register(owner: KmodId, ids: *const KmodDevId, nids: usize,
                                          probe: KUsbProbeFn, detach: Option<KUsbDetachFn>, ctx: usize) -> i32 {
    if ids.is_null() || nids == 0 || nids > 64 { return -22; }
    let v: alloc::vec::Vec<usb::class::IdEntry> = unsafe { core::slice::from_raw_parts(ids, nids) }
        .iter().filter(|e| e.bus == BUS_USB)
        .map(|e| usb::class::IdEntry { vid: e.a, pid: e.b, class: e.class, mask: e.class_mask }).collect();
    if v.is_empty() { return -22; }
    let p = Box::into_raw(Box::new(UsbCls { probe, detach, ctx })) as usize;
    crate::kmod::registry::note_registration(owner, RegKind::USB_CLASS, p as u64);
    usb::class::register(usb::class::Driver {
        owner, ids: usb::class::leak_ids(v), probe: usb_probe_tramp, detach: Some(usb_detach_tramp), ctx: p,
    });
    0
}

