//! Overlay support inside the VFS (design-overlay.md section 2).
//!
//! This lane (O1) provides only the layer-independent plumbing:
//!   * `real`   - helpers that operate on the REAL layers with kernel buffers,
//!   * the per-mount pseudo-pid (`OVL_PID_BASE | mount id`) and its credential
//!     override, consulted by `cred_of()` before the scheduler is asked,
//!   * an in-guest selftest (`/proc/sys/fs/ovl_selftest`).
//!
//! Nothing here knows about the mount table: a mount is just a `u32` id.

pub mod real;

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

/// Pseudo-pids live far above any scheduler pid (pid_max is 4194304).
pub const OVL_PID_BASE: u32 = 0x7000_0000;
const OVL_PID_MASK: u32 = 0xFFFF_0000;

/// Credentials captured at mount time, keyed by pseudo-pid.
static CREDS: Mutex<BTreeMap<u32, xattr::Cred>> = Mutex::new(BTreeMap::new());
/// Fast path: false until the first overlay mount registers.
static ANY: AtomicBool = AtomicBool::new(false);

/// The pseudo-pid that real operations of overlay mount `mnt_id` run as.
pub fn pseudo_pid(mnt_id: u32) -> u32 { OVL_PID_BASE | (mnt_id & 0xFFFF) }

pub fn is_pseudo_pid(pid: u32) -> bool { pid & OVL_PID_MASK == OVL_PID_BASE }

/// Record the mounter's credentials for mount `mnt_id`; returns the pseudo-pid.
pub fn register(mnt_id: u32, cred: xattr::Cred) -> u32 {
    let pid = pseudo_pid(mnt_id);
    CREDS.lock().insert(pid, cred);
    ANY.store(true, Ordering::Release);
    pid
}

/// Forget mount `mnt_id`: drop the credential and tear down the pseudo-pid's
/// transient fd table (closing anything a real op left open).
pub fn unregister(mnt_id: u32) {
    let pid = pseudo_pid(mnt_id);
    CREDS.lock().remove(&pid);
    let _ = crate::handle_close_all_pub(pid);
}

/// The override consulted by `cred_of`. One relaxed load when no overlay exists.
///
/// Two cases: `pid` is itself a pseudo-pid (the VFS handlers run as it), or
/// `pid` is a thread currently inside a real op (`enter`): the fs servers
/// (f2fs) are called through the IPC port with `sched::current_pid()` as the
/// caller, not the pseudo-pid, so the thread is mapped to the mounter's cred
/// for the duration of the call.
#[inline]
pub fn cred_override(pid: u32) -> Option<xattr::Cred> {
    if !ANY.load(Ordering::Relaxed) { return None; }
    if is_pseudo_pid(pid) { return CREDS.lock().get(&pid).copied(); }
    ACTIVE.lock().get(&pid).copied()
}

/// Threads inside a real op, mapped to the cred they run as.
static ACTIVE: Mutex<BTreeMap<u32, xattr::Cred>> = Mutex::new(BTreeMap::new());

/// RAII window during which the current thread's `cred_of` answers the
/// mounter's credential. Re-entrant (restores the previous entry).
pub struct Guard { tid: u32, prev: Option<xattr::Cred> }

pub fn enter(pseudo: u32) -> Option<Guard> {
    let cred = CREDS.lock().get(&pseudo).copied()?;
    let tid = sched::current_pid();
    let prev = ACTIVE.lock().insert(tid, cred);
    Some(Guard { tid, prev })
}

impl Drop for Guard {
    fn drop(&mut self) {
        let mut a = ACTIVE.lock();
        match self.prev { Some(c) => { a.insert(self.tid, c); } None => { a.remove(&self.tid); } }
    }
}

/// Selftest body for `/proc/sys/fs/ovl_selftest`: appends "PASS name" or
/// "FAIL name detail" lines to `out`.
pub fn selftest(out: &mut alloc::vec::Vec<u8>) { real::selftest(out) }
