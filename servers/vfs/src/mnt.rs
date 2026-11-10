//! Mount namespaces: the per-namespace mount tree, the superblock registry
//! and the symlink-scan interface (lane M0, design-mntns.md).
//!
//! Coordinates. A *namespace path* is a lexical absolute path in a task's
//! mount namespace (what root, cwd, `..` and getcwd work on). A *backing
//! path* is a path in the single global space the VFS serves. Superblock
//! instances created by a mount (tmpfs, devpts, mqueue, overlay, cgroup2)
//! get their own backing root under the hidden prefix [`SB_ROOT`].
//!
//! NsId 0 (the init namespace) is the identity: its table is [`ns0`] (the
//! former alias.rs), and boot, chroot, the Firefox void root and every ns-0
//! lookup keep exactly their old code path. Only tasks whose mount
//! namespace is not 0 use the tree below.
//!
//! Published interfaces for later lanes:
//!  * superblocks: [`sb_create`], [`sb_get`], [`sb_put`], [`sb_root`], [`sb_count`]
//!  * symlink scan: [`register_linkscan`] / [`linkscan`] (see [`LinkScan`])
//!  * per-namespace trees: [`copy`], [`translate`], [`reverse`], [`release`]
//!  * path context for the VFS chokepoint: [`callctx_set`] / [`callctx_get`]

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use spin::Mutex;
use sched::ns::NsId;

pub use ns0::under;

/// Hidden backing prefix of every superblock instance. No ns-0 path reaches
/// it (`resolve_ns` answers ENOENT) and it is not listed in `/`.
pub const SB_ROOT: &[u8] = b"/.leandros-sb";

pub const MNT_RDONLY: u8 = 1;
pub const MNT_NOSUID: u8 = 2;
pub const MNT_NODEV: u8 = 4;
pub const MNT_NOEXEC: u8 = 8;
pub const MNT_NOATIME: u8 = 16;
pub const MNT_NODIRATIME: u8 = 32;
pub const MNT_RELATIME: u8 = 64;
pub const MNT_STRICTATIME: u8 = 128;
/// `Mnt::locked` bit: the mount itself is locked (cannot be umounted).
pub const LOCK_MNT: u8 = 0x80;
// MNT_LOCK_{RDONLY,NOSUID,NODEV,NOEXEC,ATIME} reuse the low flag bits.

pub type SbId = u32;

pub struct Mnt {
    /// Global monotonically allocated mountinfo id; never 0.
    pub id: u32,
    /// Parent mount id; 0 = this namespace's root mount.
    pub parent: u32,
    /// Mountpoint relative to the parent's root; `/` = stacked on the
    /// parent's root (overmount).
    pub mp: Box<[u8]>,
    /// Backing path of this mount's root (a directory, or a file for file binds).
    pub root: Box<[u8]>,
    pub sb: SbId,
    pub flags: u8,
    pub locked: u8,
    pub group: u32,
    pub master: u32,
    pub unbindable: bool,
    pub fstype: &'static str,
    pub device: Box<[u8]>,
    pub opts: Box<[u8]>,
}

impl Clone for Mnt {
    fn clone(&self) -> Mnt {
        Mnt { id: self.id, parent: self.parent, mp: self.mp.clone(), root: self.root.clone(), sb: self.sb,
              flags: self.flags, locked: self.locked, group: self.group, master: self.master,
              unbindable: self.unbindable, fstype: self.fstype, device: self.device.clone(), opts: self.opts.clone() }
    }
}

pub struct MntNs {
    pub owner_user: NsId,
    pub mnts: Vec<Mnt>,
    pub seq: u64,
}

/// Leaf lock: never held across sched or port calls.
static NS: Mutex<BTreeMap<NsId, MntNs>> = Mutex::new(BTreeMap::new());
static NEXT_ID: AtomicU32 = AtomicU32::new(1000);

pub fn alloc_mnt_id() -> u32 { NEXT_ID.fetch_add(1, Ordering::Relaxed) }

/// Result of a tree walk.
pub struct Xlat {
    pub backing: Vec<u8>,
    pub mnt_id: u32,
    /// Length of the namespace-path prefix that names the final mount point.
    pub ns_prefix_len: usize,
    pub flags: u8,
}

fn root_idx(t: &[Mnt]) -> Option<usize> { t.iter().position(|m| m.parent == 0) }

/// Follow overmounts (children with `mp == "/"`, newest wins) from `cur`.
fn top_of(t: &[Mnt], mut cur: usize) -> usize {
    loop {
        let id = t[cur].id;
        match t.iter().enumerate().filter(|(_, m)| m.parent == id && &*m.mp == b"/").max_by_key(|(_, m)| m.id) {
            Some((i, _)) => cur = i,
            None => return cur,
        }
    }
}

fn join(root: &[u8], rem: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(root.len() + rem.len() + 1);
    if root == b"/" { if rem.is_empty() { v.push(b'/'); } else { v.extend_from_slice(rem); } }
    else { v.extend_from_slice(root); v.extend_from_slice(rem); }
    v
}

fn walk(t: &[Mnt], path: &[u8]) -> Option<Xlat> {
    let mut cur = top_of(t, root_idx(t)?);
    let mut rem: &[u8] = path;
    let mut consumed = 0usize;
    for _ in 0..256 {
        let id = t[cur].id;
        let best = t.iter().enumerate()
            .filter(|(_, m)| m.parent == id && &*m.mp != b"/" && under(rem, &m.mp))
            .max_by_key(|(_, m)| (m.mp.len(), m.id));
        match best {
            Some((i, m)) => { consumed += m.mp.len(); rem = &rem[m.mp.len()..]; cur = top_of(t, i); }
            None => break,
        }
    }
    let m = &t[cur];
    let backing = join(&m.root, rem);
    // The hidden superblock space is reachable only through a mount whose
    // root is in it (a bind of `/` must not expose it).
    if is_sb_path(&backing) && !is_sb_path(&m.root) { return None; }
    Some(Xlat { backing, mnt_id: m.id, ns_prefix_len: consumed, flags: m.flags })
}

/// Rewrite namespace path `path` (absolute, normalised) of mount namespace
/// `ns` to its backing path. `None` when the namespace has no tree.
pub fn translate(ns: NsId, path: &[u8]) -> Option<Xlat> {
    let g = NS.lock();
    walk(&g.get(&ns)?.mnts, path)
}

/// Absolute namespace path of the mountpoint of `m`.
fn ns_mountpoint(t: &[Mnt], mut i: usize) -> Vec<u8> {
    let mut parts: Vec<&[u8]> = Vec::new();
    for _ in 0..256 {
        let m = &t[i];
        if m.parent == 0 { break; }
        if &*m.mp != b"/" { parts.push(&m.mp); }
        match t.iter().position(|x| x.id == m.parent) { Some(p) => i = p, None => break }
    }
    let mut v = Vec::new();
    for p in parts.iter().rev() { v.extend_from_slice(p); }
    if v.is_empty() { v.push(b'/'); }
    v
}

/// The inverse of [`translate`]: the namespace path at which backing path
/// `backing` is visible. Prefers the mount `hint` (the open file's mount id),
/// else the longest covering mount root whose path round-trips.
pub fn reverse(ns: NsId, backing: &[u8], hint: u32) -> Option<Vec<u8>> {
    let g = NS.lock();
    let t = &g.get(&ns)?.mnts;
    let mut order: Vec<usize> = (0..t.len()).collect();
    order.sort_by_key(|&i| (t[i].id != hint, core::cmp::Reverse(t[i].root.len())));
    for i in order {
        let m = &t[i];
        if !under(backing, &m.root) { continue; }
        let mp = ns_mountpoint(t, i);
        let rest: &[u8] = if &*m.root == b"/" { backing } else { &backing[m.root.len()..] };
        let cand = join(&mp, rest);
        if let Some(x) = walk(t, &cand) { if x.backing == backing { return Some(cand); } }
    }
    None
}

