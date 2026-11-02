//! Module registry and owner tags. Entries live as long as the module (no unload in v1)
//! except a failed init that registered nothing, which is dropped so the load can be retried.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, Ordering};
use kmod_abi::{KmodId, RegKind};
use spin::Mutex;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State { Coming, Live, Failed, Going, Zombie }

#[derive(Clone, Copy)]
pub struct ModInfo {
    pub id: KmodId,
    pub name: [u8; 32],
    pub base: usize,
    pub size: usize,
    pub state: State,
    pub regs: u32,
    /// Header `exit` VA (0 = none) and whether the image was built unloadable.
    pub exit: usize,
    pub unloadable: bool,
}

static ENTRIES: Mutex<Vec<ModInfo>> = Mutex::new(Vec::new());
static REGS: Mutex<Vec<(KmodId, RegKind, u64)>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU16 = AtomicU16::new(1);

fn name_eq(a: &[u8; 32], b: &[u8; 32]) -> bool { a == b }

/// Insert a `Coming` entry. None if the name is taken or the id space is exhausted.
pub fn add(name: [u8; 32], base: usize, size: usize, exit: usize, unloadable: bool) -> Option<KmodId> {
    let mut e = ENTRIES.lock();
    if e.iter().any(|m| name_eq(&m.name, &name)) { return None; }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    if id == 0 || id == u16::MAX { return None; }
    let id = KmodId(id);
    e.push(ModInfo { id, name, base, size, state: State::Coming, regs: 0, exit, unloadable });
    Some(id)
}

pub fn find_by_name(name: &[u8; 32]) -> Option<ModInfo> { ENTRIES.lock().iter().find(|m| name_eq(&m.name, name)).copied() }

/// Atomically Live -> Going. Err(state) when the module is in any other state.
pub fn begin_unload(id: KmodId) -> Result<(), State> {
    let mut e = ENTRIES.lock();
    match e.iter_mut().find(|m| m.id == id) {
        Some(m) if m.state == State::Live => { m.state = State::Going; Ok(()) }
        Some(m) => Err(m.state),
        None => Err(State::Failed),
    }
}

/// Pages (va, phys) of a loaded image, kept for the slot free.
static PAGES: Mutex<Vec<(KmodId, Vec<(usize, usize)>)>> = Mutex::new(Vec::new());
pub fn set_pages(id: KmodId, p: Vec<(usize, usize)>) { PAGES.lock().push((id, p)); }
pub fn take_pages(id: KmodId) -> Vec<(usize, usize)> {
    let mut g = PAGES.lock();
    match g.iter().position(|x| x.0 == id) { Some(i) => g.remove(i).1, None => Vec::new() }
}

/// Drop every remaining registration record of a generation (their gates are closed for good).
pub fn forget_regs(id: KmodId) { REGS.lock().retain(|r| r.0 != id); }

/// Kinds left behind that are not gated: nothing may be freed while one exists.
pub fn ungated_left(id: KmodId) -> usize {
    REGS.lock().iter().filter(|r| r.0 == id && r.1 != RegKind::KTHREAD && r.1 != RegKind::TICK).count()
}

pub fn name_exists(name: &[u8; 32]) -> bool { ENTRIES.lock().iter().any(|m| name_eq(&m.name, name)) }

pub fn set_state(id: KmodId, s: State) {
    if let Some(m) = ENTRIES.lock().iter_mut().find(|m| m.id == id) { m.state = s; }
}

/// Drop an entry and its (empty) registration set.
pub fn remove(id: KmodId) {
    ENTRIES.lock().retain(|m| m.id != id);
    REGS.lock().retain(|r| r.0 != id);
}

pub fn registrations_of_kind(id: KmodId, k: RegKind) -> usize { REGS.lock().iter().filter(|r| r.0 == id && r.1 == k).count() }

pub fn reg_count(id: KmodId) -> usize { REGS.lock().iter().filter(|r| r.0 == id).count() }

pub fn name_of(id: KmodId) -> [u8; 32] {
    ENTRIES.lock().iter().find(|m| m.id == id).map_or([0; 32], |m| m.name)
}

pub fn for_each(f: &mut dyn FnMut(&ModInfo)) {
    let snap: Vec<ModInfo> = {
        let regs = REGS.lock();
        ENTRIES.lock().iter().map(|m| {
            let mut m = *m;
            m.regs = regs.iter().filter(|r| r.0 == m.id).count() as u32;
            m
        }).collect()
    };
    for m in &snap { f(m); }
}

/// Record that `owner` registered `handle` of `kind` (consumer lanes call this).
pub fn note_registration(owner: KmodId, kind: RegKind, handle: u64) {
    let owner = super::gate::owner(owner);
    REGS.lock().push((owner, kind, handle));
}

pub fn registrations(owner: KmodId, f: &mut dyn FnMut(RegKind, u64)) {
    let snap: Vec<(RegKind, u64)> = REGS.lock().iter().filter(|r| r.0 == owner).map(|r| (r.1, r.2)).collect();
    for (k, h) in snap { f(k, h); }
}

