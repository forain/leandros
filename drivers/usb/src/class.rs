//! USB class-driver registry and the tick-callback list.
//!
//! A class driver names the interfaces it wants with id-table entries (vid/pid with 0xffff as
//! wildcard, class/subclass/protocol under a mask, the same shape as `KmodDevId` for USB). At
//! enumerate time [`wants`] tells the xhci layer whether to configure the device for a class
//! driver and [`offer`] hands the configured function to the first driver that claims it. A
//! function nobody claims is kept, and replayed to every driver registered later (a module
//! loaded after enumeration still binds). The built-in drivers live in a static table; module
//! drivers join a runtime list through [`register`].

use alloc::boxed::Box;
use alloc::vec::Vec;
use kmod_abi::KmodId;
use spin::Mutex;

use crate::xhci::isoch::UsbFunction;

/// One id-table entry. `vid`/`pid` 0xffff match anything. `class` is class<<16|sub<<8|proto,
/// compared under `mask` against each interface descriptor.
#[derive(Clone, Copy, Debug)]
pub struct IdEntry {
    pub vid: u16,
    pub pid: u16,
    pub class: u32,
    pub mask: u32,
}

impl IdEntry {
    fn matches(&self, vid: u16, pid: u16, intf: u32) -> bool {
        (self.vid == 0xffff || self.vid == vid)
            && (self.pid == 0xffff || self.pid == pid)
            && (intf & self.mask) == (self.class & self.mask)
    }
}

/// What a probe is told about the interface that matched.
#[derive(Clone, Copy, Debug, Default)]
pub struct Match {
    pub bus: u8,
    pub vid: u16,
    pub pid: u16,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub iface: u8,
}

/// Claim the function (`Ok`) or give it back (`Err`).
pub type ProbeFn = fn(UsbFunction, &Match, usize) -> Result<(), UsbFunction>;
/// The device on (`hc`, `slot`) is gone.
pub type DetachFn = fn(&crate::xhci::Hc, u8, usize);

#[derive(Clone, Copy)]
pub struct Driver {
    pub owner: KmodId,
    pub ids: &'static [IdEntry],
    pub probe: ProbeFn,
    pub detach: Option<DetachFn>,
    pub ctx: usize,
}

/// Built-in drivers: video streaming interfaces (class 0x0E, subclass 0x02) go to uvc.
#[cfg(not(feature = "uvc-module"))]
static BUILTIN: [Driver; 1] = [Driver {
    owner: KmodId(0),
    ids: &[IdEntry { vid: 0xffff, pid: 0xffff, class: 0x0E_02_00, mask: 0xFF_FF_00 }],
    probe: crate::uvc::class_probe,
    detach: Some(crate::uvcenv::class_detach),
    ctx: 0,
}];
/// CONFIG_USB_VIDEO_CLASS=m or n: video interfaces go to the uvc module (or nobody).
#[cfg(feature = "uvc-module")]
static BUILTIN: [Driver; 0] = [];

static MODULES: Mutex<Vec<Driver>> = Mutex::new(Vec::new());
/// Configured functions that matched a driver's ids but were declined by every driver.
static UNCLAIMED: Mutex<Vec<(Match, UsbFunction)>> = Mutex::new(Vec::new());

fn walk(cfg: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut i = 0usize;
    core::iter::from_fn(move || {
        if i + 2 > cfg.len() { return None; }
        let len = cfg[i] as usize;
        if len < 2 || i + len > cfg.len() { return None; }
        let d = &cfg[i..i + len];
        i += len;
        Some(d)
    })
}

fn drivers() -> Vec<Driver> {
    let mut v: Vec<Driver> = BUILTIN.to_vec();
    v.extend(MODULES.lock().iter().copied());
    v
}