/// Like [`reverse`] for a descriptor whose mount is unknown: among the mounts
/// whose path round-trips, the newest wins (a container's rootfs mounts are
/// newer than the copies of the host's, which cover the same backing paths).
pub fn reverse_newest(ns: NsId, backing: &[u8]) -> Option<Vec<u8>> {
    let g = NS.lock();
    let t = &g.get(&ns)?.mnts;
    let mut order: Vec<usize> = (0..t.len()).collect();
    order.sort_by_key(|&i| core::cmp::Reverse(t[i].id));
    let order2 = order.clone();
    for i in order {
        let m = &t[i];
        if !under(backing, &m.root) { continue; }
        let mp = ns_mountpoint(t, i);
        let rest: &[u8] = if &*m.root == b"/" { backing } else { &backing[m.root.len()..] };
        let cand = join(&mp, rest);
        if let Some(x) = walk(t, &cand) { if x.backing == backing { return Some(cand); } }
    }
    // A descriptor opened before something was mounted on its object (crun
    // opens the target O_PATH, binds onto /proc/self/fd/N, then sets the
    // propagation / attributes through the same descriptor): its backing path
    // names a covered object, so no mount round-trips to it. Linux follows the
    // mount at the end of the magic link; do the same - the name of the
    // mountpoint now resolves to the covering mount.
    for &i in order2.iter() {
        let m = &t[i];
        if !under(backing, &m.root) { continue; }
        let mp = ns_mountpoint(t, i);
        let rest: &[u8] = if &*m.root == b"/" { backing } else { &backing[m.root.len()..] };
        let cand = join(&mp, rest);
        if let Some(x) = walk(t, &cand) { if x.ns_prefix_len == cand.len() && cand.len() > 1 { return Some(cand); } }
    }
    None
}


/// Does the namespace have a tree (i.e. was it created through [`copy`])?
pub fn has_tree(ns: NsId) -> bool { NS.lock().contains_key(&ns) }

/// Install a tree for `dst` as a copy of `src` (0 = synthesized from the
/// ns-0 tables). Mount ids are new; group/master are copied. When the owner
/// user namespace changes, every mount becomes locked and shared mounts
/// become slaves. Call before the task switches, holding no other locks.
pub fn copy(src: NsId, dst: NsId, dst_owner: NsId, owner_changed: bool) -> Result<(), i32> {
    let (mut mnts, seq) = if src == 0 {
        (crate::ns0_snapshot(), 0)
    } else {
        let g = NS.lock();
        match g.get(&src) { Some(s) => (s.mnts.clone(), s.seq), None => return Err(-22) }
    };
    let remap: Vec<(u32, u32)> = mnts.iter().map(|m| (m.id, alloc_mnt_id())).collect();
    let nid = |old: u32| remap.iter().find(|r| r.0 == old).map(|r| r.1).unwrap_or(0);
    for m in mnts.iter_mut() {
        m.id = nid(m.id);
        m.parent = nid(m.parent);
        sb_get(m.sb);
        if owner_changed {
            m.locked |= LOCK_MNT | (m.flags & 0x7f);
            if m.group != 0 { m.master = m.group; m.group = 0; }
        }
    }
    NS.lock().insert(dst, MntNs { owner_user: dst_owner, mnts, seq });
    Ok(())
}

// ── Mount, bind and umount inside a namespace (lane M1) ─────────────────────

/// Upper bound on mounts per namespace (Linux `fs.mount-max` is 100000).
pub const MNT_MAX: usize = 1024;

/// Like [`walk`] but returns the final mount's index and how many bytes of
/// `path` the walk consumed up to and including that mount's mountpoint.
fn walk_idx(t: &[Mnt], path: &[u8]) -> Option<(usize, usize)> {
    let mut cur = top_of(t, root_idx(t)?);
    let mut consumed = 0usize;
    for _ in 0..256 {
        let id = t[cur].id;
        let rem = &path[consumed..];
        let best = t.iter().enumerate()
            .filter(|(_, m)| m.parent == id && &*m.mp != b"/" && under(rem, &m.mp))
            .max_by_key(|(_, m)| (m.mp.len(), m.id));
        match best {
            Some((i, m)) => { consumed += m.mp.len(); cur = top_of(t, i); }
            None => break,
        }
    }
    Some((cur, consumed))
}

/// Indices of every mount below mount id `id` (transitively).
fn descendants(t: &[Mnt], id: u32) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    let mut frontier: Vec<u32> = alloc::vec![id];
    while let Some(p) = frontier.pop() {
        for (i, m) in t.iter().enumerate() {
            if m.parent == p && !out.contains(&i) { out.push(i); frontier.push(m.id); }
        }
    }
    out
}

/// Where a new mount for `dst` attaches: (parent id, mountpoint relative to it).
fn attach_point(t: &[Mnt], dst: &[u8]) -> Result<(u32, Vec<u8>), i32> {
    let (di, doff) = walk_idx(t, dst).ok_or(-22)?;
    let rem = &dst[doff..];
    let mp: Vec<u8> = if rem.is_empty() || rem == b"/" { b"/".to_vec() } else { rem.to_vec() };
    Ok((t[di].id, mp))
}

/// `mount --bind` (`rec == false`) or `--rbind` of namespace path `src` onto
/// `dst`, in namespace `ns` only. A non-recursive bind of a tree with locked
/// children is EINVAL (it would expose what the locks cover); an unbindable
/// source is EINVAL. Returns the new top mount id.
pub fn bind(ns: NsId, src: &[u8], dst: &[u8], rec: bool) -> Result<u32, i32> {
    let mut g = NS.lock();
    let t = &mut g.get_mut(&ns).ok_or(-22)?.mnts;
    if t.len() >= MNT_MAX { return Err(-28); }
    let mut add = clone_subtree(t, src, rec)?;
    let (pid, mp) = attach_point(t, dst)?;
    add[0].parent = pid;
    add[0].mp = mp.into_boxed_slice();
    let top_id = add[0].id;
    if t.len() + add.len() > MNT_MAX { return Err(-28); }
    for m in add.iter() { sb_get(m.sb); }
    t.extend(add);
    Ok(top_id)
}

