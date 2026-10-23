//! Per-thread fd-table overrides (unshare(CLONE_FILES), CLOSE_RANGE_UNSHARE).
//!
//! A thread's descriptors normally live in its process's table, named by the
//! tgid. A thread that unshared its table gets a private id (`FD_KEY_PRIVATE
//! | n`), recorded here against its tid. Readers are lock-free (the VFS
//! resolves a table id on every fd operation); removal never moves entries.

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

pub const FD_KEY_PRIVATE: u32 = 0x8000_0000;
const SLOTS: usize = 64;

/// `(tid << 32) | key`, 0 = free.
static TID_KEY: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
/// Owning tgid of the slot.
static TGID: [AtomicU32; SLOTS] = [const { AtomicU32::new(0) }; SLOTS];
static HWM: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// Serialises writers only; a leaf lock.
static WRITE: spin::Mutex<()> = spin::Mutex::new(());
static NEXT: AtomicU32 = AtomicU32::new(1);

/// Does any thread have a private table?
#[inline]
pub fn any() -> bool { LIVE.load(Ordering::Acquire) != 0 }

pub fn alloc_key() -> u32 {
    FD_KEY_PRIVATE | (NEXT.fetch_add(1, Ordering::Relaxed) & 0x7fff_ffff).max(1)
}

/// The private table id of thread `tid`, if it has one (lock-free).
pub fn override_of(tid: u32) -> Option<u32> {
    let n = HWM.load(Ordering::Acquire);
    for s in TID_KEY[..n.min(SLOTS)].iter() {
        let v = s.load(Ordering::Acquire);
        if v != 0 && (v >> 32) as u32 == tid { return Some(v as u32); }
    }
    None
}

fn put(tid: u32, key: u32, tgid: u32) -> bool {
    let want = ((tid as u64) << 32) | key as u64;
    let mut free = None;
    for i in 0..SLOTS {
        let v = TID_KEY[i].load(Ordering::Relaxed);
        if v == 0 { if free.is_none() { free = Some(i); } }
        else if (v >> 32) as u32 == tid {
            TGID[i].store(tgid, Ordering::Relaxed);
            TID_KEY[i].store(want, Ordering::Release);
            return true;
        }
    }
    let Some(i) = free else { return false };
    TGID[i].store(tgid, Ordering::Relaxed);
    TID_KEY[i].store(want, Ordering::Release);
    if HWM.load(Ordering::Relaxed) <= i { HWM.store(i + 1, Ordering::Release); }
    LIVE.fetch_add(1, Ordering::AcqRel);
    true
}

/// Give `tid` the private table `key`. false: the registry is full.
pub fn set(tid: u32, key: u32, tgid: u32) -> bool {
    let _g = WRITE.lock();
    put(tid, key, tgid)
}

/// Forget `tid`'s private table. `Some((key, tgid, last))`: `last` = no other
/// thread still uses `key`.
pub fn drop(tid: u32) -> Option<(u32, u32, bool)> {
    let _g = WRITE.lock();
    for i in 0..SLOTS {
        let v = TID_KEY[i].load(Ordering::Relaxed);
        if v != 0 && (v >> 32) as u32 == tid {
            let key = v as u32;
            let tgid = TGID[i].load(Ordering::Relaxed);
            TID_KEY[i].store(0, Ordering::Release);
            LIVE.fetch_sub(1, Ordering::AcqRel);
            let last = !TID_KEY.iter().any(|s| { let w = s.load(Ordering::Relaxed); w != 0 && w as u32 == key });
            return Some((key, tgid, last));
        }
    }
    None
}

/// A child thread (CLONE_THREAD|CLONE_FILES) joins its creator's private
/// table. No-op if the creator has none.
pub fn inherit(parent_tid: u32, child_tid: u32) -> bool {
    let _g = WRITE.lock();
    for i in 0..SLOTS {
        let v = TID_KEY[i].load(Ordering::Relaxed);
        if v != 0 && (v >> 32) as u32 == parent_tid {
            let tgid = TGID[i].load(Ordering::Relaxed);
            return put(child_tid, v as u32, tgid);
        }
    }
    false
}

/// Threads using table `key` of group `tgid`: a private key counts its slots;
/// the shared table (`key == tgid`) counts the group's threads that have not
/// unshared.
pub fn users(key: u32, tgid: u32) -> usize {
    let mut with_key = 0usize;
    let mut in_group = 0usize;
    for i in 0..SLOTS {
        let v = TID_KEY[i].load(Ordering::Acquire);
        if v == 0 { continue; }
        if v as u32 == key { with_key += 1; }
        if TGID[i].load(Ordering::Relaxed) == tgid { in_group += 1; }
    }
    if key & FD_KEY_PRIVATE != 0 { with_key } else { crate::thread_count(tgid).saturating_sub(in_group) }
}