/// The first interface of `cfg` that `d` wants.
fn find(d: &Driver, cfg: &[u8], vid: u16, pid: u16, bus: u8) -> Option<Match> {
    walk(cfg).filter(|x| x.len() >= 9 && x[1] == 4).find_map(|x| {
        let intf = (x[5] as u32) << 16 | (x[6] as u32) << 8 | x[7] as u32;
        d.ids.iter().any(|e| e.matches(vid, pid, intf))
            .then_some(Match { bus, vid, pid, class: x[5], subclass: x[6], protocol: x[7], iface: x[2] })
    })
}

/// Whether some class driver wants an interface of this device; the class byte of that interface.
pub fn wants(cfg: &[u8], vid: u16, pid: u16) -> Option<u8> {
    drivers().iter().find_map(|d| find(d, cfg, vid, pid, 0)).map(|m| m.class)
}

fn try_drivers(drv: &[Driver], mut f: UsbFunction, m0: Match) -> Result<(), (UsbFunction, bool)> {
    let mut matched = false;
    for d in drv {
        let Some(m) = find(d, &f.cfg, m0.vid, m0.pid, m0.bus) else { continue };
        matched = true;
        match (d.probe)(f, &m, d.ctx) {
            Ok(()) => return Ok(()),
            Err(back) => f = back,
        }
    }
    Err((f, matched))
}

/// What happened to an offered function.
pub enum Offer {
    /// A driver took it.
    Claimed,
    /// Matched drivers declined it; it is kept for drivers registered later.
    Kept,
    /// No driver wants it.
    NoMatch(UsbFunction),
}

/// Offer a freshly configured function to the class drivers. Task context.
pub fn offer(f: UsbFunction, bus: u8, vid: u16, pid: u16) -> Offer {
    let key = (&*f.hc as *const _ as usize, f.slot);
    GONE.lock().retain(|g| *g != key); // a new function on this slot: the old departure no longer applies
    let m0 = Match { bus, vid, pid, ..Default::default() };
    match try_drivers(&drivers(), f, m0) {
        Ok(()) => Offer::Claimed,
        Err((f, true)) => {
            UNCLAIMED.lock().push((m0, f));
            Offer::Kept
        }
        Err((f, false)) => Offer::NoMatch(f),
    }
}

/// Add a runtime class driver and replay the unclaimed functions to it. Task context.
pub fn register(d: Driver) {
    MODULES.lock().push(d);
    let pending: Vec<(Match, UsbFunction)> = core::mem::take(&mut *UNCLAIMED.lock());
    for (m0, f) in pending {
        let one = [d];
        match try_drivers(&one, f, m0) {
            Ok(()) => {}
            Err((f, _)) => UNCLAIMED.lock().push((m0, f)),
        }
    }
}

/// Remove a module's class drivers (it is unloading). Probes already running finish; the gate of the
/// module refuses any that start later.
pub fn unregister_owner(owner: KmodId) { MODULES.lock().retain(|d| d.owner != owner); }

/// Keep a function whose driver went away for drivers registered later (reload re-probes it).
/// False (function dropped) when its device already left: the hot-unplug purge ran earlier, and
/// the departure record is checked under the same lock the purge takes.
pub fn requeue(m: Match, f: UsbFunction) -> bool {
    let key = (&*f.hc as *const _ as usize, f.slot);
    let gone = GONE.lock();
    if gone.contains(&key) { return false; }
    UNCLAIMED.lock().push((m, f));
    drop(gone);
    true
}

/// Devices that left (hc address, slot) since their slot was last offered a function.
static GONE: Mutex<Vec<(usize, u8)>> = Mutex::new(Vec::new());

/// An unload that was refused (EBUSY) reopened the module's gate: offer it the functions whose
/// probe was declined while the gate was closed.
pub fn reoffer_owner(owner: KmodId) {
    let Some(d) = MODULES.lock().iter().find(|d| d.owner == owner).copied() else { return };
    let pending: Vec<(Match, UsbFunction)> = core::mem::take(&mut *UNCLAIMED.lock());
    for (m0, f) in pending {
        let one = [d];
        match try_drivers(&one, f, m0) {
            Ok(()) => {}
            Err((f, _)) => UNCLAIMED.lock().push((m0, f)),
        }
    }
}