/// Copy the mount tree rooted at namespace path `src` (all of it when `rec`,
/// only the top mount otherwise). The returned vector starts with the new top
/// mount, detached (`parent == 0`, `mp == "/"`, not locked); the caller
/// attaches it and takes the superblock references. A non-recursive copy of a
/// tree with locked children is EINVAL (it would expose what the locks
/// cover); an unbindable source is EINVAL.
fn clone_subtree(t: &[Mnt], src: &[u8], rec: bool) -> Result<Vec<Mnt>, i32> {
    let (si, soff) = walk_idx(t, src).ok_or(-2)?;
    let sub: &[u8] = &src[soff..];
    let sm = &t[si];
    if sm.unbindable { return Err(-22); }
    let sid = sm.id;
    let in_sub = |mp: &[u8]| sub.is_empty() || (mp != b"/" && under(mp, sub));
    if !rec && t.iter().any(|c| c.parent == sid && c.locked & LOCK_MNT != 0 && in_sub(&c.mp)) { return Err(-22); }
    let mut top = sm.clone();
    top.id = alloc_mnt_id();
    top.parent = 0;
    top.mp = b"/".to_vec().into_boxed_slice();
    top.root = join(&sm.root, sub).into_boxed_slice();
    top.locked &= !LOCK_MNT;
    let top_id = top.id;
    let mut add: Vec<Mnt> = alloc::vec![top];
    if rec {
        // old id -> new id for every cloned mount
        let mut map: Vec<(u32, u32)> = alloc::vec![(sid, top_id)];
        let mut work: Vec<u32> = alloc::vec![sid];
        while let Some(old) = work.pop() {
            let new_parent = map.iter().find(|r| r.0 == old).map(|r| r.1).unwrap_or(0);
            for c in t.iter().filter(|c| c.parent == old) {
                // Linux copy_tree: skipping a locked unbindable child would
                // reveal what it covers, so the bind fails with EPERM.
                if c.unbindable {
                    if c.locked & LOCK_MNT != 0 { return Err(-1); }
                    continue;
                }
                if old == sid && !(&*c.mp != b"/" && (sub.is_empty() || under(&c.mp, sub))) { continue; }
                let mut n = c.clone();
                n.id = alloc_mnt_id();
                n.parent = new_parent;
                if old == sid && !sub.is_empty() {
                    let rest = &c.mp[sub.len()..];
                    n.mp = (if rest.is_empty() { b"/".to_vec() } else { rest.to_vec() }).into_boxed_slice();
                }
                map.push((c.id, n.id));
                work.push(c.id);
                add.push(n);
            }
        }
    }
    Ok(add)
}

// ── New mount API: detached trees, move_mount, mount_setattr (lane opentree) ──

/// Anonymous (detached) mount namespaces made by open_tree(OPEN_TREE_CLONE):
/// (namespace id, whether the tree root is a directory). The tree lives in
/// `NS` under that id and is freed with the namespace when its last fd closes.
static ANON: Mutex<Vec<(NsId, bool)>> = Mutex::new(Vec::new());

/// Where a detached tree's descriptor now lives after move_mount attached it:
/// (detached id, namespace, namespace path). The descriptor keeps naming the
/// moved mount, so mount_setattr(fd, "", AT_EMPTY_PATH) and mount(2) on
/// /proc/self/fd/N act on it at its new place (crun does both right after
/// moving a cloned file/dir bind into the rootfs). Bounded: oldest dropped.
static ATTACHED: Mutex<Vec<(NsId, NsId, Vec<u8>)>> = Mutex::new(Vec::new());

pub fn attached_at(anon: NsId) -> Option<(NsId, Vec<u8>)> {
    ATTACHED.lock().iter().rev().find(|a| a.0 == anon).map(|a| (a.1, a.2.clone()))
}

/// Is `ns` a detached tree made by open_tree?
pub fn is_anon(ns: NsId) -> bool { ANON.lock().iter().any(|a| a.0 == ns) }

/// Does the detached tree `ns` have a directory as its root?
pub fn anon_is_dir(ns: NsId) -> bool { ANON.lock().iter().find(|a| a.0 == ns).map_or(true, |a| a.1) }

/// open_tree(OPEN_TREE_CLONE): copy the tree at `src` (a path in `ns`) into
/// the fresh namespace `anon`, which becomes the detached tree.
pub fn open_tree_clone(ns: NsId, src: &[u8], rec: bool, anon: NsId, anon_owner: NsId, is_dir: bool) -> Result<(), i32> {
    let add = {
        let g = NS.lock();
        let t = &g.get(&ns).ok_or(-22)?.mnts;
        if t.len() >= MNT_MAX { return Err(-28); }
        clone_subtree(t, src, rec)?
    };
    for m in add.iter() { sb_get(m.sb); }
    ATTACHED.lock().retain(|a| a.0 != anon); // namespace ids are recycled
    ANON.lock().push((anon, is_dir));
    NS.lock().insert(anon, MntNs { owner_user: anon_owner, mnts: add, seq: 0 });
    Ok(())
}

/// move_mount() of a detached tree: graft it at `dst` in `ns`. The detached
/// tree is consumed (a second attach is EINVAL).
pub fn move_attach(ns: NsId, anon: NsId, dst: &[u8]) -> Result<(), i32> {
    let mut g = NS.lock();
    let mut mnts = match g.get_mut(&anon) { Some(a) => core::mem::take(&mut a.mnts), None => return Err(-22) };
    if mnts.is_empty() { return Err(-22); }
    let res = (|| -> Result<(), i32> {
        let t = &mut g.get_mut(&ns).ok_or(-22)?.mnts;
        if t.len() + mnts.len() > MNT_MAX { return Err(-28); }
        let (pid, mp) = attach_point(t, dst)?;
        let ri = mnts.iter().position(|m| m.parent == 0).ok_or(-22)?;
        mnts[ri].parent = pid;
        mnts[ri].mp = mp.into_boxed_slice();
        t.extend(core::mem::take(&mut mnts));
        Ok(())
    })();
    if res.is_ok() {
        let mut a = ATTACHED.lock();
        if a.len() >= 64 { a.remove(0); }
        a.push((anon, ns, dst.to_vec()));
    }
    if res.is_err() {
        if let Some(a) = g.get_mut(&anon) { a.mnts = mnts; }
    }
    res
}

/// move_mount() of an existing mount: re-parent the mount whose root is
/// namespace path `src` onto `dst`. Locked mounts, the namespace root and a
/// destination inside the moved subtree are refused.
pub fn move_mnt(ns: NsId, src: &[u8], dst: &[u8]) -> Result<(), i32> {
    let mut g = NS.lock();
    let t = &mut g.get_mut(&ns).ok_or(-22)?.mnts;
    let (si, soff) = walk_idx(t, src).ok_or(-2)?;
    if soff != src.len() && !(src == b"/" && soff == 0) { return Err(-22); }
    if t[si].parent == 0 || t[si].locked & LOCK_MNT != 0 { return Err(-22); }
    let sid = t[si].id;
    let (di, _) = walk_idx(t, dst).ok_or(-2)?;
    if di == si || descendants(t, sid).contains(&di) { return Err(-40); }
    let (pid, mp) = attach_point(t, dst)?;
    t[si].parent = pid;
    t[si].mp = mp.into_boxed_slice();
    Ok(())
}

/// mount_setattr(): set/clear per-mount flags and/or change propagation of the
/// mount rooted at `path` (and of everything below it when `rec`). Clearing a
/// flag the mount is locked to is EPERM; nothing changes on failure.
pub fn setattr(ns: NsId, path: &[u8], set: u8, clr: u8, prop: usize, rec: bool) -> Result<(), i32> {
    {
        let mut g = NS.lock();
        let t = &mut g.get_mut(&ns).ok_or(-22)?.mnts;
        let (i, off) = walk_idx(t, path).ok_or(-2)?;
        if off != path.len() && !(path == b"/" && off == 0) { return Err(-22); }
        let mut idx: Vec<usize> = alloc::vec![i];
        if rec { idx.extend(descendants(t, t[i].id)); }
        if idx.iter().any(|&k| clr & t[k].locked & 0x7f != 0) { return Err(-1); }
        for &k in idx.iter() { t[k].flags = (t[k].flags & !clr) | set; }
    }
    if prop != 0 {
        let r = set_propagation(ns, path, prop | if rec { MS_REC } else { 0 });
        if r != 0 { return Err(r); }
    }
    Ok(())
}

