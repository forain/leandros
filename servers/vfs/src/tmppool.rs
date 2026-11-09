//! The tmpfs pool: up to `MAX_TMP_FILES` slots shared by every /tmp, /dev/shm,
//! /run/user name, memfd, dma-buf export and ephemeral /proc snapshot. The
//! per-slot arrays start empty and double on demand (lane tmpfsgrow); a slot
//! index is an inode's identity, so slots never move and the pool never shrinks.
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
//! * the xattr arenas, moved to a side table and allocated on first write
//!   (a fixed 2 KiB arena per slot was 8 MiB of BSS, almost all of it zero);
//! * a child index: every named slot is linked into its parent directory's
//!   child list (`parent`/`first`/`next`/`prev`), so "is this directory
//!   empty?" (rmdir, rename onto a directory) is O(1) and a directory
//!   rename visits exactly its subtree instead of the whole pool.
//!
//! Paths and symlink bodies are heap-allocated to their real length, so the
//! limits are Linux's (PATH_MAX, symlink bodies up to PATH_MAX - 1) rather
//! than what a fixed inline buffer per slot could afford.
//!
//! The fields those structures are derived from — `path`, `in_use`,
//! `ephemeral`, `link_to` — are private to this module, and an entry cannot
//! be built outside it, so every change to them goes through a `TmpPool`
//! method and the index cannot drift. `TmpPool` derefs to the entry slice,
//! so all other fields are read and written as before.

use core::ops::{Deref, DerefMut};

/// Ceiling of the growable pool (was a fixed 4096; lane tmpfsgrow, L5.8).
/// Slot links are u16 (NIL = u16::MAX, the name index stores slot + 1) and a
/// VMO identity keeps the slot in its low 16 bits, so 32768 is the largest
/// power of two (the name index masks) that fits both.
pub(crate) const MAX_TMP_FILES: usize = 32768;
/// Slots a fresh pool grows to on its first claim; every growth doubles.
const FIRST_CAP: usize = 512;
/// A stored path is shorter than this (PATH_MAX, NUL included).
pub(crate) const MAX_TMP_PATH: usize = crate::PATH_MAX;
/// Longest symlink body: PATH_MAX - 1, as on Linux (`symlink(2)` takes a
/// NUL-terminated string of at most PATH_MAX bytes).
pub(crate) const TMP_LINK_MAX: usize = crate::PATH_MAX - 1;
/// "No slot" in the child index.
const NIL: u16 = u16::MAX;

type XattrArena = [u8; xattr::TMP_XATTR_ARENA];
/// What a slot without an arena reads as: no attributes.
static EMPTY_XATTR: XattrArena = [0u8; xattr::TMP_XATTR_ARENA];

const _: () = assert!(MAX_TMP_FILES.is_power_of_two() && FIRST_CAP.is_power_of_two());
const _: () = assert!(MAX_TMP_FILES < u16::MAX as usize);
const _: () = assert!(FIRST_CAP % 64 == 0 && FIRST_CAP <= MAX_TMP_FILES);

