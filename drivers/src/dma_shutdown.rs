//! Bus-master shutdown hooks for kexec (Linux `.shutdown` for DMA engines).
//!
//! A kexec jumps into a kernel that treats every page outside its own image
//! as free memory. Any device still running DMA at the jump keeps writing
//! into pages the next kernel hands to its buddy allocator (Pi 5, 2026-10-27:
//! live mDNS frames from the RP1 GEM RX ring landed on buddy free-list links
//! and in epoll's `EpCache`). Every driver that programs a bus master
//! registers one hook here at attach time; `kernel/src/power.rs` runs them
//! with interrupts off, after the last disk write and before the jump.
//!
//! Two stages: `Device` hooks stop one engine each (rings, channels,
//! controllers) while the bus still answers; `Bus` hooks then take the
//! whole interconnect down (PCIe PERST# + bridge reset) so nothing behind
//! it can master again. Hooks must be bounded, lock-free (or `try_lock`)
//! and must not allocate in a way that can block.

use core::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    /// Stop one DMA engine; the bus it sits on still works.
    Device,
    /// Reset or isolate a bus; runs after every `Device` hook.
    Bus,
}

/// A hook returns true when its device is now quiet.
pub type Hook = fn() -> bool;

#[derive(Clone, Copy)]
struct Entry { name: &'static str, stage: Stage, hook: Hook, ext: Option<(fn(usize) -> bool, usize)>, owner: u16 }

impl Entry {
    fn call(&self) -> bool { match self.ext { Some((f, c)) => f(c), None => (self.hook)() } }
}

fn never() -> bool { true }

const MAX: usize = 16;
static ENTRIES: spin::Mutex<[Option<Entry>; MAX]> = spin::Mutex::new([None; MAX]);
static COUNT: AtomicUsize = AtomicUsize::new(0);

/// Registers `hook` under `name` (shown in the kexec log line). A second
/// registration of the same name replaces the first. Silently dropped when
/// the table is full (16 entries).
pub fn register(name: &'static str, stage: Stage, hook: Hook) {
    let mut t = ENTRIES.lock();
    if let Some(e) = t.iter_mut().flatten().find(|e| e.name == name) {
        *e = Entry { name, stage, hook, ext: None, owner: 0 };
        return;
    }
    if let Some(slot) = t.iter_mut().find(|e| e.is_none()) {
        *slot = Some(Entry { name, stage, hook, ext: None, owner: 0 });
        COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

/// Module registration: the hook gets a context word and is removed by [`unregister_owner`].
/// False when the table is full or the name is taken.
pub fn register_ext(owner: u16, name: &'static str, stage: Stage, hook: fn(usize) -> bool, ctx: usize) -> bool {
    let mut t = ENTRIES.lock();
    if t.iter().flatten().any(|e| e.name == name) { return false; }
    if let Some(slot) = t.iter_mut().find(|e| e.is_none()) {
        *slot = Some(Entry { name, stage, hook: never, ext: Some((hook, ctx)), owner });
        COUNT.fetch_add(1, Ordering::Relaxed);
        true
    } else { false }
}

/// Remove every hook `owner` registered. Returns how many went.
pub fn unregister_owner(owner: u16) -> usize {
    let mut t = ENTRIES.lock();
    let mut n = 0;
    for slot in t.iter_mut() {
        if matches!(slot, Some(e) if e.owner == owner && owner != 0) { *slot = None; n += 1; }
    }
    COUNT.fetch_sub(n, Ordering::Relaxed);
    n
}

/// Drop the hook registered under `name` (kexec bisect knob `kexec.skip=`). True if one went.
pub fn remove(name: &str) -> bool {
    let mut t = ENTRIES.lock();
    for slot in t.iter_mut() {
        if matches!(slot, Some(e) if e.name == name) { *slot = None; COUNT.fetch_sub(1, Ordering::Relaxed); return true; }
    }
    false
}

/// Is a hook registered under `name`? (ktest)
pub fn has(name: &str) -> bool { ENTRIES.lock().iter().flatten().any(|e| e.name == name) }

/// Number of registered hooks.
pub fn count() -> usize { COUNT.load(Ordering::Relaxed) }

/// Runs every hook of `stage` in registration order. Returns the names of
/// the devices that reported quiet and those that did not.
pub fn run(stage: Stage) -> (alloc::vec::Vec<&'static str>, alloc::vec::Vec<&'static str>) {
    let (mut ok, mut bad) = (alloc::vec::Vec::new(), alloc::vec::Vec::new());
    // Other CPUs may still be running (a late `register`): spin briefly
    // instead of silently skipping every hook.
    let mut tries = 0u32;
    let t = loop {
        if let Some(t) = ENTRIES.try_lock() { break t; }
        tries += 1;
        if tries > 2_000_000 {
            crate::pci::serial_debug("[kexec] dma_shutdown registry busy\n");
            return (ok, bad);
        }
        core::hint::spin_loop();
    };
    let list: [Option<Entry>; MAX] = *t;
    drop(t);
    for e in list.iter().flatten().filter(|e| e.stage == stage) {
        if e.call() { ok.push(e.name) } else { bad.push(e.name) }
    }
    (ok, bad)
}

/// Runs every hook of `stage` with no allocation and no waiting on locks.
/// For the last CPU of a kexec after every other CPU is parked: a parked
/// CPU may have been stopped holding the registry lock, so it is forced.
/// `report(name, quiet)` is called once per hook.
pub fn run_parked(stage: Stage, mut report: impl FnMut(&'static str, bool)) {
    // SAFETY: the caller is the only CPU still running.
    unsafe { ENTRIES.force_unlock(); }
    let list: [Option<Entry>; MAX] = *ENTRIES.lock();
    for e in list.iter().flatten().filter(|e| e.stage == stage) {
        report(e.name, e.call());
    }
}