/// Namespace references held by `nsfs` superblocks: sb id -> namespace id.
static NSFS_SB: Mutex<Vec<(SbId, u16)>> = Mutex::new(Vec::new());

fn nsfs_destroy(root: &[u8]) {
    let n = root.iter().rev().take_while(|c| c.is_ascii_digit()).count();
    let sbid: SbId = core::str::from_utf8(&root[root.len() - n..]).ok().and_then(|d| d.parse().ok()).unwrap_or(0);
    let id = {
        let mut t = NSFS_SB.lock();
        t.iter().position(|e| e.0 == sbid).map(|i| t.remove(i).1)
    };
    if let Some(id) = id { crate::sched_ns_put(id); }
}

/// A superblock for a bound nsfs file: it OWNS one namespace reference
/// (already taken by the caller) and drops it when the last mount (or mount
/// namespace copy) of the file goes away.
pub fn nsfs_sb_new(id: u16) -> SbId {
    let (sb, _) = sb_create("nsfs", Some(nsfs_destroy));
    NSFS_SB.lock().push((sb, id));
    sb
}

/// Bind a namespace file (synthetic source `/.leandros-ns/<kind>/<id>`, one
/// namespace reference already taken for `synth`) onto the file at namespace
/// path `dst` of mount namespace `ns`. On error the reference is dropped.
pub fn bind_nsfs(ns: NsId, synth: &[u8], dst: &[u8]) -> Result<u32, i32> {
    let Some((_, id)) = ns0::ns_src(synth) else { return Err(-22) };
    let sb = nsfs_sb_new(id);
    let r = (|| {
        let mut g = NS.lock();
        let t = &mut g.get_mut(&ns).ok_or(-22)?.mnts;
        if t.len() >= MNT_MAX { return Err(-28); }
        let (pid, mp) = attach_point(t, dst)?;
        let mut m = mk(alloc_mnt_id(), pid, &mp, synth, sb);
        m.flags = MNT_NOSUID | MNT_RELATIME;
        m.fstype = "nsfs";
        m.device = Box::from(&b"nsfs"[..]);
        let mid = m.id;
        sb_get(sb);
        t.push(m);
        Ok(mid)
    })();
    // No mount took the reference: putting an unreferenced sb destroys it.
    if r.is_err() { sb_put(sb); }
    r
}

/// Attach a brand-new mount (a fresh superblock, already referenced by the
/// caller's `sb_create` + this call) at namespace path `dst`.
pub fn add_mount(ns: NsId, dst: &[u8], root: &[u8], sb: SbId, fstype: &'static str, device: &[u8], flags: u8) -> Result<u32, i32> {
    let mut g = NS.lock();
    let t = &mut g.get_mut(&ns).ok_or(-22)?.mnts;
    if t.len() >= MNT_MAX { return Err(-28); }
    let (pid, mp) = attach_point(t, dst)?;
    let mut m = mk(alloc_mnt_id(), pid, &mp, root, sb);
    m.flags = flags;
    m.fstype = fstype;
    m.device = device.to_vec().into_boxed_slice();
    let id = m.id;
    sb_get(sb);
    t.push(m);
    Ok(id)
}

/// Linux `mnt_already_visible` for proc and sysfs mounts by a task in a
/// non-initial user namespace: some mount of `fstype` exposing the whole
/// filesystem (root = its tree root) is in `ns`, with nothing locked stacked
/// on it and no locked flag the new mount would drop.
pub fn fully_visible(ns: NsId, fstype: &str, new_flags: u8) -> bool {
    let g = NS.lock();
    let Some(n) = g.get(&ns) else { return false };
    let t = &n.mnts;
    let full: &[u8] = if fstype == "proc" { b"/proc" } else { b"/sys" };
    t.iter().any(|m| {
        m.fstype == fstype && &*m.root == full
            && (m.locked & 0x7f) & !new_flags == 0
            && !t.iter().any(|c| c.parent == m.id && c.locked & LOCK_MNT != 0)
    })
}

/// umount2 of the mount at namespace path `target`. The mount must be the
/// final component's mountpoint (EINVAL otherwise), not the namespace root,
/// not locked (EINVAL), and without submounts unless `detach` (EBUSY). A
/// detach takes every descendant, locked or not, with it.
pub fn umount(ns: NsId, target: &[u8], detach: bool) -> Result<(), i32> {
    let dead_sbs: Vec<SbId> = {
        let mut g = NS.lock();
        let t = &mut g.get_mut(&ns).ok_or(-22)?.mnts;
        let (i, off) = walk_idx(t, target).ok_or(-22)?;
        // `/` names the top of the root's overmount stack (offset 0).
        if off != target.len() && !(target == b"/" && off == 0) { return Err(-22); }
        let m = &t[i];
        if m.parent == 0 { return Err(-22); }
        if m.locked & LOCK_MNT != 0 { return Err(-22); }
        let kids = descendants(t, m.id);
        if !kids.is_empty() && !detach { return Err(-16); }
        let mut gone: Vec<u32> = kids.iter().map(|&k| t[k].id).collect();
        gone.push(m.id);
        let sbs: Vec<SbId> = t.iter().filter(|x| gone.contains(&x.id)).map(|x| x.sb).collect();
        t.retain(|x| !gone.contains(&x.id));
        sbs
    };
    for sb in dead_sbs { sb_put(sb); }
    Ok(())
}

/// Result of a tree pivot: how stored namespace paths move.
pub struct PivotMap {
    /// Namespace path of the new root before the pivot.
    pub new_root: Vec<u8>,
    /// Namespace path (in the new tree) at which the old root now sits.
    pub old_at: Vec<u8>,
}

impl PivotMap {
    /// Where the namespace path `p` (absolute, normalised) lives after the
    /// pivot. A path under the new root loses that prefix; the rest of the
    /// old tree is reached through the old root's new attachment point. The
    /// old root itself (`/`) is the new root for the tasks standing on it.
    pub fn map(&self, p: &[u8]) -> Vec<u8> {
        if p == b"/" { return b"/".to_vec(); }
        if under(p, &self.new_root) {
            let rest = &p[self.new_root.len()..];
            return if rest.is_empty() { b"/".to_vec() } else { rest.to_vec() };
        }
        if &*self.old_at == b"/" { return p.to_vec(); }
        let mut v = self.old_at.clone();
        v.extend_from_slice(p);
        v
    }
}

fn is_shared(m: &Mnt) -> bool { m.group != 0 }

