//! Alias mounts: `proc`, `devtmpfs`, `sysfs` and bind mounts.
//!
//! LeandrOS has a single filesystem namespace in which `/proc`, `/dev` and
//! the synthesized parts of `/sys` are not mounts but path prefixes the VFS
//! recognises (`should_lookup_ramfs`). A chroot therefore saw none of them:
//! inside a jail at `/root/sysroot`, `/proc/self/exe` became the host path
//! `/root/sysroot/proc/self/exe`, which lands on the jail's empty f2fs
//! directory.
//!
//! An alias entry maps a host-absolute *mount point* onto a host-absolute
//! *source* prefix: `mount -t proc proc /root/sysroot/proc` adds
//! `/root/sysroot/proc -> /proc`, `mount --bind /dev /root/sysroot/dev` adds
//! `/root/sysroot/dev -> /dev`. The kernel's path resolution produces the
//! namespace path (which is what `..`, the chroot floor, the cwd and getcwd
//! work on) and then rewrites it through [`translate`] right before the VFS
//! sees it, the same point at which Linux would cross into the mounted
//! filesystem during the walk. Paths reported back to userspace
//! (`/proc/self/fd/N`, `/proc/self/exe`, `fchdir`) go through [`reverse`] so
//! they read as the caller sees them.
//!
//! The table is global (there are no mount namespaces), small, and empty on
//! a normal boot: `ACTIVE` lets every path operation skip the lock then.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::Mutex;

/// Upper bound on alias mounts (one jail typically needs three or four).
pub const MAX_ALIASES: usize = 64;

pub struct Alias {
    /// Host-absolute mount point, normalised, no trailing slash.
    pub mountpoint: Vec<u8>,
    /// Host-absolute source prefix the mount point stands for.
    pub source: Vec<u8>,
    /// `proc`, `devtmpfs`, `sysfs`, or the source's fstype for a bind mount.
    pub fstype: &'static str,
    /// The `mount(2)` source string, for /proc/mounts.
    pub device: Vec<u8>,
}

static ALIASES: Mutex<Vec<Alias>> = Mutex::new(Vec::new());
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// `p` equals `prefix` or lies below it (component boundary respected).
pub fn under(p: &[u8], prefix: &[u8]) -> bool {
    if prefix == b"/" { return p.first() == Some(&b'/'); }
    p.starts_with(prefix) && (p.len() == prefix.len() || p[prefix.len()] == b'/')
}

/// `prefix` + the part of `p` below `old` (which `p` must be `under`).
fn rebase(p: &[u8], old: &[u8], new: &[u8]) -> Vec<u8> {
    let rest = if old == b"/" { &p[..] } else { &p[old.len()..] };
    let mut v = Vec::with_capacity(new.len() + rest.len());
    if new == b"/" {
        if rest.is_empty() { v.push(b'/'); } else { v.extend_from_slice(rest); }
    } else {
        v.extend_from_slice(new);
        v.extend_from_slice(rest);
    }
    v
}

/// Add (or replace) the alias at `mountpoint`. A mount point equal to its
/// source is a no-op (mounting proc on the real /proc). EINVAL for an
/// alias whose source lies under its own mount point (a loop); ENOSPC when
/// the table is full.
pub fn add(mountpoint: &[u8], source: &[u8], fstype: &'static str, device: &[u8]) -> Result<(), i32> {
    if mountpoint == source { return Ok(()); }
    if under(source, mountpoint) { return Err(-22); }
    let mut t = ALIASES.lock();
    if let Some(a) = t.iter_mut().find(|a| a.mountpoint == mountpoint) {
        a.source = source.to_vec();
        a.fstype = fstype;
        a.device = device.to_vec();
        return Ok(());
    }
    if t.len() >= MAX_ALIASES { return Err(-28); }
    t.push(Alias { mountpoint: mountpoint.to_vec(), source: source.to_vec(), fstype,
                   device: device.to_vec() });
    ACTIVE.store(t.len(), Ordering::Release);
    Ok(())
}

/// Remove the alias mounted exactly at `mountpoint`. False when none is.
pub fn remove(mountpoint: &[u8]) -> bool {
    let mut t = ALIASES.lock();
    let before = t.len();
    t.retain(|a| a.mountpoint != mountpoint);
    ACTIVE.store(t.len(), Ordering::Release);
    t.len() != before
}

/// Rewrite a namespace path to the backing path the VFS serves, following
/// the longest matching mount point (repeatedly, so an alias whose source is
/// itself under another alias still lands). `None` when no alias applies —
/// the overwhelmingly common case, answered without taking the lock.
pub fn translate(path: &[u8]) -> Option<Vec<u8>> {
    if ACTIVE.load(Ordering::Acquire) == 0 { return None; }
    let t = ALIASES.lock();
    let mut cur: Option<Vec<u8>> = None;
    for _ in 0..8 {
        let p: &[u8] = cur.as_deref().unwrap_or(path);
        let best = t.iter()
            .filter(|a| under(p, &a.mountpoint))
            .max_by_key(|a| a.mountpoint.len());
        match best {
            Some(a) => cur = Some(rebase(p, &a.mountpoint, &a.source)),
            None => break,
        }
    }
    cur
}

/// The inverse of [`translate`] as seen by a task whose root is `root`
/// (`None` = not chrooted): a backing path that is not itself visible under
/// `root` but lies under the source of an alias mounted inside `root` is
/// reported at that mount point. Returns `None` when nothing applies.
pub fn reverse(path: &[u8], root: Option<&[u8]>) -> Option<Vec<u8>> {
    if ACTIVE.load(Ordering::Acquire) == 0 { return None; }
    let root = root.unwrap_or(b"/");
    if under(path, root) { return None; }
    let t = ALIASES.lock();
    t.iter()
        .filter(|a| under(path, &a.source) && under(&a.mountpoint, root))
        .max_by_key(|a| a.source.len())
        .map(|a| rebase(path, &a.source, &a.mountpoint))
}

/// The mount point of the alias whose mount point is exactly `path`.
pub fn is_mountpoint(path: &[u8]) -> bool {
    if ACTIVE.load(Ordering::Acquire) == 0 { return false; }
    ALIASES.lock().iter().any(|a| a.mountpoint == path)
}

/// Snapshot `(device, mountpoint, fstype)` of every alias, for /proc/mounts.
pub fn list() -> Vec<(Vec<u8>, Vec<u8>, &'static str)> {
    if ACTIVE.load(Ordering::Acquire) == 0 { return Vec::new(); }
    ALIASES.lock().iter().map(|a| (a.device.clone(), a.mountpoint.clone(), a.fstype)).collect()
}