/// Asked once per interface that no class driver wants yet: vid, pid, class, subclass, protocol.
/// The kernel's module autoloader installs it; a module it loads registers its driver from `init`.
pub type AutoloadFn = fn(u16, u16, u8, u8, u8);
static AUTOLOAD: Mutex<Option<AutoloadFn>> = Mutex::new(None);

pub fn set_autoload(f: AutoloadFn) { *AUTOLOAD.lock() = Some(f); }

/// Like [`wants`], but when nobody wants the device yet the autoload hook gets to load a driver
/// module for each interface first (alias match at enumerate). Task context, no class locks held.
pub fn wants_or_load(cfg: &[u8], vid: u16, pid: u16) -> Option<u8> {
    if let Some(c) = wants(cfg, vid, pid) { return Some(c); }
    let hook = *AUTOLOAD.lock();
    let f = hook?;
    for x in walk(cfg).filter(|x| x.len() >= 9 && x[1] == 4) { f(vid, pid, x[5], x[6], x[7]); }
    wants(cfg, vid, pid)
}

/// Functions kept for late drivers (for tests and diagnostics).
pub fn unclaimed_count() -> usize { UNCLAIMED.lock().len() }

/// The device on (`hc`, `slot`) left: drop it from the unclaimed list and tell every driver.
pub fn detach(hc: &crate::xhci::Hc, slot: u8) {
    {
        let mut gone = GONE.lock();
        let key = (hc as *const _ as usize, slot);
        if !gone.contains(&key) { gone.push(key); }
        UNCLAIMED.lock().retain(|(_, f)| !(core::ptr::eq(&*f.hc, hc) && f.slot == slot));
    }
    for d in drivers() {
        if let Some(h) = d.detach { h(hc, slot, d.ctx); }
    }
}

// ── tick callbacks ──────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct TickCb { owner: KmodId, f: fn(usize), ctx: usize }

static TICKS: Mutex<Vec<TickCb>> = Mutex::new(Vec::new());

/// Run `f(ctx)` on every USB timer tick (IRQ context: must not block).
pub fn tick_register(owner: KmodId, f: fn(usize), ctx: usize) {
    TICKS.lock().push(TickCb { owner, f, ctx });
}

pub fn tick_count() -> usize { TICKS.lock().len() }

static TICK_RUNNING: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Remove every tick callback of `owner` and wait until no CPU is still inside `tick()` with a
/// copy of the old list (failed module init: the code is about to be unmapped). Returns the
/// `ctx` values removed so the caller can free its trampoline boxes.
pub fn tick_unregister_owner(owner: KmodId) -> usize {
    let n = { let mut g = TICKS.lock(); let b = g.len(); g.retain(|c| c.owner != owner); b - g.len() };
    while TICK_RUNNING.load(core::sync::atomic::Ordering::Acquire) != 0 { core::hint::spin_loop(); }
    n
}

/// USB timer tick (IRQ context). A tick that finds the list busy is skipped.
pub fn tick() {
    TICK_RUNNING.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
    let Some(g) = TICKS.try_lock() else { TICK_RUNNING.fetch_sub(1, core::sync::atomic::Ordering::AcqRel); return };
    // Copy out under the lock: callbacks are few and the list only grows.
    let mut buf = [None::<TickCb>; 8];
    for (i, c) in g.iter().take(8).enumerate() { buf[i] = Some(*c); }
    drop(g);
    for c in buf.iter().flatten() { let _ = c.owner; (c.f)(c.ctx); }
    TICK_RUNNING.fetch_sub(1, core::sync::atomic::Ordering::AcqRel);
}

/// Leak a slice so a driver's id table can be `'static`.
pub fn leak_ids(v: Vec<IdEntry>) -> &'static [IdEntry] { Box::leak(v.into_boxed_slice()) }