/// pivot_root(2) in mount namespace `ns` (not the initial one). The mount at
/// `new_path` becomes the root mount and the old root mount is attached at
/// `put_old`; `MNT_LOCKED` moves from the old root to the new one. Errors
/// follow Linux: EINVAL (not a mountpoint, put_old not under new_root, a
/// shared parent, new root locked), EBUSY (new_root or put_old on the current
/// root mount).
pub fn pivot_root(ns: NsId, new_path: &[u8], put_old: &[u8]) -> Result<PivotMap, i32> {
    let mut g = NS.lock();
    let t = &mut g.get_mut(&ns).ok_or(-22)?.mnts;
    let (ri, _) = walk_idx(t, b"/").ok_or(-22)?;
    if t[ri].parent != 0 { return Err(-22); }
    let (ni, noff) = walk_idx(t, new_path).ok_or(-22)?;
    if noff != new_path.len() && !(new_path == b"/" && noff == 0) { return Err(-22); }
    let (yi, yoff) = walk_idx(t, put_old).ok_or(-22)?;
    if ni == ri || yi == ri {
        // The root mount: new_root on it is EBUSY; a put_old on it is either
        // EBUSY or, outside the new root, EINVAL.
        if ni == ri { return Err(-16); }
    }
    let xi = t.iter().position(|m| m.id == t[ni].parent).ok_or(-22)?;
    if is_shared(&t[yi]) || is_shared(&t[xi]) { return Err(-22); }
    if t[ni].locked & LOCK_MNT != 0 { return Err(-22); }
    // put_old must lie inside the new root's tree.
    let mut cur = yi;
    let mut inside = false;
    for _ in 0..256 {
        if cur == ni { inside = true; break; }
        match t.iter().position(|m| m.id == t[cur].parent) { Some(p) => cur = p, None => break }
    }
    if !inside { return Err(-22); }
    if yi == ri { return Err(-16); }
    let rel: Vec<u8> = if yoff >= put_old.len() { b"/".to_vec() } else { put_old[yoff..].to_vec() };
    let new_root: Vec<u8> = if new_path.is_empty() { b"/".to_vec() } else { new_path.to_vec() };
    let old_at: Vec<u8> = if put_old.len() <= new_root.len() || put_old == &new_root[..] { b"/".to_vec() }
        else if new_root == b"/" { put_old.to_vec() } else { put_old[new_root.len()..].to_vec() };
    let (nid, yid) = (t[ni].id, t[yi].id);
    let _ = nid;
    // Re-parent: the old root hangs off put_old, the new root becomes the root.
    t[ri].parent = yid;
    t[ri].mp = rel.into_boxed_slice();
    t[ni].parent = 0;
    t[ni].mp = b"/".to_vec().into_boxed_slice();
    if t[ri].locked & LOCK_MNT != 0 { t[ri].locked &= !LOCK_MNT; t[ni].locked |= LOCK_MNT; }
    Ok(PivotMap { new_root, old_at })
}

/// Owner user namespace of `ns`'s mount tree.
pub fn owner_of(ns: NsId) -> Option<NsId> { NS.lock().get(&ns).map(|n| n.owner_user) }

/// Number of mounts in `ns` (tests).
pub fn mount_count(ns: NsId) -> usize { NS.lock().get(&ns).map_or(0, |n| n.mnts.len()) }

/// Free the tree of `ns`, dropping its superblock references.
pub fn release(ns: NsId) {
    ANON.lock().retain(|a| a.0 != ns);
    let t = NS.lock().remove(&ns);
    if let Some(t) = t { for m in t.mnts.iter() { sb_put(m.sb); } }
}

/// Free the trees of namespaces that died under the scheduler lock, then
/// make their ids reusable.
pub fn reap() { sched::ns::reap(|id| release(id)); }

/// Run `f` on `ns`'s table (leaf lock held: no sched or port calls inside).
pub fn with_ns<R>(ns: NsId, f: impl FnOnce(&mut MntNs) -> R) -> Option<R> {
    NS.lock().get_mut(&ns).map(f)
}

// ── Propagation state and mountinfo rendering (M3) ───────────────────────────

pub const MS_UNBINDABLE: usize = 1 << 17;
pub const MS_PRIVATE: usize = 1 << 18;
pub const MS_SLAVE: usize = 1 << 19;
pub const MS_SHARED: usize = 1 << 20;
pub const MS_REC: usize = 0x4000;

/// Peer-group id the initial namespace's `/` starts in (`shared:1`).
pub const ROOT_GROUP: u32 = 1;
static NEXT_GROUP: AtomicU32 = AtomicU32::new(2);

/// (group, master, unbindable) of one propagation state.
pub type Prop = (u32, u32, bool);

/// Propagation state of namespace-0 mounts, keyed by namespace mountpoint.
/// A mount without an entry is private, except `/` which starts shared.
static PROP0: Mutex<BTreeMap<Vec<u8>, Prop>> = Mutex::new(BTreeMap::new());

pub fn ns0_prop(path: &[u8]) -> Prop {
    match PROP0.lock().get(path) {
        Some(p) => *p,
        None if path == b"/" => (ROOT_GROUP, 0, false),
        None => (0, 0, false),
    }
}

fn apply_mode(p: Prop, mode: usize) -> Prop {
    match mode {
        MS_SHARED => (if p.0 != 0 { p.0 } else { NEXT_GROUP.fetch_add(1, Ordering::Relaxed) }, p.1, false),
        MS_SLAVE => (0, if p.0 != 0 { p.0 } else { p.1 }, false),
        MS_UNBINDABLE => (0, 0, true),
        _ => (0, 0, false),
    }
}

/// mount(2) MS_SHARED/PRIVATE/SLAVE/UNBINDABLE (+MS_REC) on `path`, a
/// namespace path in `ns`. EINVAL when it is not a mount point or `flags`
/// names several propagation types.
pub fn set_propagation(ns: NsId, path: &[u8], flags: usize) -> i32 {
    let mode = flags & (MS_UNBINDABLE | MS_PRIVATE | MS_SLAVE | MS_SHARED);
    if mode.count_ones() != 1 { return -22; }
    let rec = flags & MS_REC != 0;
    let hit = |p: &[u8]| p == path || (rec && under(p, path));
    if ns == 0 || !has_tree(ns) {
        let mps = crate::ns0_mountpoints();
        if !mps.iter().any(|m| m.as_slice() == path) { return -22; }
        let mut g = PROP0.lock();
        for m in mps.iter().filter(|m| hit(m)) {
            let cur = g.get(m.as_slice()).copied().unwrap_or(if m.as_slice() == b"/" { (ROOT_GROUP, 0, false) } else { (0, 0, false) });
            let mut n = apply_mode(cur, mode);
            // `/` is the one peer group of ns 0 at id 1: re-sharing it returns there.
            if m.as_slice() == b"/" && mode == MS_SHARED && cur.0 == 0 { n.0 = ROOT_GROUP; }
            g.insert(m.clone(), n);
        }
        return 0;
    }
    with_ns(ns, |n| {
        let paths: Vec<Vec<u8>> = (0..n.mnts.len()).map(|i| ns_mountpoint(&n.mnts, i)).collect();
        if !paths.iter().any(|p| p.as_slice() == path) { return -22; }
        for (i, p) in paths.iter().enumerate() {
            if !hit(p) { continue; }
            let m = &mut n.mnts[i];
            let r = apply_mode((m.group, m.master, m.unbindable), mode);
            m.group = r.0; m.master = r.1; m.unbindable = r.2;
        }
        0
    }).unwrap_or(-22)
}

fn push_u32(v: &mut Vec<u8>, mut n: u32) {
    let mut d = [0u8; 10]; let mut i = 0;
    loop { d[i] = b'0' + (n % 10) as u8; i += 1; n /= 10; if n == 0 { break; } }
    while i > 0 { i -= 1; v.push(d[i]); }
}

/// Per-mount option list: `rw|ro` then the nosuid/nodev/... flags.
fn flag_opts(flags: u8) -> Vec<u8> {
    let mut v: Vec<u8> = (if flags & MNT_RDONLY != 0 { &b"ro"[..] } else { &b"rw"[..] }).to_vec();
    for (bit, name) in [(MNT_NOSUID, &b",nosuid"[..]), (MNT_NODEV, b",nodev"), (MNT_NOEXEC, b",noexec"),
                        (MNT_NOATIME, b",noatime"), (MNT_NODIRATIME, b",nodiratime")] {
        if flags & bit != 0 { v.extend_from_slice(name); }
    }
    if flags & MNT_STRICTATIME == 0 && flags & MNT_NOATIME == 0 { v.extend_from_slice(b",relatime"); }
    v
}

