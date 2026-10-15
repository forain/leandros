//! The tmpfs pool: `MAX_TMP_FILES` slots shared by every /tmp, /dev/shm,
//! /run/user name, memfd, dma-buf export and ephemeral /proc snapshot.
//!
//! Lane tmpfast (2026-10-15). Growing the pool 512 -> 4096 (a9c4743d) made
//! every tmpfs lookup ~8x slower, because each one walked all 4096 slots of
//! ~2.5 KiB (a 2 KiB xattr arena sat inline in every entry), and a path walk
//! does one lookup per component. The pool therefore owns its bookkeeping:
//!
//! * a name index (open addressing, linear probing, backward-shift delete)
//!   from path to slot, holding exactly the *named* slots (`in_use &&
//!   !ephemeral`). `find` is O(1) whether or not the name exists;
//! * `phash[i]`, the hash of slot i's parent path (0 when unnamed), so
//!   getdents walks a dense `u32` array and only reads candidate entries;
//! * an in-use bitmap: free-slot search and whole-pool walks skip empty
//!   slots 64 at a time;
//! * `nalias[o]`, the number of in-use slots whose `link_to` names `o`
//!   (what `tmp_alias_count` used to count with a full scan on every stat);
//! * the xattr arenas, moved to a side table so the hot entry is ~500 B.
//!
//! The fields those structures are derived from — `path`, `in_use`,
//! `ephemeral`, `link_to` — are private to this module, and an entry cannot
//! be built outside it, so every change to them goes through a `TmpPool`
//! method and the index cannot drift. `TmpPool` derefs to the entry slice,
//! so all other fields are read and written as before.

use core::ops::{Deref, DerefMut};

pub(crate) const MAX_TMP_FILES: usize = 4096;
pub(crate) const MAX_TMP_PATH: usize = 128;
/// Longest symlink body. Matches `read_cstr_raw`'s 256-byte buffer, which is
/// the most `symlink(2)` can ever hand us.
pub(crate) const TMP_LINK_MAX: usize = 256;

const IDX_SLOTS: usize = MAX_TMP_FILES * 2; // load factor <= 1/2
const IDX_MASK: usize = IDX_SLOTS - 1;
const _: () = assert!(IDX_SLOTS.is_power_of_two());
const _: () = assert!(MAX_TMP_FILES < u16::MAX as usize);
const _: () = assert!(MAX_TMP_FILES % 64 == 0);

pub(crate) struct TmpFileEntry {
    path:     [u8; MAX_TMP_PATH],
    path_len: usize,
    in_use:   bool,
    /// Synthetic /proc snapshot parked in the pool under a fake "/tmp/.proc_N"
    /// path, or a nameless inode (unlinked while open or while aliases
    /// remain). Owned by the fd rather than by a name: invisible to lookup and
    /// to getdents64, and freed on close (see `tmp_release_ephemeral`).
    ephemeral: bool,
    /// Hard-link indirection, stored as owner + 1 so that 0 means "owns its
    /// own bytes" (see `tmp_owner`). Directories are never hard-linked.
    link_to:  usize,
    /// Symlink body (`is_link` only; `len` is its length). A regular file's
    /// bytes are never stored here — they live in `TMP_VMOS[owner]`.
    pub(crate) link:     [u8; TMP_LINK_MAX],
    /// Logical size (EOF) of a regular file, or the symlink body length.
    /// Authoritative; mirrored into `TmpVmo.len` whenever a VMO exists.
    pub(crate) len:      usize,
    pub(crate) is_dir:   bool,
    /// mknod(S_IFIFO) node — reported as S_IFIFO/DT_FIFO, no FIFO semantics.
    pub(crate) is_fifo:  bool,
    /// symlink(2) node; `link[..len]` is the target verbatim.
    pub(crate) is_link:  bool,
    /// AF_UNIX bound-socket node; `sock_id` names the net server's listener.
    pub(crate) is_sock:  bool,
    pub(crate) sock_id:  u64,
    pub(crate) mode:     u32, // permission bits (rwxrwxrwx), set at creation from umask
    pub(crate) uid:      u32, // owner, set at creation from the creating task's euid
    pub(crate) gid:      u32, // owner group, set at creation from the creating task's egid
    /// `(sec, nsec)` on this kernel's CLOCK_REALTIME. Belong to the
    /// data-owning slot.
    pub(crate) atime: (i64, i64),
    pub(crate) mtime: (i64, i64),
    pub(crate) ctime: (i64, i64),
}

