//! Built-in platform-device registry (bool). Devices are announced by the DT walker
//! ([`scan_dt`]) and by board code; drivers (built-in or modules through `kapi_plat_register`)
//! register a `compatible` list and a probe. First claim wins; registering replays the devices
//! nobody has claimed yet. An announced device that stays unclaimed calls the autoload hook
//! (`modules.alias` `of` lines), exactly like the USB class registry.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::Mutex;

/// Resources of an announced platform device. Board announcements fill what the kernel computes.
#[derive(Clone, Copy, Debug, Default)]
pub struct PlatDev {
    pub base: u64,
    pub size: u64,
    pub irq: u32,
    pub dma_offset: u64,
    pub flags: u32,
    /// Extra register ranges (`regs[0]` empty = use `base`/`size`) and interrupt lines (`kmod_abi::KPlatDev` shape).
    pub regs: [(u64, u64); 4],
    pub irqs: [u32; 4],
    pub nirq: u32,
    pub dt_node: u32,
}

/// Probe: return true to claim the device.
pub type ProbeFn = fn(&str, &PlatDev) -> bool;
/// Probe with a context word (module registrations): `(ctx, compat, dev)`.
pub type ExtProbeFn = fn(usize, &str, &PlatDev) -> bool;

#[derive(Clone, Copy)]
enum Probe { Builtin(ProbeFn), Ext(ExtProbeFn, usize) }

/// Owner id 0 = built-in driver.
struct Dev { compat: String, dev: PlatDev, claimed: bool, claimed_by: u16, hooked: bool }
struct Drv { compats: Vec<String>, probe: Probe, owner: u16 }

static DEVS: Mutex<Vec<Dev>> = Mutex::new(Vec::new());
static DRVS: Mutex<Vec<Drv>> = Mutex::new(Vec::new());
static HOOK: AtomicUsize = AtomicUsize::new(0);

/// Install the unclaimed-compatible autoload hook (`kmod::autoload::of_hook`).
/// Raw swap of the autoload hook (0 = none) without a replay; returns the previous one. For ktest.
pub fn swap_autoload(f: usize) -> usize { HOOK.swap(f, Ordering::Relaxed) }

/// Devices announced before the hook existed (board code runs before it) are replayed to it now.
pub fn set_autoload(f: fn(&str)) {
    HOOK.store(f as usize, Ordering::Relaxed);
    replay_unclaimed();
}

/// Offer every announced, unclaimed device that never reached the autoload hook to a driver, then to the hook.
pub fn replay_unclaimed() {
    let n = DEVS.lock().len();
    for idx in 0..n {
        if try_claim(idx) { continue; }
        let compat = {
            let mut g = DEVS.lock();
            match g.get_mut(idx) {
                Some(e) if !e.claimed && !e.hooked => { e.hooked = true; e.compat.clone() }
                _ => continue,
            }
        };
        run_hook(&compat);
    }
}

fn run_hook(compat: &str) {
    let h = HOOK.load(Ordering::Relaxed);
    if h != 0 {
        let f: fn(&str) = unsafe { core::mem::transmute(h) };
        f(compat);
    }
}

fn find_probe(compat: &str) -> Option<(Probe, u16)> {
    DRVS.lock().iter().find(|d| d.compats.iter().any(|c| c == compat)).map(|d| (d.probe, d.owner))
}

/// Announce a device. Returns true if a registered driver claimed it; otherwise the autoload
/// hook runs (a module that registers from its init then claims it through the replay).
pub fn announce(compat: &str, dev: PlatDev) -> bool {
    let idx = {
        let mut g = DEVS.lock();
        // Board announcements override DT resources for the same compatible.
        if let Some(e) = g.iter_mut().find(|e| e.compat == compat) {
            if e.claimed { return true; }
            e.dev = dev;
            g.iter().position(|e| e.compat == compat).unwrap()
        } else {
            g.push(Dev { compat: String::from(compat), dev, claimed: false, claimed_by: 0, hooked: false });
            g.len() - 1
        }
    };
    if try_claim(idx) { return true; }
    if HOOK.load(Ordering::Relaxed) != 0 {
        if let Some(e) = DEVS.lock().get_mut(idx) { e.hooked = true; }
        run_hook(compat);
    }
    DEVS.lock().get(idx).map_or(false, |e| e.claimed)
}

fn try_claim(idx: usize) -> bool {
    let (compat, dev) = {
        let g = DEVS.lock();
        match g.get(idx) { Some(e) if !e.claimed => (e.compat.clone(), e.dev), _ => return false }
    };
    let Some((probe, owner)) = find_probe(&compat) else { return false };
    let ok = match probe { Probe::Builtin(f) => f(&compat, &dev), Probe::Ext(f, ctx) => f(ctx, &compat, &dev) };
    if ok {
        if let Some(e) = DEVS.lock().get_mut(idx) { e.claimed = true; e.claimed_by = owner; }
        true
    } else { false }
}