/// Linux escapes space, tab, newline and backslash as octal in path fields.
fn push_esc(v: &mut Vec<u8>, s: &[u8]) {
    for &c in s {
        if matches!(c, b' ' | b'\t' | b'\n' | b'\\') {
            v.extend_from_slice(&[b'\\', b'0' + (c >> 6), b'0' + ((c >> 3) & 7), b'0' + (c & 7)]);
        } else { v.push(c); }
    }
}

/// `/proc/<pid>/mountinfo` (`info`) or `/proc/<pid>/mounts` for a namespace
/// that has a tree; None when `ns` has none (the caller renders ns 0 itself).
pub fn render_tree(ns: NsId, info: bool) -> Option<Vec<u8>> {
    let g = NS.lock();
    let t = &g.get(&ns)?.mnts;
    let mut out = Vec::new();
    for (i, m) in t.iter().enumerate() {
        let mp = ns_mountpoint(t, i);
        let dev = sb_dev(m.sb).unwrap_or(m.id);
        // Never print the hidden pool: the source of a pool-backed mount is its
        // fstype, and its root is relative to the instance directory.
        let src: &[u8] = if m.device.is_empty() { b"none" }
            else if is_sb_path(&m.device) { m.fstype.as_bytes() } else { &m.device };
        let shown_root: Vec<u8> = if is_sb_path(&m.root) {
            let mut cnt = 0usize;
            let mut at = m.root.len();
            for (i, &c) in m.root.iter().enumerate() {
                if c == b'/' { cnt += 1; if cnt == 4 { at = i; break; } }
            }
            if at >= m.root.len() { b"/".to_vec() } else { m.root[at..].to_vec() }
        } else { m.root.to_vec() };
        if info {
            push_u32(&mut out, m.id);
            out.push(b' ');
            push_u32(&mut out, if m.parent == 0 { m.id } else { m.parent });
            out.push(b' ');
            push_u32(&mut out, dev >> 8); out.push(b':'); push_u32(&mut out, dev & 0xff);
            out.push(b' ');
            push_esc(&mut out, &shown_root);
            out.push(b' ');
            push_esc(&mut out, &mp);
            out.push(b' ');
            out.extend_from_slice(&flag_opts(m.flags));
            if m.group != 0 { out.extend_from_slice(b" shared:"); push_u32(&mut out, m.group); }
            if m.master != 0 { out.extend_from_slice(b" master:"); push_u32(&mut out, m.master); }
            if m.unbindable { out.extend_from_slice(b" unbindable"); }
            out.extend_from_slice(b" - ");
            out.extend_from_slice(m.fstype.as_bytes());
            out.push(b' ');
            push_esc(&mut out, src);
            out.extend_from_slice(if m.flags & MNT_RDONLY != 0 { b" ro\n" } else { b" rw\n" });
        } else {
            push_esc(&mut out, src);
            out.push(b' ');
            push_esc(&mut out, &mp);
            out.push(b' ');
            out.extend_from_slice(m.fstype.as_bytes());
            out.push(b' ');
            out.extend_from_slice(&flag_opts(m.flags));
            out.extend_from_slice(b" 0 0\n");
        }
    }
    Some(out)
}

// ── Superblock registry ──────────────────────────────────────────────────────

pub struct SbEntry {
    pub id: SbId,
    pub kind: &'static str,
    /// Backing root `/.leandros-sb/<kind>/<n>`.
    pub root: Vec<u8>,
    /// Anonymous minor of `st_dev`.
    pub dev: u32,
    pub refs: u32,
    /// Called (outside the registry lock) with the backing root when the last
    /// mount node goes.
    pub destroy: Option<fn(&[u8])>,
}

pub const SB_DEV_BASE: u32 = 0x1000;
static SB: Mutex<Vec<SbEntry>> = Mutex::new(Vec::new());
static SB_NEXT: AtomicU32 = AtomicU32::new(1);

/// Create a superblock instance of `kind`; returns its id and backing root.
/// Starts with zero references: [`sb_get`] it for each mount node. Ids below
/// 1 are reserved: 0 means "no superblock" (ns-0 synthesized mounts).
pub fn sb_create(kind: &'static str, destroy: Option<fn(&[u8])>) -> (SbId, Vec<u8>) {
    let id = SB_NEXT.fetch_add(1, Ordering::Relaxed);
    let mut root = Vec::with_capacity(SB_ROOT.len() + kind.len() + 12);
    root.extend_from_slice(SB_ROOT);
    root.push(b'/');
    root.extend_from_slice(kind.as_bytes());
    root.push(b'/');
    let mut digits = [0u8; 10]; let mut n = id; let mut d = 0;
    loop { digits[d] = b'0' + (n % 10) as u8; d += 1; n /= 10; if n == 0 { break; } }
    for i in (0..d).rev() { root.push(digits[i]); }
    SB.lock().push(SbEntry { id, kind, root: root.clone(), dev: SB_DEV_BASE + id, refs: 0, destroy });
    (id, root)
}

pub fn sb_get(id: SbId) {
    if id == 0 { return; }
    if let Some(e) = SB.lock().iter_mut().find(|e| e.id == id) { e.refs += 1; }
}

/// Drop one reference; at zero the entry is removed and its destroy hook
/// runs. True when the superblock was destroyed.
pub fn sb_put(id: SbId) -> bool {
    if id == 0 { return false; }
    let dead = {
        let mut t = SB.lock();
        let Some(i) = t.iter().position(|e| e.id == id) else { return false };
        if t[i].refs > 1 { t[i].refs -= 1; None } else { Some(t.remove(i)) }
    };
    match dead { Some(e) => { if let Some(f) = e.destroy { f(&e.root); } true } None => false }
}

pub fn sb_root(id: SbId) -> Option<Vec<u8>> { SB.lock().iter().find(|e| e.id == id).map(|e| e.root.clone()) }
pub fn sb_dev(id: SbId) -> Option<u32> { SB.lock().iter().find(|e| e.id == id).map(|e| e.dev) }
pub fn sb_refs(id: SbId) -> Option<u32> { SB.lock().iter().find(|e| e.id == id).map(|e| e.refs) }
/// Number of live superblock instances (baseline checks).
pub fn sb_count() -> usize { SB.lock().len() }
/// Does backing path `p` lie in the hidden superblock space?
pub fn is_sb_path(p: &[u8]) -> bool { under(p, SB_ROOT) }

// ── Symlink scan (LINKSCAN) ──────────────────────────────────────────────────

/// Non-following scan of `backing` below `floor` (the mount root): `None` when
/// no component is a symlink, else `(end offset in backing of the link's
/// component, link body)`. With `follow == false` the final component is not
/// examined. Implemented by tmpfs, f2fs, the initrd ramfs and overlay.
pub type LinkScan = fn(backing: &[u8], floor: &[u8], follow: bool) -> Option<(usize, Vec<u8>)>;

static LINKSCANNERS: Mutex<Vec<(Vec<u8>, LinkScan)>> = Mutex::new(Vec::new());

/// Register `f` as the scanner for backing paths under `prefix`.
pub fn register_linkscan(prefix: &[u8], f: LinkScan) {
    let mut t = LINKSCANNERS.lock();
    if let Some(e) = t.iter_mut().find(|e| e.0 == prefix) { e.1 = f; } else { t.push((prefix.to_vec(), f)); }
}