impl TmpFileEntry {
    const fn empty() -> Self {
        Self { path: [0u8; MAX_TMP_PATH], path_len: 0, in_use: false, ephemeral: false,
               link_to: 0, link: [0u8; TMP_LINK_MAX], len: 0,
               is_dir: false, is_fifo: false, is_link: false, is_sock: false, sock_id: 0,
               mode: 0, uid: 0, gid: 0,
               atime: (0, 0), mtime: (0, 0), ctime: (0, 0) }
    }
    #[inline] pub(crate) fn path(&self) -> &[u8] { &self.path[..self.path_len] }
    #[inline] pub(crate) fn in_use(&self) -> bool { self.in_use }
    #[inline] pub(crate) fn ephemeral(&self) -> bool { self.ephemeral }
    /// Raw hard-link field: owner + 1, or 0 when this slot owns its bytes.
    #[inline] pub(crate) fn link_to(&self) -> usize { self.link_to }
    #[inline] fn named(&self) -> bool { self.in_use && !self.ephemeral }
}

/// FNV-1a, never 0 (0 marks "no parent hash" in `phash`).
fn hash(p: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in p { h = (h ^ b as u32).wrapping_mul(0x0100_0193); }
    if h == 0 { 1 } else { h }
}

/// Parent of a stored path ("/tmp/d/f" -> "/tmp/d"); same rule as `tmp_parent`
/// minus the tmpfs-root special case, which only matters for "/tmp" itself
/// (never a pool entry).
fn parent_of(p: &[u8]) -> &[u8] {
    match p.iter().rposition(|&b| b == b'/') {
        Some(0) => &p[..1],
        Some(i) => &p[..i],
        None => b"",
    }
}

pub(crate) struct TmpPool {
    e:      [TmpFileEntry; MAX_TMP_FILES],
    /// Per-inode xattr arena (see the `xattr` crate). Belongs to the
    /// data-owning slot: every xattr op maps through `tmp_owner()` first.
    xattr:  [[u8; xattr::TMP_XATTR_ARENA]; MAX_TMP_FILES],
    /// `hash(path)` of a named slot (stale otherwise).
    hash:   [u32; MAX_TMP_FILES],
    /// `hash(parent(path))` of a named slot, 0 otherwise.
    phash:  [u32; MAX_TMP_FILES],
    /// In-use slots whose `link_to` is `i + 1`.
    nalias: [u16; MAX_TMP_FILES],
    used:   [u64; MAX_TMP_FILES / 64],
    /// Name index: slot + 1, 0 = empty.
    table:  [u16; IDX_SLOTS],
    /// In-use slot count.
    count:  usize,
}

impl Deref for TmpPool {
    type Target = [TmpFileEntry];
    #[inline] fn deref(&self) -> &[TmpFileEntry] { &self.e }
}
impl DerefMut for TmpPool {
    #[inline] fn deref_mut(&mut self) -> &mut [TmpFileEntry] { &mut self.e }
}

impl TmpPool {
    pub(crate) const fn new() -> Self {
        Self { e: [const { TmpFileEntry::empty() }; MAX_TMP_FILES],
               xattr: [[0u8; xattr::TMP_XATTR_ARENA]; MAX_TMP_FILES],
               hash: [0; MAX_TMP_FILES], phash: [0; MAX_TMP_FILES],
               nalias: [0; MAX_TMP_FILES], used: [0; MAX_TMP_FILES / 64],
               table: [0; IDX_SLOTS], count: 0 }
    }

    // ── name index ──────────────────────────────────────────────────────────

    fn index_insert(&mut self, idx: usize) {
        let h = hash(self.e[idx].path());
        self.hash[idx] = h;
        self.phash[idx] = hash(parent_of(self.e[idx].path()));
        let mut s = h as usize & IDX_MASK;
        while self.table[s] != 0 { s = (s + 1) & IDX_MASK; }
        self.table[s] = idx as u16 + 1;
    }

