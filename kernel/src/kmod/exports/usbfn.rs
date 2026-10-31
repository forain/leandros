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
/// Open interrupt-IN pipes: (pipe handle, function handle, `*mut IntrEntry`).
pub(super) static INTRS: spin::Mutex<alloc::vec::Vec<(u64, u64, usize)>> = spin::Mutex::new(alloc::vec::Vec::new());
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
    // Interrupt pipes die with the device (the slot is disabled by the hot-plug scan).
    let mut gone = alloc::vec::Vec::new();
    INTRS.lock().retain(|e| { if e.1 == h { gone.push(e.2); false } else { true } });
    for p in gone { drop(unsafe { Box::from_raw(p as *mut IntrEntry) }); }
    super::hci::drop_bt(h);
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
pub(super) struct UsbCls {
    probe: KUsbProbeFn, detach: Option<KUsbDetachFn>, ctx: usize,
    gate: Option<Arc<crate::kmod::gate::Gate>>, owner: KmodId,
}

/// The `Match` each claimed function was probed with (a reload re-offers the function).
static MATCHES: spin::Mutex<alloc::vec::Vec<(u64, usb::class::Match)>> = spin::Mutex::new(alloc::vec::Vec::new());

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
    let rc = match &b.gate {
        None => (b.probe)(&intf, b.ctx),
        Some(g) => g.call(crate::kmod::gate::Class::Upper, || (b.probe)(&intf, b.ctx)).unwrap_or(-19),
    };
    if rc == 0 { MATCHES.lock().push((h, *m)); return Ok(()); }
    drop_func(h);
    match Arc::try_unwrap(f) { Ok(f) => Err(f), Err(_) => Ok(()) }
}

/// Handles whose device left while the owner's Upper gate was closed (a refused unload in flight):
/// the driver's `detach` has not run yet; it is replayed after the gate reopens, or by the teardown.
static PENDING_GONE: spin::Mutex<alloc::vec::Vec<u64>> = spin::Mutex::new(alloc::vec::Vec::new());

fn run_detach(b: &UsbCls, h: u64) {
    if let Some(d) = b.detach { d(h, b.ctx); }
    MATCHES.lock().retain(|e| e.0 != h);
    drop_func(h);
}

/// The device on (`hc`, `slot`) left: tell the driver, then drop its handles. With the gate closed the
/// handle is parked instead (detach must not be skipped while the handle is dropped).
pub(super) fn usb_detach_tramp(hc: &usb::xhci::Hc, slot: u8, p: usize) {
    let b = unsafe { &*(p as *const UsbCls) };
    let hs: alloc::vec::Vec<u64> = FUNCS.lock().iter()
        .filter(|e| e.1 == p && core::ptr::eq(&*e.2.hc, hc) && e.2.slot == slot).map(|e| e.0).collect();
    for h in hs {
        match &b.gate {
            None => run_detach(b, h),
            Some(g) => {
                let ran = match b.detach {
                    Some(d) => g.call(crate::kmod::gate::Class::Upper, || d(h, b.ctx)).is_some(),
                    None => true,
                };
                if ran {
                    MATCHES.lock().retain(|e| e.0 != h);
                    drop_func(h);
                } else {
                    let mut pg = PENDING_GONE.lock();
                    if !pg.contains(&h) { pg.push(h); }
                }
            }
        }
    }
}

/// An unload was refused and the Upper gate reopened: replay the detaches that arrived while it was
/// closed, and re-offer the functions whose probe was declined meanwhile.
pub fn usb_class_reopened(p: u64) {
    let p = p as usize;
    let b = unsafe { &*(p as *const UsbCls) };
    let mine: alloc::vec::Vec<u64> = {
        let funcs = FUNCS.lock();
        let mut pg = PENDING_GONE.lock();
        let m: alloc::vec::Vec<u64> = pg.iter().copied().filter(|h| funcs.iter().any(|e| e.0 == *h && e.1 == p)).collect();
        pg.retain(|h| !m.contains(h));
        m
    };
    for h in mine {
        if let (Some(d), Some(g)) = (b.detach, &b.gate) { g.call(crate::kmod::gate::Class::Upper, || d(h, b.ctx)); }
        MATCHES.lock().retain(|e| e.0 != h);
        drop_func(h);
    }
    usb::class::reoffer_owner(b.owner);
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
    let owner = crate::kmod::gate::owner(owner);
    let Some(gate) = crate::kmod::gate::get(owner) else { return -19 };
    let gate = Some(gate);
    let p = Box::into_raw(Box::new(UsbCls { probe, detach, ctx, gate, owner })) as usize;
    crate::kmod::registry::note_registration(owner, RegKind::USB_CLASS, p as u64);
    usb::class::register(usb::class::Driver {
        owner, ids: usb::class::leak_ids(v), probe: usb_probe_tramp, detach: Some(usb_detach_tramp), ctx: p,
    });
    0
}