/// Run the scanner owning `backing` (longest registered prefix). `None` when
/// no scanner is registered or the path holds no link.
pub fn linkscan(backing: &[u8], floor: &[u8], follow: bool) -> Option<(usize, Vec<u8>)> {
    let f = { LINKSCANNERS.lock().iter().filter(|e| under(backing, &e.0)).max_by_key(|e| e.0.len()).map(|e| e.1) }?;
    f(backing, floor, follow)
}

// ── Per-call path context (mntns != 0 only) ─────────────────────────────────

/// What the kernel knows about a path buffer handed to the VFS: its namespace
/// path and final mount id. Keyed by THREAD pid (mntns is per-thread; the VFS
/// canonicalizes callers to the tgid, so it cannot be keyed there).
#[derive(Clone)]
pub struct CallCtx { pub ptr: usize, pub ns_path: Vec<u8>, pub mnt_id: u32 }

static CALLCTX: Mutex<BTreeMap<u32, Vec<CallCtx>>> = Mutex::new(BTreeMap::new());

pub fn callctx_set(pid: u32, c: CallCtx) {
    let mut g = CALLCTX.lock();
    let v = g.entry(pid).or_insert_with(Vec::new);
    if v.len() >= 2 { v.remove(0); }
    v.push(c);
}

pub fn callctx_clear(pid: u32, ptr: usize) {
    let mut g = CALLCTX.lock();
    if let Some(v) = g.get_mut(&pid) {
        v.retain(|c| c.ptr != ptr);
        if v.is_empty() { g.remove(&pid); }
    }
}

pub fn callctx_get(pid: u32, ptr: usize) -> Option<CallCtx> {
    CALLCTX.lock().get(&pid).and_then(|v| v.iter().find(|c| c.ptr == ptr).cloned())
}

// ── Kernel selftest (reachable as /proc/mntselftest) ────────────────────────

fn mk(id: u32, parent: u32, mp: &[u8], root: &[u8], sb: SbId) -> Mnt {
    Mnt { id, parent, mp: mp.into(), root: root.into(), sb, flags: 0, locked: 0, group: 0, master: 0,
          unbindable: false, fstype: "test", device: Box::from(&b"t"[..]), opts: Box::from(&b""[..]) }
}