/// Register a driver; replays every unclaimed announced device it matches.
pub fn register(compats: &'static [&'static str], probe: ProbeFn) {
    DRVS.lock().push(Drv { compats: compats.iter().map(|c| String::from(*c)).collect(), probe: Probe::Builtin(probe), owner: 0 });
    let n = DEVS.lock().len();
    for i in 0..n { try_claim(i); }
}

/// Register a module driver (`owner` != 0) with a context word; replays unclaimed devices like [`register`].
pub fn register_ext(owner: u16, compats: Vec<String>, probe: ExtProbeFn, ctx: usize) {
    DRVS.lock().push(Drv { compats, probe: Probe::Ext(probe, ctx), owner });
    let n = DEVS.lock().len();
    for i in 0..n { try_claim(i); }
}

/// Drop every driver `owner` registered and unclaim the devices it claimed (module removal / failed init).
/// Returns the context words of the dropped registrations.
pub fn unregister_owner(owner: u16) -> Vec<usize> {
    let mut ctxs = Vec::new();
    DRVS.lock().retain(|d| {
        if d.owner != owner { return true; }
        if let Probe::Ext(_, c) = d.probe { ctxs.push(c); }
        false
    });
    for e in DEVS.lock().iter_mut() {
        if e.claimed && e.claimed_by == owner { e.claimed = false; e.claimed_by = 0; e.hooked = false; }
    }
    ctxs
}

/// Is the device with this `compatible` announced and claimed? (ktest)
pub fn is_claimed(compat: &str) -> Option<bool> { DEVS.lock().iter().find(|e| e.compat == compat).map(|e| e.claimed) }

/// Number of announced devices and of unclaimed ones (diagnostics, `kmodctl`).
pub fn counts() -> (usize, usize) {
    let g = DEVS.lock();
    (g.len(), g.iter().filter(|d| !d.claimed).count())
}

/// Announce every `compatible` string of every enabled DT node. Run once before `autoload::boot`.
/// The first string is the node's most specific one; every string is announced.
pub fn scan_dt() -> usize {
    let mut found: Vec<(String, PlatDev)> = Vec::new();
    unsafe {
        boot::device_tree::for_each_compatible(&mut |n| {
            let (base, size) = n.reg.unwrap_or((0, 0));
            for c in n.compat.split(|&b| b == 0).filter(|c| !c.is_empty()) {
                if let Ok(s) = core::str::from_utf8(c) {
                    found.push((String::from(s), PlatDev { base, size, ..PlatDev::default() }));
                }
            }
        });
    }
    let n = found.len();
    for (c, d) in found { announce(&c, d); }
    n
}

/// True when any node of the flattened tree carries `compat`, whatever its `status` (the Pi 5 sdio2
/// `wifi@1` child is invisible to `scan_dt`, which walks enabled nodes only).
pub fn fdt_has_compat(dtb: &[u8], compat: &str) -> bool {
    let be = |o: usize| dtb.get(o..o + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]));
    if be(0) != Some(0xd00d_feed) { return false; }
    let (Some(st), Some(strs)) = (be(8), be(12)) else { return false };
    let (st, strs) = (st as usize, strs as usize);
    let end = (st + be(36).unwrap_or(0) as usize).min(dtb.len());
    let mut o = st;
    while o + 4 <= end {
        let Some(tok) = be(o) else { return false };
        o += 4;
        match tok {
            1 => {
                let Some(n) = dtb.get(o..end).and_then(|s| s.iter().position(|&c| c == 0)) else { return false };
                o = (o + n + 1 + 3) & !3;
            }
            3 => {
                let (Some(len), Some(noff)) = (be(o), be(o + 4)) else { return false };
                let len = len as usize;
                let Some(val) = dtb.get(o + 8..o + 8 + len) else { return false };
                o = (o + 8 + len + 3) & !3;
                let Some(ns) = dtb.get(strs + noff as usize..) else { return false };
                let nl = ns.iter().position(|&c| c == 0).unwrap_or(ns.len());
                if &ns[..nl] == b"compatible" && val.split(|&c| c == 0).any(|c| c == compat.as_bytes()) { return true; }
            }
            2 | 4 => {}
            _ => break,
        }
    }
    false
}

/// Announce `compat` when the boot DTB has such a node (any status). Run before `scan_dt`.
pub fn announce_if_in_dtb(compat: &str) -> bool {
    let Some(dtb) = (unsafe { boot::device_tree::recorded_dtb() }) else { return false };
    if !fdt_has_compat(dtb, compat) { return false; }
    announce(compat, PlatDev::default());
    true
}

/// Announce `compat` unconditionally (board knowledge, e.g. kexec DTB lacks the node).
/// Returns true if the DTB had it too.
pub fn announce_board(compat: &str) -> bool {
    let in_dtb = unsafe { boot::device_tree::recorded_dtb() }.map_or(false, |d| fdt_has_compat(d, compat));
    announce(compat, PlatDev::default());
    in_dtb
}