// ── interrupt-IN pipes (boot HID) ──

/// An open interrupt pipe with the reports its last poll harvested.
pub(super) struct IntrEntry { pipe: usb::xhci::intr::IntrPipe, reports: usb::xhci::intr::Reports, next: usize }

/// Number of open interrupt pipes held by modules (ktest).
pub fn usb_intr_pipes() -> usize { INTRS.lock().len() }

/// Open the interrupt-IN endpoint `ep` (a boot HID interface in the function's configuration):
/// the endpoint is configured and report TDs are queued. Protocol, idle and LED requests are the
/// driver's, over `kapi_usb_ctrl`. `max` is the largest report the caller reads (<= 64).
/// Returns the pipe handle (> 0) or -errno. Task context.
#[no_mangle]
pub extern "C" fn kapi_usb_intr_open(handle: u64, ep: u8, max: usize) -> i64 {
    if max == 0 || max > usb::xhci::intr::REPORT_MAX { return -22; }
    let Some(f) = func(handle) else { return -19 };
    let Some(d) = usb::hid::parse_config(&f.cfg).boot.into_iter().find(|d| d.ep_addr == ep) else { return -22 };
    match usb::xhci::intr::open_on(&f, &d) {
        Ok(pipe) => {
            let ph = NEXT_HANDLE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let ptr = Box::into_raw(Box::new(IntrEntry { pipe, reports: usb::xhci::intr::Reports::new(), next: 0 })) as usize;
            INTRS.lock().push((ph, handle, ptr));
            ph as i64
        }
        Err(_) => -5,
    }
}

fn intr_ptr(ph: u64) -> Option<*mut IntrEntry> {
    INTRS.try_lock()?.iter().find(|e| e.0 == ph).map(|e| e.2 as *mut IntrEntry)
}

/// Take the next report (IRQ-safe: never blocks). Returns its length, -11 when none is queued,
/// -16 when the controller was busy (retry next tick), -19 when the device is gone or the handle
/// is bad. A report longer than `cap` is truncated.
#[no_mangle]
pub extern "C" fn kapi_usb_intr_poll(pipe: u64, buf: *mut u8, cap: usize) -> isize {
    if buf.is_null() && cap != 0 { return -22; }
    let Some(p) = intr_ptr(pipe) else { return -19 };
    let e = unsafe { &mut *p };
    if e.next >= e.reports.n {
        e.reports.n = 0;
        e.next = 0;
        match e.pipe.poll(&mut e.reports) {
            usb::xhci::intr::Poll::Busy => return -16,
            usb::xhci::intr::Poll::Gone => return -19,
            usb::xhci::intr::Poll::Ok => {}
        }
        if e.reports.n == 0 { return -11; }
    }
    let r = e.reports.get(e.next);
    e.next += 1;
    let k = r.len().min(cap);
    if k != 0 { unsafe { core::ptr::copy_nonoverlapping(r.as_ptr(), buf, k); } }
    r.len() as isize
}

/// Send a HID output report (SET_REPORT, report id 0) to the pipe's interface without waiting
/// (IRQ-safe). 0 queued, -16 one is still in flight (retry), -95 the device cannot take it,
/// -19 bad handle.
#[no_mangle]
pub extern "C" fn kapi_usb_intr_out(pipe: u64, buf: *const u8, len: usize) -> i32 {
    if buf.is_null() || len == 0 || len > 8 { return -22; }
    let Some(p) = intr_ptr(pipe) else { return -19 };
    let e = unsafe { &mut *p };
    if !e.pipe.leds_usable() { return -95; }
    if e.pipe.set_report_nowait(unsafe { core::slice::from_raw_parts(buf, len) }) { 0 } else { -16 }
}

/// Forget the pipe. The endpoint stays configured until the device leaves.
#[no_mangle]
pub extern "C" fn kapi_usb_intr_close(pipe: u64) {
    let mut found = None;
    INTRS.lock().retain(|e| { if e.0 == pipe { found = Some(e.2); false } else { true } });
    if let Some(p) = found { drop(unsafe { Box::from_raw(p as *mut IntrEntry) }); }
}

// ── bulk pipes (mass storage) ──

/// Open bulk-only transports: (pipe, function handle, boxed `XhciBulk`).
static BULKS: spin::Mutex<alloc::vec::Vec<(u64, u64, usize)>> = spin::Mutex::new(alloc::vec::Vec::new());