    fn index_remove(&mut self, idx: usize) {
        self.phash[idx] = 0;
        let mut s = self.hash[idx] as usize & IDX_MASK;
        loop {
            match self.table[s] {
                0 => return, // not indexed (cannot happen for a named slot)
                v if v as usize == idx + 1 => break,
                _ => s = (s + 1) & IDX_MASK,
            }
        }
        // Backward-shift deletion: pull later members of the probe run into
        // the hole whenever the hole lies on their path from home.
        let mut hole = s;
        let mut j = s;
        loop {
            j = (j + 1) & IDX_MASK;
            let v = self.table[j];
            if v == 0 { break; }
            let home = self.hash[v as usize - 1] as usize & IDX_MASK;
            // Can `v` (home `home`, now at `j`) move back to `hole`? Only if
            // `home` is not cyclically in (hole, j].
            let dist_home = j.wrapping_sub(home) & IDX_MASK;
            let dist_hole = j.wrapping_sub(hole) & IDX_MASK;
            if dist_home >= dist_hole {
                self.table[hole] = v;
                hole = j;
            }
        }
        self.table[hole] = 0;
    }

    /// The *named* slot with this exact path. Ephemeral /proc snapshots and
    /// nameless inodes are excluded: no user path can name them.
    pub(crate) fn find(&self, path: &[u8]) -> Option<usize> {
        let h = hash(path);
        let mut s = h as usize & IDX_MASK;
        loop {
            let v = self.table[s];
            if v == 0 { return None; }
            let i = v as usize - 1;
            if self.hash[i] == h && self.e[i].path() == path { return Some(i); }
            s = (s + 1) & IDX_MASK;
        }
    }

    // ── slot lifecycle ──────────────────────────────────────────────────────

    /// Lowest free slot, if any.
    pub(crate) fn free_slot(&self) -> Option<usize> {
        self.used.iter().enumerate()
            .find(|(_, w)| **w != u64::MAX)
            .map(|(k, w)| k * 64 + (!*w).trailing_zeros() as usize)
    }

    /// In-use slot count.
    pub(crate) fn count(&self) -> usize { self.count }

    /// Reset slot `idx` to a fresh in-use entry with `path` (all other fields
    /// zero, no xattrs). `ephemeral` claims it as an fd-owned, unnamed slot.
    /// Callers have length-checked `path` against `MAX_TMP_PATH`.
    pub(crate) fn claim(&mut self, idx: usize, path: &[u8], ephemeral: bool) {
        self.release(idx);
        let e = &mut self.e[idx];
        e.in_use = true;
        e.ephemeral = ephemeral;
        let n = path.len().min(MAX_TMP_PATH);
        e.path[..n].copy_from_slice(&path[..n]);
        e.path_len = n;
        self.used[idx / 64] |= 1u64 << (idx % 64);
        self.count += 1;
        if !ephemeral { self.index_insert(idx); }
        self.fsck("claim");
    }

    /// Free slot `idx` (no-op when already free). The caller has released
    /// the slot's VMO.
    pub(crate) fn release(&mut self, idx: usize) {
        if !self.e[idx].in_use { return; }
        if self.e[idx].named() { self.index_remove(idx); }
        let to = self.e[idx].link_to;
        if to != 0 && to - 1 < MAX_TMP_FILES {
            self.nalias[to - 1] = self.nalias[to - 1].saturating_sub(1);
        }
        self.used[idx / 64] &= !(1u64 << (idx % 64));
        self.count -= 1;
        self.e[idx] = TmpFileEntry::empty();
        if self.xattr[idx][0] != 0 || self.xattr[idx].iter().any(|&b| b != 0) {
            self.xattr[idx] = [0u8; xattr::TMP_XATTR_ARENA];
        }
        self.fsck("release");
    }

    /// Rename slot `idx` (named or not). Callers have length-checked.
    pub(crate) fn set_path(&mut self, idx: usize, path: &[u8]) {
        let named = self.e[idx].named();
        if named { self.index_remove(idx); }
        let e = &mut self.e[idx];
        e.path = [0u8; MAX_TMP_PATH];
        let n = path.len().min(MAX_TMP_PATH);
        e.path[..n].copy_from_slice(&path[..n]);
        e.path_len = n;
        if named { self.index_insert(idx); }
        self.fsck("set_path");
    }

    /// Drop a slot's name but keep the inode (see `tmp_drop_name`).
    pub(crate) fn make_nameless(&mut self, idx: usize) {
        if self.e[idx].named() { self.index_remove(idx); }
        self.e[idx].ephemeral = true;
        self.fsck("make_nameless");
    }

