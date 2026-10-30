//! Module registry and owner tags. Entries live as long as the module (no unload in v1)
//! except a failed init that registered nothing, which is dropped so the load can be retried.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, Ordering};
use kmod_abi::{KmodId, RegKind};
use spin::Mutex;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State { Coming, Live, Failed }

#[derive(Clone, Copy)]
pub struct ModInfo {
    pub id: KmodId,
    pub name: [u8; 32],
    pub base: usize,
    pub size: usize,
    pub state: State,
    pub regs: u32,
}

static ENTRIES: Mutex<Vec<ModInfo>> = Mutex::new(Vec::new());
static REGS: Mutex<Vec<(KmodId, RegKind, u64)>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU16 = AtomicU16::new(1);

fn name_eq(a: &[u8; 32], b: &[u8; 32]) -> bool { a == b }

/// Insert a `Coming` entry. None if the name is taken or the id space is exhausted.
pub fn add(name: [u8; 32], base: usize, size: usize) -> Option<KmodId> {
    let mut e = ENTRIES.lock();
    if e.iter().any(|m| name_eq(&m.name, &name)) { return None; }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    if id == 0 || id == u16::MAX { return None; }
    let id = KmodId(id);
    e.push(ModInfo { id, name, base, size, state: State::Coming, regs: 0 });
    Some(id)
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
        RegKind::TICK => { usb::class::tick_unregister_owner(owner); true }
        RegKind::PM => { super::exports::pm_unregister_owner(owner); true }
        RegKind::BACKLIGHT => { super::exports::backlight_unregister_owner(owner); true }
        RegKind::V4L2 => { crate::syscall::v4l2::unregister_camera(handle as usize); true }
        _ => false,
    }
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