/// Number of open bulk pipes held by modules (ktest).
pub fn usb_bulk_pipes() -> usize { BULKS.lock().len() }

/// Configure the bulk-only interface `iface` of the function's configuration (SET_CONFIGURATION is
/// done): both bulk endpoints get rings and the alternate setting is selected. Returns the pipe
/// handle (> 0) or -errno. Task context.
#[no_mangle]
pub extern "C" fn kapi_usb_bulk_open(handle: u64, iface: u8) -> i64 {
    let Some(f) = func(handle) else { return -19 };
    match usb::xhci::open_bulk_on(&f, iface) {
        Ok(b) => {
            let ph = NEXT_HANDLE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let ptr = Box::into_raw(Box::new(b)) as usize;
            BULKS.lock().push((ph, handle, ptr));
            ph as i64
        }
        Err(_) => -5,
    }
}

fn bulk_ptr(ph: u64) -> Option<*mut usb::XhciBulk> {
    BULKS.lock().iter().find(|e| e.0 == ph).map(|e| e.2 as *mut usb::XhciBulk)
}

/// One transfer on an open bulk pipe. `op`: 0 bulk OUT, 1 bulk IN, 2 clear-halt IN, 3 clear-halt OUT,
/// 4 class control OUT, 5 class control IN (for 4/5 `arg` = request | value << 8 | index << 24; the
/// request goes to the pipe's interface). Returns the byte count or -errno (-32 stall, -110 timeout,
/// -19 gone). Task context (sleeps).
#[no_mangle]
pub extern "C" fn kapi_usb_bulk(pipe: u64, op: u32, arg: u64, buf: *mut u8, len: usize, timeout_ns: u64) -> isize {
    use usb::BulkTransport;
    if buf.is_null() && len != 0 { return -22; }
    let Some(p) = bulk_ptr(pipe) else { return -19 };
    let b = unsafe { &mut *p };
    let ms = (timeout_ns / 1_000_000).clamp(1, u32::MAX as u64) as u32;
    let (req, val, idx) = (arg as u8, (arg >> 8) as u16, (arg >> 24) as u16);
    let sl_mut = || unsafe { core::slice::from_raw_parts_mut(buf, len) };
    let r = match op {
        0 => b.bulk_out(unsafe { core::slice::from_raw_parts(buf, len) }, ms),
        1 => b.bulk_in(sl_mut(), ms),
        2 => b.clear_halt_in().map(|_| 0),
        3 => b.clear_halt_out().map(|_| 0),
        4 => b.class_control_out(req, val, idx, unsafe { core::slice::from_raw_parts(buf, len) }).map(|_| 0),
        5 => b.class_control_in(req, val, idx, sl_mut()),
        _ => return -22,
    };
    match r { Ok(n) => n as isize, Err(e) => usb_errno(e) as isize }
}

/// Forget the pipe. The endpoints stay configured until the device leaves.
#[no_mangle]
pub extern "C" fn kapi_usb_bulk_close(pipe: u64) {
    let mut found = None;
    BULKS.lock().retain(|e| { if e.0 == pipe { found = Some(e.2); false } else { true } });
    if let Some(p) = found { drop(unsafe { Box::from_raw(p as *mut usb::XhciBulk) }); }
}

/// Unload, on the exit thread before the module's `exit()` (its gates are closed and drained, so the
/// module runs only here): stop new probes, run the driver's `detach` for every function it claimed,
/// drop the handles, and hand each function back to the unclaimed list so a reload binds it again.
pub fn usb_class_teardown(p: u64) {
    let p = p as usize;
    let b = unsafe { &*(p as *const UsbCls) };
    usb::class::unregister_owner(b.owner);
    let hs: alloc::vec::Vec<(u64, Arc<UsbFunction>)> = FUNCS.lock().iter().filter(|e| e.1 == p).map(|e| (e.0, e.2.clone())).collect();
    for (h, f) in hs {
        if let Some(d) = b.detach { d(h, b.ctx); }
        let gone = { let mut pg = PENDING_GONE.lock(); let n = pg.len(); pg.retain(|x| *x != h); pg.len() != n };
        let m = { let mut g = MATCHES.lock(); g.iter().position(|e| e.0 == h).map(|i| g.remove(i).1) };
        drop_func(h);
        if gone { continue; } // the device left during a refused unload: nothing to hand back
        match (m, Arc::try_unwrap(f)) {
            (Some(m), Ok(f)) => { if !usb::class::requeue(m, f) { crate::kmod::log("[kmod] usb teardown: device already left, function dropped\n"); } }
            (Some(_), Err(_)) => crate::kmod::log("[kmod] usb teardown: function still referenced, not requeued\n"),
            _ => {}
        }
    }
}