    /// Make in-use slot `idx` a hard-link alias of data-owning slot `owner`.
    pub(crate) fn set_link_to(&mut self, idx: usize, owner: usize) {
        let old = self.e[idx].link_to;
        if old != 0 && old - 1 < MAX_TMP_FILES {
            self.nalias[old - 1] = self.nalias[old - 1].saturating_sub(1);
        }
        self.e[idx].link_to = owner + 1;
        self.nalias[owner] += 1;
        self.fsck("set_link_to");
    }

    /// Number of in-use slots aliasing data-owning slot `owner`.
    #[inline] pub(crate) fn alias_count(&self, owner: usize) -> usize { self.nalias[owner] as usize }

    // ── walks ───────────────────────────────────────────────────────────────

    /// In-use slot indices from `start`, ascending, skipping free slots a
    /// bitmap word at a time.
    pub(crate) fn used_from(&self, start: usize) -> impl Iterator<Item = usize> + '_ {
        let mut i = start;
        core::iter::from_fn(move || {
            while i < MAX_TMP_FILES {
                let w = self.used[i / 64] >> (i % 64);
                if w == 0 { i = (i / 64 + 1) * 64; continue; }
                let j = i + w.trailing_zeros() as usize;
                i = j + 1;
                return Some(j);
            }
            None
        })
    }

    /// Named slots from `start` whose parent path is `dir` — exactly, after
    /// the hash pre-filter. Ascending slot order (getdents' cursor).
    pub(crate) fn children_from<'a>(&'a self, dir: &'a [u8], start: usize)
        -> impl Iterator<Item = usize> + 'a
    {
        let ph = hash(dir);
        self.used_from(start)
            .filter(move |&i| self.phash[i] == ph && parent_of(self.e[i].path()) == dir
                    && self.e[i].named())
    }

    // ── xattrs ──────────────────────────────────────────────────────────────

    #[inline] pub(crate) fn xattr(&self, idx: usize) -> &[u8; xattr::TMP_XATTR_ARENA] { &self.xattr[idx] }
    #[inline] pub(crate) fn xattr_mut(&mut self, idx: usize) -> &mut [u8; xattr::TMP_XATTR_ARENA] { &mut self.xattr[idx] }
    /// Entry `idx` and its xattr arena, both mutable.
    #[inline] pub(crate) fn entry_xattr_mut(&mut self, idx: usize)
        -> (&mut TmpFileEntry, &mut [u8; xattr::TMP_XATTR_ARENA])
    {
        (&mut self.e[idx], &mut self.xattr[idx])
    }

    // ── self-check ──────────────────────────────────────────────────────────

    /// Test builds only: `LEANDROS_TMPFSCK=1 ./scripts/build-all.sh` runs
    /// `verify` after every mutation and reports drift on the serial log.
    /// Compiled out otherwise.
    #[inline]
    fn fsck(&self, what: &str) {
        if option_env!("LEANDROS_TMPFSCK").is_some() {
            let bad = self.verify();
            if bad != 0 {
                crate::dbg_str("[VFS] tmpfsck FAILED after ");
                crate::dbg_str(what);
                crate::dbg_str(": ");
                crate::dbg_dec(bad);
                crate::dbg_str("\n");
            }
        }
    }

    /// Recompute every derived structure from the entries and compare.
    /// Returns the number of inconsistencies (0 = healthy). O(N); used by the
    /// census dump and by `tmpfsck` in vfstest.
    pub(crate) fn verify(&self) -> usize {
        let mut bad = 0usize;
        let mut n = 0usize;
        let mut indexed = 0usize;
        let (mut links, mut total_alias) = (0usize, 0usize);
        for i in 0..MAX_TMP_FILES {
            let e = &self.e[i];
            let bit = self.used[i / 64] >> (i % 64) & 1 == 1;
            if bit != e.in_use { bad += 1; }
            if e.in_use { n += 1; }
            if e.named() {
                indexed += 1;
                if self.find(e.path()).is_none() { bad += 1; }
                if self.phash[i] != hash(parent_of(e.path())) { bad += 1; }
            } else if self.phash[i] != 0 { bad += 1; }
            if e.in_use && e.link_to != 0 {
                links += 1;
                if e.link_to - 1 >= MAX_TMP_FILES || self.nalias[e.link_to - 1] == 0 { bad += 1; }
            }
            total_alias += self.nalias[i] as usize;
        }
        if links != total_alias { bad += 1; }
        if n != self.count { bad += 1; }
        if self.table.iter().filter(|&&v| v != 0).count() != indexed { bad += 1; }
        bad
    }
}