/// Walk/reverse/copy/registry checks on a synthetic tree in a scratch
/// namespace id. Returns `(case, passed)` rows.
pub fn selftest() -> Vec<(&'static str, bool)> {
    let mut out: Vec<(&'static str, bool)> = Vec::new();
    let ns: NsId = 0xFFF0;
    let (sb1, r1) = sb_create("tmpfs", None);
    sb_get(sb1);
    let mut r1d = r1.clone(); r1d.extend_from_slice(b"/d");
    // root(/ -> /) ; tmpfs at /a ; file bind at /a/f -> /etc/hosts ; nested at /a/d/n ; overmount on /a
    let t = alloc::vec![
        mk(1, 0, b"/", b"/", 0),
        mk(2, 1, b"/a", &r1, sb1),
        mk(3, 2, b"/f", b"/etc/hosts", 0),
        mk(4, 2, b"/d/n", b"/srv/n", 0),
    ];
    NS.lock().insert(ns, MntNs { owner_user: 0, mnts: t, seq: 0 });
    let chk = |p: &[u8], want: &[u8]| translate(ns, p).map(|x| x.backing == want).unwrap_or(false);
    out.push(("walk_root", chk(b"/x/y", b"/x/y")));
    out.push(("walk_mount", chk(b"/a/q", &join(&r1, b"/q"))));
    out.push(("walk_mount_exact", chk(b"/a", &r1)));
    out.push(("walk_file_bind", chk(b"/a/f", b"/etc/hosts")));
    out.push(("walk_nested", chk(b"/a/d/n/z", b"/srv/n/z")));
    out.push(("walk_boundary", chk(b"/ab", b"/ab")));
    out.push(("walk_prefix_len", translate(ns, b"/a/d/n/z").map(|x| x.ns_prefix_len == 6 && x.mnt_id == 4).unwrap_or(false)));
    out.push(("reverse_roundtrip", reverse(ns, b"/srv/n/z", 0).as_deref() == Some(&b"/a/d/n/z"[..])));
    out.push(("reverse_file_bind", reverse(ns, b"/etc/hosts", 0).as_deref() == Some(&b"/a/f"[..])));
    // overmount: a second mount stacked on /a hides the first
    with_ns(ns, |n| n.mnts.push(mk(5, 1, b"/a", b"/over", 0)));
    out.push(("overmount_top", chk(b"/a/q", b"/over/q")));
    out.push(("overmount_hides_children", chk(b"/a/f", b"/over/f")));
    with_ns(ns, |n| n.mnts.push(mk(6, 5, b"/", b"/over2", 0)));
    out.push(("overmount_stacked_on_root", chk(b"/a/x", b"/over2/x")));
    // copy keeps shape, new ids, sb refs
    let dst: NsId = 0xFFF1;
    let before = sb_refs(sb1);
    let ok = copy(ns, dst, 0, true).is_ok();
    // /a is overmounted by now, so the copy shows the same top-of-stack view.
    let same = translate(dst, b"/a/d/n/z").map(|x| x.backing == b"/over2/d/n/z" && x.mnt_id != 6).unwrap_or(false);
    out.push(("copy_shape", ok && same && chk(b"/a/x", b"/over2/x")));
    out.push(("copy_sb_ref", sb_refs(sb1) == before.map(|b| b + 1)));
    out.push(("copy_locked", with_ns(dst, |n| n.mnts.iter().all(|m| m.locked & LOCK_MNT != 0)).unwrap_or(false)));
    release(dst);
    out.push(("release_sb_ref", sb_refs(sb1) == before));
    // registry
    out.push(("sb_root_hidden", is_sb_path(&r1) && !is_sb_path(b"/.leandros-sbx") && r1.starts_with(SB_ROOT)));
    let _ = r1d;
    release(ns);
    out.push(("sb_destroyed_with_tree", sb_refs(sb1).is_none()));
    // linkscan dispatch: longest prefix wins
    fn s_a(_: &[u8], _: &[u8], _: bool) -> Option<(usize, Vec<u8>)> { Some((1, b"A".to_vec())) }
    fn s_b(_: &[u8], _: &[u8], _: bool) -> Option<(usize, Vec<u8>)> { Some((2, b"B".to_vec())) }
    register_linkscan(b"/.mnttest-ls", s_a);
    register_linkscan(b"/.mnttest-ls/in", s_b);
    out.push(("linkscan_longest_prefix", linkscan(b"/.mnttest-ls/in/x", b"/", true).map(|r| r.0 == 2).unwrap_or(false)
        && linkscan(b"/.mnttest-ls/x", b"/", true).map(|r| r.0 == 1).unwrap_or(false)));
    LINKSCANNERS.lock().retain(|e| !e.0.starts_with(b"/.mnttest-ls"));
    // callctx keyed by pid
    callctx_set(7777, CallCtx { ptr: 5, ns_path: b"/x".to_vec(), mnt_id: 9 });
    let g = callctx_get(7777, 5).map(|c| c.mnt_id == 9).unwrap_or(false) && callctx_get(7778, 5).is_none();
    callctx_clear(7777, 5);
    out.push(("callctx", g && callctx_get(7777, 5).is_none()));
    out
}

/// The ns-0 table: alias mounts and binds (the former alias.rs).
pub mod ns0 {
// Alias mounts: `proc`, `devtmpfs`, `sysfs` and bind mounts.
//
// LeandrOS has a single filesystem namespace in which `/proc`, `/dev` and
// the synthesized parts of `/sys` are not mounts but path prefixes the VFS
// recognises (`should_lookup_ramfs`). A chroot therefore saw none of them:
// inside a jail at `/root/sysroot`, `/proc/self/exe` became the host path
// `/root/sysroot/proc/self/exe`, which lands on the jail's empty f2fs
// directory.
//
// An alias entry maps a host-absolute *mount point* onto a host-absolute
// *source* prefix: `mount -t proc proc /root/sysroot/proc` adds
// `/root/sysroot/proc -> /proc`, `mount --bind /dev /root/sysroot/dev` adds
// `/root/sysroot/dev -> /dev`. The kernel's path resolution produces the
// namespace path (which is what `..`, the chroot floor, the cwd and getcwd
// work on) and then rewrites it through [`translate`] right before the VFS
// sees it, the same point at which Linux would cross into the mounted
// filesystem during the walk. Paths reported back to userspace
// (`/proc/self/fd/N`, `/proc/self/exe`, `fchdir`) go through [`reverse`] so
// they read as the caller sees them.
//
// The table is global (there are no mount namespaces), small, and empty on
// a normal boot: `ACTIVE` lets every path operation skip the lock then.

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
/// Mount points created by binding a path onto itself.
static SELF_BINDS: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// Prefix of the synthetic source of a bind mount whose source is an nsfs
/// file: `/.leandros-ns/<link kind>/<namespace id>`. The alias holds one
/// reference on that namespace for as long as it is mounted.
pub const NS_SRC: &[u8] = b"/.leandros-ns/";

/// `(link kind, namespace id)` when `p` names a bound namespace.
pub fn ns_src(p: &[u8]) -> Option<(u8, u16)> {
    let r = p.strip_prefix(NS_SRC)?;
    let sl = r.iter().position(|&c| c == b'/')?;
    let k: u8 = core::str::from_utf8(&r[..sl]).ok()?.parse().ok()?;
    let id: u16 = core::str::from_utf8(&r[sl + 1..]).ok()?.parse().ok()?;
    Some((k, id))
}

/// Mount points made by self-binds (for `ns0_mountpoints`).
pub fn self_binds() -> Vec<Vec<u8>> { SELF_BINDS.lock().clone() }
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
    if mountpoint == source {
        // A self-bind (`mount --bind /run/netns /run/netns`): the path stays
        // as it is, but becomes a mount point so its propagation can change.
        let mut sb = SELF_BINDS.lock();
        if !sb.iter().any(|m| m.as_slice() == mountpoint) {
            if sb.len() >= MAX_ALIASES { return Err(-28); }
            sb.push(mountpoint.to_vec());
        }
        return Ok(());
    }
    if under(source, mountpoint) { return Err(-22); }
    let mut t = ALIASES.lock();
    if let Some(a) = t.iter_mut().find(|a| a.mountpoint == mountpoint) {
        let old_dp = crate::devpts::inst_of_backing(&a.source);
        let old = tmpfs_inst_of(&a.source);
        if let Some((_, id)) = ns_src(&a.source) { crate::sched_ns_put(id); }
        a.source = source.to_vec();
        a.fstype = fstype;
        a.device = device.to_vec();
        if let Some(n) = tmpfs_inst_of(source) { super::sb_get(n); }
        if let Some(n) = crate::devpts::inst_of_backing(source) { crate::devpts::get(n); }
        drop(t);
        if let Some(n) = old { super::sb_put(n); }
        if let Some(n) = old_dp { crate::devpts::put(n); }
        return Ok(());
    }
    if t.len() >= MAX_ALIASES { return Err(-28); }
    t.push(Alias { mountpoint: mountpoint.to_vec(), source: source.to_vec(), fstype,
                   device: device.to_vec() });
    ACTIVE.store(t.len(), Ordering::Release);
    // A mount holds a reference on the tmpfs instance it exposes.
    if let Some(n) = tmpfs_inst_of(source) { super::sb_get(n); }
    if let Some(n) = crate::devpts::inst_of_backing(source) { crate::devpts::get(n); }
    Ok(())
}

/// The superblock id when `p` names (or lies below) a tmpfs instance root
/// `/.leandros-sb/tmpfs/<id>`.
pub fn tmpfs_inst_of(p: &[u8]) -> Option<u32> {
    let r = p.strip_prefix(b"/.leandros-sb/tmpfs/".as_slice())?;
    let end = r.iter().position(|&c| c == b'/').unwrap_or(r.len());
    core::str::from_utf8(&r[..end]).ok()?.parse().ok()
}

/// Remove the alias mounted exactly at `mountpoint`. False when none is.
pub fn remove(mountpoint: &[u8]) -> bool {
    {
        let mut sb = SELF_BINDS.lock();
        if let Some(i) = sb.iter().position(|m| m.as_slice() == mountpoint) { sb.remove(i); return true; }
    }
    let mut t = ALIASES.lock();
    let nsid = t.iter().find(|a| a.mountpoint == mountpoint).and_then(|a| ns_src(&a.source));
    let before = t.len();
    let gone = t.iter().find(|a| a.mountpoint == mountpoint).and_then(|a| crate::devpts::inst_of_backing(&a.source));
    let tmp_inst = t.iter().find(|a| a.mountpoint == mountpoint).and_then(|a| tmpfs_inst_of(&a.source));
    t.retain(|a| a.mountpoint != mountpoint);
    ACTIVE.store(t.len(), Ordering::Release);
    let removed = t.len() != before;
    drop(t);
    // A devpts instance lives as long as its last mount (copies hold refs).
    if let Some(id) = gone { crate::devpts::put(id); }
    // The last mount of a tmpfs instance frees it.
    if let Some(n) = tmp_inst { super::sb_put(n); }
    if removed { if let Some((_, id)) = nsid { crate::sched_ns_put(id); } }
    removed
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

/// Does namespace path `path` lie under some alias mount point? Every alias
/// mount is nosuid: they put the kernel's own trees (/proc, /dev, /sys) and
/// bind-mounted host directories (/tmp, /home) into chroot jails, and no
/// set-ID program should gain privileges from a path that only exists to
/// make a jail work. /proc/mounts lists them `nosuid`.
pub fn crosses(path: &[u8]) -> bool {
    if ACTIVE.load(Ordering::Acquire) == 0 { return false; }
    ALIASES.lock().iter().any(|a| under(path, &a.mountpoint))
}

/// The mount point of the alias whose mount point is exactly `path`.
pub fn is_mountpoint(path: &[u8]) -> bool {
    if ACTIVE.load(Ordering::Acquire) == 0 { return false; }
    ALIASES.lock().iter().any(|a| a.mountpoint == path) || SELF_BINDS.lock().iter().any(|m| m.as_slice() == path)
}

/// Snapshot `(mountpoint, source, fstype, device)` of every alias.
pub fn entries() -> Vec<(Vec<u8>, Vec<u8>, &'static str, Vec<u8>)> {
    if ACTIVE.load(Ordering::Acquire) == 0 { return Vec::new(); }
    ALIASES.lock().iter().map(|a| (a.mountpoint.clone(), a.source.clone(), a.fstype, a.device.clone())).collect()
}

/// Snapshot `(device, mountpoint, fstype)` of every alias, for /proc/mounts.
pub fn list() -> Vec<(Vec<u8>, Vec<u8>, &'static str)> {
    let mut v: Vec<_> = ALIASES.lock().iter().map(|a| (a.device.clone(), a.mountpoint.clone(), a.fstype)).collect();
    for m in SELF_BINDS.lock().iter() { v.push((b"none".to_vec(), m.clone(), "none")); }
    v
}

}