pub(crate) struct TmpFileEntry {
    path:     alloc::vec::Vec<u8>,
    in_use:   bool,
    /// Synthetic /proc snapshot parked in the pool under a fake "/tmp/.proc_N"
    /// path, or a nameless inode (unlinked while open or while aliases
    /// remain). Owned by the fd rather than by a name: invisible to lookup and
    /// to getdents64, and freed on close (see `tmp_release_ephemeral`).
    ephemeral: bool,
    /// Lost its name while still referenced (`make_nameless`): readlink of
    /// a descriptor reads `<path> (deleted)`, as Linux d_path.
    unlinked: bool,
    /// Hard-link indirection, stored as owner + 1 so that 0 means "owns its
    /// own bytes" (see `tmp_owner`). Directories are never hard-linked.
    link_to:  usize,
    /// Symlink body (`is_link` only; `len` is its length). A regular file's
    /// bytes are never stored here — they live in `TMP_VMOS[owner]`.
    pub(crate) link:     alloc::vec::Vec<u8>,
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
    /// mknod(S_IFCHR / S_IFBLK) node: 0 = not a device, 1 = char, 2 = block.
    /// `rdev` is the Linux `dev_t` (see `makedev`). Opening one is redirected
    /// to the driver that owns that number (`devnode_open`).
    pub(crate) devtype:  u8,
    pub(crate) rdev:     u32,
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
        Self { path: alloc::vec::Vec::new(), in_use: false, ephemeral: false, unlinked: false,
               link_to: 0, link: alloc::vec::Vec::new(), len: 0,
               is_dir: false, is_fifo: false, is_link: false, is_sock: false, sock_id: 0, devtype: 0, rdev: 0,
               mode: 0, uid: 0, gid: 0,
               atime: (0, 0), mtime: (0, 0), ctime: (0, 0) }
    }
    #[inline] pub(crate) fn path(&self) -> &[u8] { &self.path }
    #[inline] pub(crate) fn in_use(&self) -> bool { self.in_use }
    #[inline] pub(crate) fn ephemeral(&self) -> bool { self.ephemeral }
    #[inline] pub(crate) fn unlinked(&self) -> bool { self.unlinked }
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
    e:      alloc::vec::Vec<TmpFileEntry>,
    /// Per-inode xattr arena (see the `xattr` crate), allocated on the first
    /// mutable access. Belongs to the data-owning slot: every xattr op maps
    /// through `tmp_owner()` first.
    xattr:  alloc::vec::Vec<Option<alloc::boxed::Box<XattrArena>>>,
    /// Child index of the named slots. `parent[i]`: the named directory slot
    /// whose path is `parent_of(path(i))`, NIL when that is no pool slot (a
    /// tmpfs root such as /tmp). `first[d]`: head of `d`'s child list;
    /// `next`/`prev`: siblings. Maintained with the name index.
    parent: alloc::vec::Vec<u16>,
    first:  alloc::vec::Vec<u16>,
    next:   alloc::vec::Vec<u16>,
    prev:   alloc::vec::Vec<u16>,
    /// `hash(path)` of a named slot (stale otherwise).
    hash:   alloc::vec::Vec<u32>,
    /// `hash(parent(path))` of a named slot, 0 otherwise.
    phash:  alloc::vec::Vec<u32>,
    /// In-use slots whose `link_to` is `i + 1`.
    nalias: alloc::vec::Vec<u16>,
    used:   alloc::vec::Vec<u64>,
    /// Name index: slot + 1, 0 = empty. `2 * capacity` entries (0 while the
    /// pool is still empty), rebuilt by `grow_to`.
    table:  alloc::vec::Vec<u16>,
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
        use alloc::vec::Vec;
        Self { e: Vec::new(), xattr: Vec::new(),
               parent: Vec::new(), first: Vec::new(), next: Vec::new(), prev: Vec::new(),
               hash: Vec::new(), phash: Vec::new(), nalias: Vec::new(), used: Vec::new(),
               table: Vec::new(), count: 0 }
    }

    // ── growth ──────────────────────────────────────────────────────────────

    /// Capacity the next growth reaches; `None` at `MAX_TMP_FILES`.
    pub(crate) fn next_cap(&self) -> Option<usize> {
        let cap = self.e.len();
        if cap >= MAX_TMP_FILES { return None; }
        Some(if cap == 0 { FIRST_CAP } else { (cap * 2).min(MAX_TMP_FILES) })
    }

    /// Grow every per-slot array to `cap` slots (all new slots free) and
    /// rebuild the name index at `2 * cap`. Slot indices never move, so every
    /// `TmpFile { idx }`, inode number and VMO identity stays valid. Returns
    /// false, with the pool unchanged, when the heap cannot supply the memory.
    pub(crate) fn grow_to(&mut self, cap: usize) -> bool {
        let old = self.e.len();
        if cap <= old || cap > MAX_TMP_FILES || !cap.is_power_of_two() || cap % 64 != 0 {
            return false;
        }
        let add = cap - old;
        let mut table: alloc::vec::Vec<u16> = alloc::vec::Vec::new();
        if self.e.try_reserve_exact(add).is_err()
            || self.xattr.try_reserve_exact(add).is_err()
            || self.parent.try_reserve_exact(add).is_err()
            || self.first.try_reserve_exact(add).is_err()
            || self.next.try_reserve_exact(add).is_err()
            || self.prev.try_reserve_exact(add).is_err()
            || self.hash.try_reserve_exact(add).is_err()
            || self.phash.try_reserve_exact(add).is_err()
            || self.nalias.try_reserve_exact(add).is_err()
            || self.used.try_reserve_exact(add / 64).is_err()
            || table.try_reserve_exact(cap * 2).is_err()
        {
            return false; // lengths untouched: the pool is exactly as before
        }
        self.e.resize_with(cap, TmpFileEntry::empty);
        self.xattr.resize_with(cap, || None);
        self.parent.resize(cap, NIL);
        self.first.resize(cap, NIL);
        self.next.resize(cap, NIL);
        self.prev.resize(cap, NIL);
        self.hash.resize(cap, 0);
        self.phash.resize(cap, 0);
        self.nalias.resize(cap, 0);
        self.used.resize(cap / 64, 0);
        table.resize(cap * 2, 0);
        self.table = table;
        // Re-home every named slot in the bigger index. `hash[i]` is current
        // for named slots; the child lists hold slot numbers and are untouched.
        for i in 0..old {
            if self.e[i].named() { self.table_put(i); }
        }
        self.fsck("grow");
        true
    }

    // ── name index ──────────────────────────────────────────────────────────

    fn index_insert(&mut self, idx: usize) {
        self.hash[idx] = hash(self.e[idx].path());
        self.phash[idx] = hash(parent_of(self.e[idx].path()));
        self.table_put(idx);
        self.link_child(idx);
    }

    /// Put named slot `idx` (its `hash` already set) into the name index.
    /// The index is non-empty: a slot exists only after `grow_to`.
    fn table_put(&mut self, idx: usize) {
        let mask = self.table.len() - 1;
        let mut s = self.hash[idx] as usize & mask;
        while self.table[s] != 0 { s = (s + 1) & mask; }
        self.table[s] = idx as u16 + 1;
    }

    /// Link named slot `idx` into its parent directory's child list.
    fn link_child(&mut self, idx: usize) {
        let p = match self.find(parent_of(self.e[idx].path())) {
            Some(p) if p != idx && self.e[p].is_dir => p,
            _ => { self.parent[idx] = NIL; return; }
        };
        self.parent[idx] = p as u16;
        let head = self.first[p];
        self.prev[idx] = NIL;
        self.next[idx] = head;
        if head != NIL { self.prev[head as usize] = idx as u16; }
        self.first[p] = idx as u16;
    }

    /// Undo `link_child`.
    fn unlink_child(&mut self, idx: usize) {
        let p = self.parent[idx];
        if p == NIL { return; }
        let (pr, nx) = (self.prev[idx], self.next[idx]);
        if pr != NIL { self.next[pr as usize] = nx; } else { self.first[p as usize] = nx; }
        if nx != NIL { self.prev[nx as usize] = pr; }
        self.parent[idx] = NIL;
        self.prev[idx] = NIL;
        self.next[idx] = NIL;
    }

    fn index_remove(&mut self, idx: usize) {
        self.unlink_child(idx);
        self.phash[idx] = 0;
        let mask = self.table.len() - 1;
        let mut s = self.hash[idx] as usize & mask;
        loop {
            match self.table[s] {
                0 => return, // not indexed (cannot happen for a named slot)
                v if v as usize == idx + 1 => break,
                _ => s = (s + 1) & mask,
            }
        }
        // Backward-shift deletion: pull later members of the probe run into
        // the hole whenever the hole lies on their path from home.
        let mut hole = s;
        let mut j = s;
        loop {
            j = (j + 1) & mask;
            let v = self.table[j];
            if v == 0 { break; }
            let home = self.hash[v as usize - 1] as usize & mask;
            // Can `v` (home `home`, now at `j`) move back to `hole`? Only if
            // `home` is not cyclically in (hole, j].
            let dist_home = j.wrapping_sub(home) & mask;
            let dist_hole = j.wrapping_sub(hole) & mask;
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
        if self.table.is_empty() { return None; } // pool not grown yet
        let mask = self.table.len() - 1;
        let h = hash(path);
        let mut s = h as usize & mask;
        loop {
            let v = self.table[s];
            if v == 0 { return None; }
            let i = v as usize - 1;
            if self.hash[i] == h && self.e[i].path() == path { return Some(i); }
            s = (s + 1) & mask;
        }
    }

    // ── slot lifecycle ──────────────────────────────────────────────────────

    /// Lowest free slot within the current capacity, if any. Callers go
    /// through `crate::tmp_free_slot`, which grows the pool when this is None.
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
        e.unlinked = false;
        let n = path.len().min(MAX_TMP_PATH - 1);
        e.path = path[..n].to_vec();
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
        if to != 0 && to - 1 < self.e.len() {
            self.nalias[to - 1] = self.nalias[to - 1].saturating_sub(1);
        }
        self.used[idx / 64] &= !(1u64 << (idx % 64));
        self.count -= 1;
        self.e[idx] = TmpFileEntry::empty();
        self.xattr[idx] = None;
        // A directory is released only once empty (rmdir), so no child can
        // still point here; clear the head regardless.
        self.first[idx] = NIL;
        self.fsck("release");
    }

    /// Rename slot `idx` (named or not). Callers have length-checked.
    pub(crate) fn set_path(&mut self, idx: usize, path: &[u8]) {
        let named = self.e[idx].named();
        if named { self.index_remove(idx); }
        let n = path.len().min(MAX_TMP_PATH - 1);
        self.e[idx].path = path[..n].to_vec();
        if named { self.index_insert(idx); }
        self.fsck("set_path");
    }

    /// Does named directory slot `dir` have any named child?
    #[inline] pub(crate) fn has_children(&self, dir: usize) -> bool { self.first[dir] != NIL }

    /// Every named slot below directory slot `dir`, parents before their
    /// children (a rename must re-path a directory before what it holds).
    pub(crate) fn subtree(&self, dir: usize) -> alloc::vec::Vec<usize> {
        let mut out = alloc::vec::Vec::new();
        let mut c = self.first[dir];
        while c != NIL { out.push(c as usize); c = self.next[c as usize]; }
        let mut i = 0;
        while i < out.len() {
            let mut c = self.first[out[i]];
            while c != NIL { out.push(c as usize); c = self.next[c as usize]; }
            i += 1;
        }
        out
    }

    /// Drop a slot's name but keep the inode (see `tmp_drop_name`).
    pub(crate) fn make_nameless(&mut self, idx: usize) {
        if self.e[idx].named() { self.index_remove(idx); }
        self.e[idx].ephemeral = true;
        self.e[idx].unlinked = true;
        self.fsck("make_nameless");
    }

    /// Make in-use slot `idx` a hard-link alias of data-owning slot `owner`.
    pub(crate) fn set_link_to(&mut self, idx: usize, owner: usize) {
        let old = self.e[idx].link_to;
        if old != 0 && old - 1 < self.e.len() {
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
            while i < self.e.len() {
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

    /// Slot `idx`'s xattr arena; all zero (no attributes) when it has none.
    #[inline] pub(crate) fn xattr(&self, idx: usize) -> &XattrArena {
        self.xattr[idx].as_deref().unwrap_or(&EMPTY_XATTR)
    }
    /// Slot `idx`'s xattr arena, allocated (empty) on first use.
    #[inline] pub(crate) fn xattr_mut(&mut self, idx: usize) -> &mut XattrArena {
        self.xattr[idx].get_or_insert_with(|| alloc::boxed::Box::new([0u8; xattr::TMP_XATTR_ARENA]))
    }
    /// Entry `idx` and its xattr arena, both mutable (allocating the arena).
    #[inline] pub(crate) fn entry_xattr_mut(&mut self, idx: usize)
        -> (&mut TmpFileEntry, &mut XattrArena)
    {
        let a = self.xattr[idx].get_or_insert_with(|| alloc::boxed::Box::new([0u8; xattr::TMP_XATTR_ARENA]));
        (&mut self.e[idx], a)
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
        for i in 0..self.e.len() {
            let e = &self.e[i];
            let bit = self.used[i / 64] >> (i % 64) & 1 == 1;
            if bit != e.in_use { bad += 1; }
            if e.in_use { n += 1; }
            if e.named() {
                indexed += 1;
                if self.find(e.path()).is_none() { bad += 1; }
                if self.phash[i] != hash(parent_of(e.path())) { bad += 1; }
                // Child index: linked under the directory its path names.
                let want = match self.find(parent_of(e.path())) {
                    Some(p) if p != i && self.e[p].is_dir => p as u16,
                    _ => NIL,
                };
                if self.parent[i] != want { bad += 1; }
                if want != NIL {
                    let mut c = self.first[want as usize];
                    let mut seen = false;
                    let mut hops = 0;
                    while c != NIL && hops <= self.e.len() {
                        if c as usize == i { seen = true; break; }
                        c = self.next[c as usize];
                        hops += 1;
                    }
                    if !seen { bad += 1; }
                }
            } else if self.phash[i] != 0 || self.parent[i] != NIL { bad += 1; }
            if e.in_use && e.link_to != 0 {
                links += 1;
                if e.link_to - 1 >= self.e.len() || self.nalias[e.link_to - 1] == 0 { bad += 1; }
            }
            total_alias += self.nalias[i] as usize;
        }
        if links != total_alias { bad += 1; }
        if n != self.count { bad += 1; }
        if self.table.iter().filter(|&&v| v != 0).count() != indexed { bad += 1; }
        let cap = self.e.len();
        for l in [self.xattr.len(), self.parent.len(), self.first.len(), self.next.len(),
                  self.prev.len(), self.hash.len(), self.phash.len(), self.nalias.len()] {
            if l != cap { bad += 1; }
        }
        if self.used.len() * 64 != cap { bad += 1; }
        if self.table.len() != if cap == 0 { 0 } else { cap * 2 } { bad += 1; }
        bad
    }
}