/// Release one registration whose callback can be taken back. False = the kind has no unregister
/// path (irq, netdev, hwrng, kthread, usb class): the slot must then stay mapped.
fn release_one(owner: KmodId, kind: RegKind, handle: u64) -> bool {
    match kind {
        RegKind::TICK => {
            // kmod.leaktick=1 (test knob): leave the registration behind as a dead gated stub.
            if super::autoload::opt("kmod.leaktick") == Some("1") { return true; }
            usb::class::tick_unregister_owner(owner); true
        }
        RegKind::PM => { super::exports::pm_unregister_owner(owner); true }
        RegKind::BACKLIGHT => { super::exports::backlight_unregister_owner(owner); true }
        RegKind::PLAT => { super::exports::plat_unregister_owner(owner); true }
        RegKind::DMAQ => { super::exports::dma_quiesce_unregister_owner(owner); true }
        RegKind::V4L2 => { crate::syscall::v4l2::unregister_camera(handle as usize); true }
        RegKind::IRQ => super::lower::release_irq(owner, handle),
        RegKind::HRTIMER => super::lower::release_hrtimer(handle),
        RegKind::HWRNG => super::exports::hwrng_release(handle),
        RegKind::USB_CLASS => true, // torn down by `pre_exit`; the record is leaked (a snapshot may still point at it)
        RegKind::BLK => { drivers::blkdev::remove_disk(handle as usize); true }
        RegKind::INPUT => super::exports::input_release(handle),
        RegKind::SND => super::exports::snd_release(),
        // The netif entry stays (name kept); its gate is closed for good, so nothing reaches the module.
        RegKind::NETDEV => true,
        _ => false,
    }
}

/// D6/D9: why `id` cannot unload, evaluated AFTER the Upper gate drained (so no open, probe or I/O
/// of this generation is in flight). Open /dev/video*, mounted block devices and DRM pin the module;
/// netdev, input, hwrng and USB class detach instead.
pub fn busy_reason(id: KmodId) -> Option<&'static str> {
    let recs: Vec<(RegKind, u64)> = REGS.lock().iter().filter(|r| r.0 == id).map(|r| (r.1, r.2)).collect();
    for (k, h) in recs {
        match k {
            RegKind::V4L2 => { if crate::syscall::v4l2::open_by(id) { return Some("camera open"); } }
            RegKind::BLK => {
                let mounts = vfs_server::list_mounts();
                for p in drivers::blkdev::family_paths(h as usize) {
                    if vfs_server::mounting_device(p) { return Some("mounting"); }
                    let want = p.strip_prefix("/dev/").unwrap_or(p);
                    if mounts.iter().any(|m| m.in_use && m.device.strip_prefix("/dev/").unwrap_or(m.device) == want) { return Some("mounted"); }
                }
            }
            RegKind::SND => { if super::exports::snd_busy() { return Some("sound open"); } }
            RegKind::DRM | RegKind::KMS => return Some("display in use"),
            _ => {}
        }
    }
    None
}

/// Runs on the exit thread right before the module's `exit()`: take back what needs the module's own code.
pub fn pre_exit(id: KmodId) {
    let recs: Vec<(RegKind, u64)> = REGS.lock().iter().filter(|r| r.0 == id).map(|r| (r.1, r.2)).collect();
    for (k, h) in recs {
        if k == RegKind::USB_CLASS { super::exports::usb_class_teardown(h); }
        // The sound backend must be unreachable before exit() frees its DMA (pipewire's tick would write into it).
        if k == RegKind::SND { super::exports::snd_release(); }
    }
}

/// A refused unload reopened the Upper gate: replay what arrived while it was closed.
pub fn reopened(id: KmodId) {
    let recs: Vec<(RegKind, u64)> = REGS.lock().iter().filter(|r| r.0 == id).map(|r| (r.1, r.2)).collect();
    for (k, h) in recs { if k == RegKind::USB_CLASS { super::exports::usb_class_reopened(h); } }
}

/// Unload step 2: tell consumers the module's upper-edge objects are going (netifs record what a reload needs).
pub fn unbind_upper(owner: KmodId) {
    let recs: Vec<(RegKind, u64)> = REGS.lock().iter().filter(|r| r.0 == owner).map(|r| (r.1, r.2)).collect();
    for (k, h) in recs { if k == RegKind::NETDEV { super::exports::netdev_detach(h); } }
}

/// Both unload drains succeeded: netifs become rebindable.
pub fn commit_upper(owner: KmodId) {
    let recs: Vec<u64> = REGS.lock().iter().filter(|r| r.0 == owner && r.1 == RegKind::NETDEV).map(|r| r.2).collect();
    for h in recs { super::exports::netdev_commit_detach(h); }
}

/// Unload gave up (Zombie): pending netif detaches are dropped, the netif stays bound to the zombie.
pub fn abort_upper(owner: KmodId) {
    let recs: Vec<u64> = REGS.lock().iter().filter(|r| r.0 == owner && r.1 == RegKind::NETDEV).map(|r| r.2).collect();
    for h in recs { super::exports::netdev_abort_detach(h); }
}

/// Failed init: unregister every registration that can be taken back, dropping its record.
/// True when none is left, i.e. nothing can call into the module and its slot may be freed.
pub fn release_all(owner: KmodId) -> bool {
    let recs: Vec<(RegKind, u64)> = REGS.lock().iter().filter(|r| r.0 == owner).map(|r| (r.1, r.2)).collect();
    for (k, h) in recs {
        if release_one(owner, k, h) {
            let mut r = REGS.lock();
            if let Some(i) = r.iter().position(|x| x.0 == owner && x.1 == k && x.2 == h) { r.remove(i); }
        }
    }
    reg_count(owner) == 0
}

/// D9 last gate: after the Upper drain, stop new /dev/pipewire opens and re-check that none slipped in.
/// False = busy (the caller backs out as for busy_reason). Sound opens bypass the Upper gate, so this is
/// the sound equivalent of the closed-gate re-check.
pub fn seal_sound(id: KmodId) -> bool {
    let has = REGS.lock().iter().any(|r| r.0 == id && r.1 == RegKind::SND);
    !has || super::exports::snd_seal()
}
pub fn unseal_sound() { super::exports::snd_unseal(); }
