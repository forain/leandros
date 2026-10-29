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
}

/// Probe: return true to claim the device.
pub type ProbeFn = fn(&str, &PlatDev) -> bool;

struct Dev { compat: String, dev: PlatDev, claimed: bool }
struct Drv { compats: &'static [&'static str], probe: ProbeFn }

static DEVS: Mutex<Vec<Dev>> = Mutex::new(Vec::new());
static DRVS: Mutex<Vec<Drv>> = Mutex::new(Vec::new());
static HOOK: AtomicUsize = AtomicUsize::new(0);

/// Install the unclaimed-compatible autoload hook (`kmod::autoload::of_hook`).
pub fn set_autoload(f: fn(&str)) { HOOK.store(f as usize, Ordering::Relaxed); }

fn find_probe(compat: &str) -> Option<ProbeFn> {
    DRVS.lock().iter().find(|d| d.compats.iter().any(|c| *c == compat)).map(|d| d.probe)
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
            g.push(Dev { compat: String::from(compat), dev, claimed: false });
            g.len() - 1
        }
    };
    if try_claim(idx) { return true; }
    let h = HOOK.load(Ordering::Relaxed);
    if h != 0 {
        let f: fn(&str) = unsafe { core::mem::transmute(h) };
        f(compat);
    }
    DEVS.lock().get(idx).map_or(false, |e| e.claimed)
}

fn try_claim(idx: usize) -> bool {
    let (compat, dev) = {
        let g = DEVS.lock();
        match g.get(idx) { Some(e) if !e.claimed => (e.compat.clone(), e.dev), _ => return false }
    };
    let Some(probe) = find_probe(&compat) else { return false };
    if probe(&compat, &dev) {
        if let Some(e) = DEVS.lock().get_mut(idx) { e.claimed = true; }
        true
    } else { false }
}

/// Register a driver; replays every unclaimed announced device it matches.
pub fn register(compats: &'static [&'static str], probe: ProbeFn) {
    DRVS.lock().push(Drv { compats, probe });
    let n = DEVS.lock().len();
    for i in 0..n { try_claim(i); }
}

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
