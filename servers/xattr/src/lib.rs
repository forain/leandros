//! Shared xattr + POSIX ACL contract for LeandrOS.
//!
//! Single source of truth used by the kernel (syscall layer), the VFS
//! (tmpfs), and the f2fs server: size caps, namespace indices, the packed
//! on-disk/in-memory entry codec, namespace permission gates, and the POSIX
//! 1003.1e ACL evaluator. Both filesystems store xattrs in the same arena
//! byte format, so a blob is interpretable by either side.
//!
//! Error convention: every fallible function returns `Err(errno)` with a
//! POSITIVE errno value from the `E*` constants below. IPC handlers reply
//! with the negated value (`-(e as i64)`).
//!
//! Arena wire format (Linux `f2fs_xattr_entry` shape, entries 4-byte
//! aligned, zero `e_name_index` terminates the list; an all-zero arena is
//! an empty list):
//!
//! ```text
//! u8  e_name_index      // namespace index, 0 == end of list
//! u8  e_name_len        // suffix length (name after the namespace prefix)
//! u16 e_value_size      // little-endian
//! u8  name[e_name_len]
//! u8  value[e_value_size]
//! pad to 4-byte boundary
//! ```

#![no_std]

// ── errnos (positive; negate when replying) ──────────────────────────────────
pub const EPERM: i32 = 1;
pub const E2BIG: i32 = 7;
pub const EACCES: i32 = 13;
pub const EEXIST: i32 = 17;
pub const EINVAL: i32 = 22;
pub const ENOSPC: i32 = 28;
pub const ERANGE: i32 = 34;
pub const ENODATA: i32 = 61;
pub const EOPNOTSUPP: i32 = 95;

// ── size caps (bytes) ────────────────────────────────────────────────────────
/// Longest full attribute name (prefix + suffix), Linux XATTR_NAME_MAX.
pub const XATTR_NAME_MAX: usize = 255;
/// Kernel-level single-value guard (Linux XATTR_SIZE_MAX): E2BIG above this.
/// Values below this can still fail with ENOSPC against a filesystem arena.
pub const XATTR_SIZE_MAX: usize = 65536;
/// Per-inode arena in a tmpfs entry.
pub const TMP_XATTR_ARENA: usize = 2048;
/// Per-inode arena in the dedicated f2fs xattr node block: 4096 - footer(20).
pub const F2FS_XATTR_ARENA: usize = 4076;

// ── setxattr flags ───────────────────────────────────────────────────────────
pub const XATTR_CREATE: u32 = 1; // fail EEXIST if the attribute exists
pub const XATTR_REPLACE: u32 = 2; // fail ENODATA if the attribute is absent

// ── namespace indices (Linux F2FS_XATTR_INDEX_*) ─────────────────────────────
pub const IDX_USER: u8 = 1;
pub const IDX_ACL_ACCESS: u8 = 2; // full name "system.posix_acl_access", empty suffix
pub const IDX_ACL_DEFAULT: u8 = 3; // full name "system.posix_acl_default", empty suffix
pub const IDX_TRUSTED: u8 = 4;
pub const IDX_SECURITY: u8 = 6;

pub const ACL_ACCESS_NAME: &[u8] = b"system.posix_acl_access";
pub const ACL_DEFAULT_NAME: &[u8] = b"system.posix_acl_default";

// ── file-mode helpers (S_IFMT lives in the low 16 bits everywhere here) ──────
const S_IFMT: u16 = 0o170000;
const S_IFDIR: u16 = 0o040000;
const S_IFREG: u16 = 0o100000;
const S_IFLNK: u16 = 0o120000;
const S_ISVTX: u16 = 0o001000;

#[inline]
pub fn is_dir(mode: u16) -> bool {
    mode & S_IFMT == S_IFDIR
}
#[inline]
fn is_reg(mode: u16) -> bool {
    mode & S_IFMT == S_IFREG
}
#[inline]
fn is_lnk(mode: u16) -> bool {
    mode & S_IFMT == S_IFLNK
}

/// Owner/permission facts about the target inode, supplied by the filesystem.
#[derive(Clone, Copy)]
pub struct FileMeta {
    pub mode: u16, // includes S_IFMT type bits
    pub uid: u32,
    pub gid: u32,
}

/// Most supplementary groups a task may hold (Linux allows 65536; nothing in
/// this image needs more than a handful, and the array lives inline in the
/// task struct).
pub const NGROUPS_MAX: usize = 32;

// Linux capability numbers the permission gates consult.
pub const CAP_CHOWN: u32 = 0;
pub const CAP_DAC_OVERRIDE: u32 = 1;
pub const CAP_DAC_READ_SEARCH: u32 = 2;
pub const CAP_FOWNER: u32 = 3;
pub const CAP_FSETID: u32 = 4;

/// Extents per id map (must equal `sched::ns::MAP_EXT`; `vfs::cred_of`
/// copies the arrays, so a mismatch is a compile error there).
pub const MAP_EXT: usize = 5;

/// Snapshot of the caller's user-namespace id maps (dependency-free copy of
/// `sched::ns::IdView`): `ext[kind]` holds `(upper_first, lower_kuid, count)`
/// extents, kind 0 = uid, 1 = gid. `ns == 0` is the identity (host) view.
#[derive(Clone, Copy)]
pub struct IdView {
    pub ns: u16,
    pub n: [u8; 2],
    pub ext: [[(u32, u32, u32); MAP_EXT]; 2],
}

impl IdView {
    pub const HOST: IdView = IdView { ns: 0, n: [0; 2], ext: [[(0, 0, 0); MAP_EXT]; 2] };
    /// Is kernel id `kid` of `kind` (0 = uid, 1 = gid) mapped in this namespace?
    #[inline]
    pub fn maps_k(&self, kind: usize, kid: u32) -> bool {
        if self.ns == 0 { return true; }
        if kid == u32::MAX { return false; }
        let n = (self.n[kind] as usize).min(MAP_EXT);
        self.ext[kind][..n].iter().any(|&(_, l, c)| kid >= l && kid - l < c)
    }
}

/// The caller's identity as every permission gate sees it: effective uid,
/// effective gid, and the supplementary group list. Group membership is
/// `egid` OR any supplementary group — there is exactly one place that
/// answers "is the caller in group g", [`Cred::in_group`], so an evaluator
/// can never consult the egid alone by accident.
#[derive(Clone, Copy)]
pub struct Cred {
    pub euid: u32,
    pub egid: u32,
    pub ngroups: u8,
    pub groups: [u32; NGROUPS_MAX],
    /// fsuid == 0 in the initial user namespace: exactly the old `euid == 0`.
    /// The ONLY thing the host-only gates (trusted.*, security.*, mount roots,
    /// devices) accept.
    pub host_root: bool,
    /// Effective capabilities; meaningful only when `view.ns != 0`.
    pub caps: u64,
    /// The caller's user-namespace id maps (kuid <-> ns id).
    pub view: IdView,
}

/// chmod(2) by a caller that is neither root nor in the file's group drops
/// S_ISGID from the new mode (Linux `setattr_prepare`: without CAP_FSETID,
/// `!in_group_p(i_gid)` clears it). The owner check is done by the caller;
/// this only filters the mode.
pub fn chmod_filter_mode(cred: &Cred, file_gid: u32, mode: u32) -> u32 {
    const S_ISGID: u32 = 0o2000;
    if cred.euid != 0 && !cred.in_group(file_gid) { mode & !S_ISGID } else { mode }
}

/// `chmod_filter_mode` with CAP_FSETID over the inode (`setattr_prepare`):
/// the capability must be held in the caller's user namespace AND the
/// inode's owner/group must be mapped there.
pub fn chmod_filter_mode_c(cred: &Cred, meta: &FileMeta, mode: u32) -> u32 {
    const S_ISGID: u32 = 0o2000;
    if !cred.in_group(meta.gid) && !cred.cap_inode(CAP_FSETID, meta.uid, meta.gid) { mode & !S_ISGID } else { mode }
}

impl Cred {
    pub const fn new(euid: u32, egid: u32) -> Self {
        Cred { euid, egid, ngroups: 0, groups: [0; NGROUPS_MAX],
               host_root: euid == 0, caps: 0, view: IdView::HOST }
    }
    pub const ROOT: Cred = Cred::new(0, 0);
    /// Attach the caller's namespace view and effective caps. A fsuid-0 task
    /// outside the initial user namespace is NOT host root.
    pub fn with_ns(mut self, view: IdView, caps: u64) -> Self {
        self.host_root = self.euid == 0 && view.ns == 0;
        self.caps = caps;
        self.view = view;
        self
    }
    /// Linux `capable_wrt_inode_uidgid`: `cap` held in the caller's user
    /// namespace AND the inode's uid and gid mapped in it. Host root passes
    /// (identical to the old `euid == 0`). Never "euid 0 in any namespace".
    #[inline]
    pub fn cap_inode(&self, cap: u32, uid: u32, gid: u32) -> bool {
        self.host_root
            || (self.view.ns != 0 && cap < 64 && self.caps >> cap & 1 != 0
                && self.view.maps_k(0, uid) && self.view.maps_k(1, gid))
    }
    /// Namespace-scoped capability not tied to an inode.
    #[inline]
    pub fn cap_any(&self, cap: u32) -> bool {
        self.host_root || (self.view.ns != 0 && cap < 64 && self.caps >> cap & 1 != 0)
    }
    /// Linux `fsuidgid_has_mapping`: false when the caller's fsuid or fsgid
    /// is unmapped in its own namespace (creating a file would be EOVERFLOW).
    #[inline]
    pub fn owner_mapped(&self) -> bool {
        self.view.maps_k(0, self.euid) && self.view.maps_k(1, self.egid)
    }
    #[inline]
    pub fn in_group(&self, gid: u32) -> bool {
        if self.egid == gid { return true; }
        let n = (self.ngroups as usize).min(NGROUPS_MAX);
        self.groups[..n].iter().any(|&g| g == gid)
    }
    #[inline]
    pub fn groups(&self) -> &[u32] {
        &self.groups[..(self.ngroups as usize).min(NGROUPS_MAX)]
    }
}

// ── name <-> (namespace index, suffix) ───────────────────────────────────────

/// Split a full attribute name into (namespace index, suffix).
/// `None` means the namespace is unsupported → EOPNOTSUPP.
/// The caller must separately enforce `full.len() <= XATTR_NAME_MAX` (ERANGE).
pub fn split_name(full: &[u8]) -> Option<(u8, &[u8])> {
    if let Some(suf) = full.strip_prefix(b"user.") {
        Some((IDX_USER, suf))
    } else if full == ACL_ACCESS_NAME {
        Some((IDX_ACL_ACCESS, b""))
    } else if full == ACL_DEFAULT_NAME {
        Some((IDX_ACL_DEFAULT, b""))
    } else if let Some(suf) = full.strip_prefix(b"trusted.") {
        Some((IDX_TRUSTED, suf))
    } else if let Some(suf) = full.strip_prefix(b"security.") {
        Some((IDX_SECURITY, suf))
    } else {
        None
    }
}

/// Rebuild the full name from (index, suffix) into `out`; returns the length.
/// `None` only on unknown index or overflow (both are internal errors).
pub fn join_name(idx: u8, suf: &[u8], out: &mut [u8]) -> Option<usize> {
    let prefix: &[u8] = match idx {
        IDX_USER => b"user.",
        IDX_ACL_ACCESS => ACL_ACCESS_NAME,
        IDX_ACL_DEFAULT => ACL_DEFAULT_NAME,
        IDX_TRUSTED => b"trusted.",
        IDX_SECURITY => b"security.",
        _ => return None,
    };
    let total = prefix.len() + suf.len();
    if total > out.len() {
        return None;
    }
    out[..prefix.len()].copy_from_slice(prefix);
    out[prefix.len()..total].copy_from_slice(suf);
    Some(total)
}

// ── arena codec ──────────────────────────────────────────────────────────────

#[inline]
fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// O(1) empty test — the `ls -l` fast path.
#[inline]
pub fn is_empty(arena: &[u8]) -> bool {
    arena.is_empty() || arena[0] == 0
}

/// Iterate entries as (offset, idx, suffix, value). Stops at the terminator
/// or at the first malformed header (treated as end of list, defensively).
struct Iter<'a> {
    arena: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for Iter<'a> {
    type Item = (usize, u8, &'a [u8], &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        let a = self.arena;
        if self.pos + 4 > a.len() || a[self.pos] == 0 {
            return None;
        }
        let idx = a[self.pos];
        let nl = a[self.pos + 1] as usize;
        let vs = u16::from_le_bytes([a[self.pos + 2], a[self.pos + 3]]) as usize;
        let body = self.pos + 4;
        if body + nl + vs > a.len() {
            return None;
        }
        let entry = (
            self.pos,
            idx,
            &a[body..body + nl],
            &a[body + nl..body + nl + vs],
        );
        self.pos = self.pos + align4(4 + nl + vs);
        Some(entry)
    }
}

fn entries(arena: &[u8]) -> Iter<'_> {
    Iter { arena, pos: 0 }
}

/// Byte length used by the entry list (excluding the terminator).
fn used_len(arena: &[u8]) -> usize {
    let mut it = entries(arena);
    while it.next().is_some() {}
    it.pos
}

/// Look up one attribute's value.
pub fn find<'a>(arena: &'a [u8], idx: u8, suf: &[u8]) -> Option<&'a [u8]> {
    entries(arena).find(|&(_, i, n, _)| i == idx && n == suf).map(|(_, _, _, v)| v)
}

/// Emit the NUL-terminated full-name list.
/// `include_trusted=false` hides `trusted.*` names (non-root callers).
/// Empty `out` = size query. Returns the total byte count needed/written.
pub fn list(arena: &[u8], out: &mut [u8], include_trusted: bool) -> Result<usize, i32> {
    let mut namebuf = [0u8; XATTR_NAME_MAX];
    let mut total = 0usize;
    // First pass: size.
    for (_, idx, suf, _) in entries(arena) {
        if idx == IDX_TRUSTED && !include_trusted {
            continue;
        }
        match join_name(idx, suf, &mut namebuf) {
            Some(n) => total += n + 1,
            None => continue,
        }
    }
    if out.is_empty() {
        return Ok(total);
    }
    if total > out.len() {
        return Err(ERANGE);
    }
    let mut w = 0usize;
    for (_, idx, suf, _) in entries(arena) {
        if idx == IDX_TRUSTED && !include_trusted {
            continue;
        }
        if let Some(n) = join_name(idx, suf, &mut namebuf) {
            out[w..w + n].copy_from_slice(&namebuf[..n]);
            out[w + n] = 0;
            w += n + 1;
        }
    }
    Ok(total)
}

/// Insert or replace an attribute. Honors XATTR_CREATE/XATTR_REPLACE.
/// Returns the new used length. The arena is rewritten compactly and the
/// tail is zeroed (which also rewrites the terminator).
pub fn set(arena: &mut [u8], idx: u8, suf: &[u8], val: &[u8], flags: u32) -> Result<usize, i32> {
    if suf.len() > u8::MAX as usize || val.len() > u16::MAX as usize {
        return Err(EINVAL);
    }
    let existing = entries(arena).find(|&(_, i, n, _)| i == idx && n == suf);
    if existing.is_some() && flags & XATTR_CREATE != 0 {
        return Err(EEXIST);
    }
    if existing.is_none() && flags & XATTR_REPLACE != 0 {
        return Err(ENODATA);
    }
    let old_entry_len = existing
        .map(|(_, _, n, v)| align4(4 + n.len() + v.len()))
        .unwrap_or(0);
    let old_off = existing.map(|(o, _, _, _)| o);
    let new_entry_len = align4(4 + suf.len() + val.len());
    let used = used_len(arena);
    // Reserve 4 bytes so a full terminator header always fits after the list.
    if used - old_entry_len + new_entry_len + 4 > arena.len() {
        return Err(ENOSPC);
    }
    // Drop the old entry by sliding the tail down.
    let mut end = used;
    if let Some(off) = old_off {
        arena.copy_within(off + old_entry_len..used, off);
        end = used - old_entry_len;
    }
    // Append the new entry.
    arena[end] = idx;
    arena[end + 1] = suf.len() as u8;
    arena[end + 2..end + 4].copy_from_slice(&(val.len() as u16).to_le_bytes());
    arena[end + 4..end + 4 + suf.len()].copy_from_slice(suf);
    arena[end + 4 + suf.len()..end + 4 + suf.len() + val.len()].copy_from_slice(val);
    let new_end = end + new_entry_len;
    // Zero alignment padding and the rest of the arena (terminator included).
    arena[end + 4 + suf.len() + val.len()..new_end].fill(0);
    arena[new_end..].fill(0);
    Ok(new_end)
}

/// Remove an attribute. Returns the new used length, or ENODATA if absent.
pub fn remove(arena: &mut [u8], idx: u8, suf: &[u8]) -> Result<usize, i32> {
    let (off, entry_len) = match entries(arena).find(|&(_, i, n, _)| i == idx && n == suf) {
        Some((o, _, n, v)) => (o, align4(4 + n.len() + v.len())),
        None => return Err(ENODATA),
    };
    let used = used_len(arena);
    arena.copy_within(off + entry_len..used, off);
    arena[used - entry_len..].fill(0);
    Ok(used - entry_len)
}

// ── namespace permission gates ───────────────────────────────────────────────
//
// Mirrors Linux fs/xattr.c xattr_permission():
//   user.*   only on regular files/dirs (write → EPERM, read → ENODATA
//            elsewhere); sticky dirs need ownership for writes; then normal
//            read/write file permission (ACL-aware) applies.
//   system.posix_acl_*  reads are unrestricted; writes need owner-or-root;
//            default ACLs only on directories; never on symlinks.
//   security.* readable by all, writable by root (CAP_SYS_ADMIN/CAP_SETFCAP).
//   trusted.* root only; hidden (ENODATA / filtered) from others.

/// Gate for getxattr. `acl` = the stored access ACL, if any.
pub fn may_read_xattr(idx: u8, meta: &FileMeta, cred: &Cred, acl: Option<&[u8]>) -> Result<(), i32> {
    match idx {
        IDX_USER => {
            if !is_reg(meta.mode) && !is_dir(meta.mode) {
                return Err(ENODATA);
            }
            if access_check(meta, cred, acl, true, false, false) {
                Ok(())
            } else {
                Err(EACCES)
            }
        }
        IDX_ACL_ACCESS | IDX_ACL_DEFAULT => Ok(()),
        IDX_SECURITY => Ok(()), // readable by all, on any inode type
        IDX_TRUSTED => {
            if cred.host_root { // Linux: CAP_SYS_ADMIN in the init ns
                Ok(())
            } else {
                Err(ENODATA)
            }
        }
        _ => Err(EOPNOTSUPP),
    }
}

/// Gate for setxattr/removexattr.
pub fn may_write_xattr(idx: u8, meta: &FileMeta, cred: &Cred, acl: Option<&[u8]>) -> Result<(), i32> {
    if idx == IDX_SECURITY {
        // security.* (incl. security.capability, CAP_SETFCAP): root only,
        // allowed on every inode type (labels on symlinks are legal).
        // host only: security.capability needs SETFCAP over the inode, which
        // is EPERM outside the initial namespace (design idmap 2.8).
        return if cred.host_root { Ok(()) } else { Err(EPERM) };
    }
    if is_lnk(meta.mode) {
        return Err(EPERM); // no user.* and no ACLs on symlinks
    }
    match idx {
        IDX_USER => {
            if !is_reg(meta.mode) && !is_dir(meta.mode) {
                return Err(EPERM);
            }
            if is_dir(meta.mode) && meta.mode & S_ISVTX != 0 && cred.euid != meta.uid && !cred.cap_inode(CAP_FOWNER, meta.uid, meta.gid) {
                return Err(EPERM);
            }
            if access_check(meta, cred, acl, false, true, false) {
                Ok(())
            } else {
                Err(EACCES)
            }
        }
        IDX_ACL_ACCESS | IDX_ACL_DEFAULT => {
            if cred.euid != meta.uid && !cred.cap_inode(CAP_FOWNER, meta.uid, meta.gid) {
                return Err(EPERM);
            }
            if idx == IDX_ACL_DEFAULT && !is_dir(meta.mode) {
                return Err(EACCES);
            }
            Ok(())
        }
        IDX_TRUSTED => {
            if cred.host_root {
                Ok(())
            } else {
                Err(EPERM)
            }
        }
        _ => Err(EOPNOTSUPP),
    }
}

// ── POSIX ACL wire format ────────────────────────────────────────────────────
//
// system.posix_acl_access / system.posix_acl_default value bytes
// (little-endian):
//   u32 a_version == 2
//   n × { u16 e_tag, u16 e_perm, u32 e_id }
// Tags:
const ACL_USER_OBJ: u16 = 0x01;
const ACL_USER: u16 = 0x02;
const ACL_GROUP_OBJ: u16 = 0x04;
const ACL_GROUP: u16 = 0x08;
const ACL_MASK: u16 = 0x10;
const ACL_OTHER: u16 = 0x20;

const ACL_HDR: usize = 4;
const ACL_ENT: usize = 8;

#[derive(Clone, Copy)]
struct AclEntry {
    tag: u16,
    perm: u16, // masked to rwx (low 3 bits)
    id: u32,
}

fn acl_entry(value: &[u8], i: usize) -> AclEntry {
    let o = ACL_HDR + i * ACL_ENT;
    AclEntry {
        tag: u16::from_le_bytes([value[o], value[o + 1]]),
        perm: u16::from_le_bytes([value[o + 2], value[o + 3]]) & 7,
        id: u32::from_le_bytes([value[o + 4], value[o + 5], value[o + 6], value[o + 7]]),
    }
}

fn acl_entry_count(value: &[u8]) -> usize {
    (value.len() - ACL_HDR) / ACL_ENT
}

/// Validation summary of a well-formed ACL.
#[derive(Clone, Copy)]
pub struct AclSummary {
    pub user_obj: u16,
    pub group_obj: u16,
    pub other: u16,
    pub mask: Option<u16>,
    pub has_named: bool,
}

/// Validate ACL wire bytes (POSIX canonical order). EINVAL on any violation.
pub fn acl_validate(value: &[u8]) -> Result<AclSummary, i32> {
    if value.len() < ACL_HDR
        || (value.len() - ACL_HDR) % ACL_ENT != 0
        || u32::from_le_bytes([value[0], value[1], value[2], value[3]]) != 2
    {
        return Err(EINVAL);
    }
    // Canonical order state machine: USER_OBJ < USER* < GROUP_OBJ < GROUP* <
    // MASK? < OTHER, with strictly increasing ids inside USER*/GROUP* runs.
    let mut state = 0u8; // 0 start, 1 after USER_OBJ, 2 after GROUP_OBJ, 3 after MASK, 4 after OTHER
    let mut summary = AclSummary { user_obj: 0, group_obj: 0, other: 0, mask: None, has_named: false };
    let mut last_id: Option<u32> = None;
    for i in 0..acl_entry_count(value) {
        let e = acl_entry(value, i);
        match e.tag {
            ACL_USER_OBJ if state == 0 => {
                summary.user_obj = e.perm;
                state = 1;
            }
            ACL_USER if state == 1 => {
                if let Some(prev) = last_id {
                    if e.id <= prev {
                        return Err(EINVAL);
                    }
                }
                last_id = Some(e.id);
                summary.has_named = true;
            }
            ACL_GROUP_OBJ if state == 1 => {
                summary.group_obj = e.perm;
                last_id = None;
                state = 2;
            }
            ACL_GROUP if state == 2 => {
                if let Some(prev) = last_id {
                    if e.id <= prev {
                        return Err(EINVAL);
                    }
                }
                last_id = Some(e.id);
                summary.has_named = true;
            }
            ACL_MASK if state == 2 => {
                summary.mask = Some(e.perm);
                state = 3;
            }
            ACL_OTHER if state == 2 || state == 3 => {
                summary.other = e.perm;
                state = 4;
            }
            _ => return Err(EINVAL),
        }
    }
    if state != 4 {
        return Err(EINVAL); // missing USER_OBJ/GROUP_OBJ/OTHER or trailing junk
    }
    if summary.has_named && summary.mask.is_none() {
        return Err(EINVAL); // named entries require a mask
    }
    Ok(summary)
}

/// Trivial = representable purely as mode bits (no named entries, no mask).
#[inline]
pub fn acl_is_trivial(s: &AclSummary) -> bool {
    !s.has_named && s.mask.is_none()
}

/// The 9 permission bits the inode mode must carry for this ACL:
/// owner = USER_OBJ, group = MASK if present else GROUP_OBJ, other = OTHER.
#[inline]
pub fn acl_mode_bits(s: &AclSummary) -> u16 {
    (s.user_obj << 6) | (s.mask.unwrap_or(s.group_obj) << 3) | s.other
}

/// posix_acl_chmod: rewrite a STORED access ACL in place from new mode bits.
/// USER_OBJ ← owner bits, OTHER ← other bits, MASK (if present, else
/// GROUP_OBJ) ← group bits. Named entries untouched.
pub fn acl_chmod_rewrite(value: &mut [u8], mode: u16) {
    let n = acl_entry_count(value);
    let has_mask = (0..n).any(|i| acl_entry(value, i).tag == ACL_MASK);
    for i in 0..n {
        let o = ACL_HDR + i * ACL_ENT;
        let tag = u16::from_le_bytes([value[o], value[o + 1]]);
        let new_perm = match tag {
            ACL_USER_OBJ => (mode >> 6) & 7,
            ACL_OTHER => mode & 7,
            ACL_MASK => (mode >> 3) & 7,
            ACL_GROUP_OBJ if !has_mask => (mode >> 3) & 7,
            _ => continue,
        };
        value[o + 2..o + 4].copy_from_slice(&new_perm.to_le_bytes());
    }
}

// ── access evaluation ────────────────────────────────────────────────────────

/// Permission-mask bits for `may_access`, Linux `MAY_*` values.
pub const MAY_EXEC: u8 = 1;
pub const MAY_WRITE: u8 = 2;
pub const MAY_READ: u8 = 4;

/// THE permission gate for every filesystem operation that consults an inode's
/// mode — path traversal (`MAY_EXEC` on each directory component), creating or
/// removing an entry (`MAY_WRITE | MAY_EXEC` on the parent), opening
/// (`MAY_READ`/`MAY_WRITE`), and AF_UNIX `connect` (`MAY_WRITE` on the socket
/// node). It is `access_check` with a Linux-style mask, so the two filesystems
/// and the VFS share one evaluator (mode bits, stored POSIX ACL, root bypass).
///
/// Root (euid 0) passes every check except execute on a non-directory with no
/// x bit at all — CAP_DAC_OVERRIDE semantics. The stored access ACL, if any,
/// is honoured exactly as it is for open/faccessat.
#[inline]
pub fn may_access(meta: &FileMeta, cred: &Cred, acl: Option<&[u8]>, mask: u8) -> bool {
    access_check(meta, cred, acl,
                 mask & MAY_READ != 0, mask & MAY_WRITE != 0, mask & MAY_EXEC != 0)
}

/// The sticky-directory rule (Linux `check_sticky`): unlinking, renaming over
/// or renaming away an entry inside a directory with S_ISVTX set is allowed
/// only to the entry's owner, the directory's owner, or root. `dir` is the
/// parent's meta, `victim_uid` the owner of the entry being removed/replaced.
/// Returns true when the operation must be refused with EPERM.
#[inline]
pub fn sticky_denies(dir: &FileMeta, victim_uid: u32, euid: u32) -> bool {
    dir.mode & S_ISVTX != 0 && euid != 0 && euid != victim_uid && euid != dir.uid
}

/// `sticky_denies` for a namespace-aware caller (Linux `check_sticky`): the
/// owner of the entry (`victim`), the owner of the directory, or CAP_FOWNER
/// over the victim inode in the caller's user namespace.
#[inline]
pub fn sticky_denies_c(dir: &FileMeta, victim: &FileMeta, cred: &Cred) -> bool {
    dir.mode & S_ISVTX != 0 && cred.euid != victim.uid && cred.euid != dir.uid
        && !cred.cap_inode(CAP_FOWNER, victim.uid, victim.gid)
}

/// The set-ID bits a write, truncate or O_TRUNC open by `fsuid` must strip
/// from a file of mode `mode` (Linux `file_remove_privs` /
/// `should_remove_suid`): S_ISUID always, S_ISGID only with group execute
/// (without it S_ISGID marks mandatory locking, not set-group-ID). Regular
/// files only; nothing for root (CAP_FSETID ≙ fsuid 0). Whoever writes —
/// the owner included — loses the bits: the program being run is no longer
/// the one that was granted them. 0 when nothing must change.
#[inline]
pub fn setid_kill_bits(mode: u16, fsuid: u32) -> u16 {
    const S_ISUID: u16 = 0o4000;
    const S_ISGID: u16 = 0o2000;
    const S_IXGRP: u16 = 0o0010;
    if fsuid == 0 || mode & S_IFMT != S_IFREG { return 0; }
    let mut kill = mode & S_ISUID;
    if mode & (S_ISGID | S_IXGRP) == (S_ISGID | S_IXGRP) { kill |= S_ISGID; }
    kill
}

/// `setid_kill_bits` with CAP_FSETID over the inode in the caller's user
/// namespace (a mapped owner is required; host root passes as before).
#[inline]
pub fn setid_kill_bits_c(meta: &FileMeta, cred: &Cred) -> u16 {
    if cred.cap_inode(CAP_FSETID, meta.uid, meta.gid) { return 0; }
    setid_kill_bits(meta.mode, u32::MAX)
}

/// fs.protected_hardlinks (Linux `may_linkat`, on by default on every major
/// distribution): a caller that does not own the source may hard-link it only
/// if it is a regular file, not set-uid, not set-gid-and-group-executable,
/// and the caller could open it for reading AND writing (`may_rw`). Root and
/// the owner are exempt. Returns true when link(2) must fail with EPERM.
pub fn hardlink_denied(src: &FileMeta, euid: u32, may_rw: bool) -> bool {
    const S_ISUID: u16 = 0o4000;
    const S_ISGID: u16 = 0o2000;
    const S_IXGRP: u16 = 0o0010;
    if euid == 0 || euid == src.uid { return false; }
    if src.mode & S_IFMT != S_IFREG { return true; }
    if src.mode & S_ISUID != 0 { return true; }
    if src.mode & (S_ISGID | S_IXGRP) == (S_ISGID | S_IXGRP) { return true; }
    !may_rw
}

/// `hardlink_denied` with CAP_FOWNER over the source inode (Linux `may_linkat`
/// exempts the owner and `capable_wrt_inode_uidgid(CAP_FOWNER)`).
pub fn hardlink_denied_c(src: &FileMeta, cred: &Cred, may_rw: bool) -> bool {
    if cred.euid == src.uid || cred.cap_inode(CAP_FOWNER, src.uid, src.gid) { return false; }
    hardlink_denied(src, u32::MAX, may_rw)
}

/// Unified permission check for open/faccessat: POSIX 1003.1e ACL walk when
/// a (non-trivial, stored) access ACL is present, classic mode bits
/// otherwise. Root (euid 0) bypasses R/W always; X needs at least one x bit
/// unless the target is a directory.
///
/// Group matching is `cred.in_group(gid)`: the effective gid or any
/// supplementary group.
pub fn access_check(
    meta: &FileMeta,
    cred: &Cred,
    acl: Option<&[u8]>,
    want_r: bool,
    want_w: bool,
    want_x: bool,
) -> bool {
    let want: u16 = (want_r as u16) << 2 | (want_w as u16) << 1 | want_x as u16;
    let euid = cred.euid;
    if cred.host_root {
        if want_x && !is_dir(meta.mode) {
            return meta.mode & 0o111 != 0;
        }
        return true;
    }
    if cred.view.ns != 0 {
        // Linux generic_permission, capabilities taken in the caller's user
        // namespace over an inode whose uid AND gid are mapped there.
        let ovr = cred.cap_inode(CAP_DAC_OVERRIDE, meta.uid, meta.gid);
        let rs = |c: &Cred| c.cap_inode(CAP_DAC_READ_SEARCH, meta.uid, meta.gid);
        if is_dir(meta.mode) {
            if ovr || (!want_w && rs(cred)) { return true; }
        } else {
            if ovr && (!want_x || meta.mode & 0o111 != 0) { return true; }
            if want_r && !want_w && !want_x && rs(cred) { return true; }
        }
    }
    if let Some(bytes) = acl {
        if acl_validate(bytes).is_ok() {
            return acl_walk(bytes, meta, cred, want);
        }
        // Corrupt stored ACL: fall through to mode bits (fail-open to the
        // mode, which the invariant keeps at least as strict as the mask).
    }
    let bits = if euid == meta.uid {
        (meta.mode >> 6) & 7
    } else if cred.in_group(meta.gid) {
        (meta.mode >> 3) & 7
    } else {
        meta.mode & 7
    };
    bits & want == want
}

fn acl_walk(value: &[u8], meta: &FileMeta, cred: &Cred, want: u16) -> bool {
    let euid = cred.euid;
    let n = acl_entry_count(value);
    let mask = (0..n)
        .map(|i| acl_entry(value, i))
        .find(|e| e.tag == ACL_MASK)
        .map(|e| e.perm);
    let masked = |perm: u16| perm & mask.unwrap_or(7);
    let mut group_found = false;
    for i in 0..n {
        let e = acl_entry(value, i);
        match e.tag {
            // Owner matches un-masked, and wins immediately.
            ACL_USER_OBJ => {
                if euid == meta.uid {
                    return e.perm & want == want;
                }
            }
            ACL_USER => {
                if euid == e.id {
                    return masked(e.perm) & want == want;
                }
            }
            ACL_GROUP_OBJ => {
                if cred.in_group(meta.gid) {
                    group_found = true;
                    if masked(e.perm) & want == want {
                        return true;
                    }
                }
            }
            ACL_GROUP => {
                if cred.in_group(e.id) {
                    group_found = true;
                    if masked(e.perm) & want == want {
                        return true;
                    }
                }
            }
            ACL_MASK => {}
            ACL_OTHER => {
                // A group member that no group entry satisfied is denied;
                // it never falls through to "other".
                if group_found {
                    return false;
                }
                return e.perm & want == want;
            }
            _ => {}
        }
    }
    false
}

// ── creation: default-ACL inheritance ────────────────────────────────────────
//
// Linux `posix_acl_create`. When the parent directory carries a default ACL,
// a new entry inherits it: every child gets the default ACL — with its
// USER_OBJ / (MASK or GROUP_OBJ) / OTHER permissions intersected with the
// requested creation mode — as its *access* ACL, and a child directory also
// gets it verbatim as its own *default* ACL. The umask is NOT applied in that
// case (the default ACL replaces it, POSIX 1003.1e §23.4.2). With no default
// ACL on the parent the mode is simply `mode & !umask`, as ever.
//
// The requested mode and the caller's umask travel in one u64 on the wire:
// `mode | umask << MODE_UMASK_SHIFT` (see `pack_create_mode`), so both
// filesystems decide the child's mode through the same function and the
// kernel does not have to know whether the parent has a default ACL.

/// Bit position of the caller's umask inside a creation-mode argument.
pub const MODE_UMASK_SHIFT: u32 = 16;

#[inline]
pub fn pack_create_mode(mode: u32, umask: u32) -> u64 {
    ((mode & 0o7777) as u64) | (((umask & 0o777) as u64) << MODE_UMASK_SHIFT)
}

/// `(mode, umask)` from a packed creation-mode argument.
#[inline]
pub fn unpack_create_mode(arg: u64) -> (u16, u16) {
    ((arg & 0o7777) as u16, ((arg >> MODE_UMASK_SHIFT) & 0o777) as u16)
}

/// Longest ACL value the inheritance path handles (32 entries).
pub const ACL_INHERIT_MAX: usize = ACL_HDR + 32 * ACL_ENT;

/// What a new inode gets at creation.
pub struct Created {
    /// The 12 permission/setid bits to store in the inode mode.
    pub mode: u16,
    /// A non-trivial access ACL to store, if the default ACL yielded one.
    pub access_len: usize,
    pub access: [u8; ACL_INHERIT_MAX],
    /// True when the child is a directory and must also store the parent's
    /// default ACL as its own default ACL (verbatim).
    pub inherit_default: bool,
}

/// Compute the creation mode (and inherited access ACL) for a child of a
/// directory whose default ACL is `default_acl` (None/invalid = none).
/// `mode` is the caller's requested permission bits, `umask` the caller's
/// umask, `is_dir` whether the child is a directory.
pub fn acl_create(default_acl: Option<&[u8]>, mode: u16, umask: u16, is_dir: bool) -> Created {
    let mut out = Created {
        mode: (mode & 0o7777) & !(umask & 0o777),
        access_len: 0,
        access: [0u8; ACL_INHERIT_MAX],
        inherit_default: false,
    };
    let dacl = match default_acl {
        Some(v) if v.len() <= ACL_INHERIT_MAX && acl_validate(v).is_ok() => v,
        _ => return out,
    };
    // posix_acl_create_masq over a copy of the default ACL.
    let mut clone = [0u8; ACL_INHERIT_MAX];
    clone[..dacl.len()].copy_from_slice(dacl);
    let n = acl_entry_count(dacl);
    let mut user_perm = (mode >> 6) & 7;
    let mut group_perm = (mode >> 3) & 7;
    let mut other_perm = mode & 7;
    let mut not_equiv = false;
    let mut mask_idx: Option<usize> = None;
    let mut group_obj_idx: Option<usize> = None;
    for i in 0..n {
        let o = ACL_HDR + i * ACL_ENT;
        let e = acl_entry(dacl, i);
        match e.tag {
            ACL_USER_OBJ => {
                let p = e.perm & user_perm;
                clone[o + 2..o + 4].copy_from_slice(&p.to_le_bytes());
                user_perm = p;
            }
            ACL_OTHER => {
                let p = e.perm & other_perm;
                clone[o + 2..o + 4].copy_from_slice(&p.to_le_bytes());
                other_perm = p;
            }
            ACL_MASK => mask_idx = Some(i),
            ACL_GROUP_OBJ => group_obj_idx = Some(i),
            ACL_USER | ACL_GROUP => not_equiv = true,
            _ => {}
        }
    }
    let gi = match mask_idx.or(group_obj_idx) {
        Some(i) => i,
        None => return out, // cannot happen for a validated ACL
    };
    {
        let o = ACL_HDR + gi * ACL_ENT;
        let e = acl_entry(dacl, gi);
        let p = e.perm & group_perm;
        clone[o + 2..o + 4].copy_from_slice(&p.to_le_bytes());
        group_perm = p;
    }
    if mask_idx.is_some() { not_equiv = true; }
    out.mode = (mode & 0o7000) | (user_perm << 6) | (group_perm << 3) | other_perm;
    if not_equiv {
        out.access_len = dacl.len();
        out.access[..dacl.len()].copy_from_slice(&clone[..dacl.len()]);
    }
    out.inherit_default = is_dir;
    out
}

// ── Linux-format (lx) f2fs xattr codec ───────────────────────────────────────
//
// On Linux-made and lx volumes the xattr store is `inline area ++ xnode[0..4072]`
// holding a 24-byte header (magic 0xF2F52011, refcount, 16 reserved bytes) then
// the same entry shape as the arena above, ended by 4 zero bytes. ACLs (name
// index 2/3) are stored in the f2fs ACL format (version 1; USER_OBJ, GROUP_OBJ,
// MASK, OTHER carry no id) while userspace sees posix_acl_xattr (version 2, all
// entries 8 bytes). `lx_decode` turns the raw store into an ordinary arena
// (with posix-format ACLs) so every gate/evaluator above keeps working;
// `lx_encode` is the inverse.

pub const LX_MAGIC: u32 = 0xF2F5_2011;
pub const LX_HDR: usize = 24;
/// Bytes of an xattr node block usable for entries (4096 - 24 B footer).
pub const LX_XNODE: usize = 4072;

const ACL_F2FS_VERSION: u32 = 1;
const ACL_POSIX_VERSION: u32 = 2;

fn acl_short_tag(tag: u16) -> bool {
    matches!(tag, 0x01 | 0x04 | 0x10 | 0x20)
}

/// f2fs ACL value -> posix_acl_xattr value. Returns the new length.
fn acl_f2fs_to_posix(v: &[u8], out: &mut [u8]) -> Option<usize> {
    if v.len() < 4 || u32::from_le_bytes([v[0], v[1], v[2], v[3]]) != ACL_F2FS_VERSION {
        return None;
    }
    out.get_mut(..4)?.copy_from_slice(&ACL_POSIX_VERSION.to_le_bytes());
    let (mut r, mut w) = (4usize, 4usize);
    while r < v.len() {
        let tag = u16::from_le_bytes([*v.get(r)?, *v.get(r + 1)?]);
        let perm = [*v.get(r + 2)?, *v.get(r + 3)?];
        let id = if acl_short_tag(tag) {
            r += 4;
            0xFFFF_FFFFu32
        } else {
            let id = u32::from_le_bytes([*v.get(r + 4)?, *v.get(r + 5)?, *v.get(r + 6)?, *v.get(r + 7)?]);
            r += 8;
            id
        };
        let o = out.get_mut(w..w + 8)?;
        o[0..2].copy_from_slice(&tag.to_le_bytes());
        o[2..4].copy_from_slice(&perm);
        o[4..8].copy_from_slice(&id.to_le_bytes());
        w += 8;
    }
    Some(w)
}

/// posix_acl_xattr value -> f2fs ACL value. Returns the new length.
fn acl_posix_to_f2fs(v: &[u8], out: &mut [u8]) -> Option<usize> {
    if v.len() < 4 || u32::from_le_bytes([v[0], v[1], v[2], v[3]]) != ACL_POSIX_VERSION || (v.len() - 4) % 8 != 0 {
        return None;
    }
    out.get_mut(..4)?.copy_from_slice(&ACL_F2FS_VERSION.to_le_bytes());
    let mut w = 4usize;
    for e in v[4..].chunks_exact(8) {
        let tag = u16::from_le_bytes([e[0], e[1]]);
        let n = if acl_short_tag(tag) { 4 } else { 8 };
        out.get_mut(w..w + n)?.copy_from_slice(&e[..n]);
        w += n;
    }
    Some(w)
}

/// Decode a raw lx store (`inline ++ xnode`) into `out`, a zeroed arena.
/// A missing/odd header or malformed tail yields what parsed so far.
pub fn lx_decode(raw: &[u8], out: &mut [u8]) {
    if raw.len() < LX_HDR + 4 || u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) != LX_MAGIC {
        return;
    }
    let (mut pos, mut w) = (LX_HDR, 0usize);
    while pos + 4 <= raw.len() && raw[pos] != 0 {
        let idx = raw[pos];
        let nl = raw[pos + 1] as usize;
        let vs = u16::from_le_bytes([raw[pos + 2], raw[pos + 3]]) as usize;
        let elen = align4(4 + nl + vs);
        if pos + 4 + nl + vs > raw.len() {
            break;
        }
        let name = &raw[pos + 4..pos + 4 + nl];
        let val = &raw[pos + 4 + nl..pos + 4 + nl + vs];
        if w + 4 + nl + 4 > out.len() {
            break;
        }
        out[w] = idx;
        out[w + 1] = nl as u8;
        out[w + 4..w + 4 + nl].copy_from_slice(name);
        let vo = w + 4 + nl;
        let vlen = if idx == IDX_ACL_ACCESS || idx == IDX_ACL_DEFAULT {
            match acl_f2fs_to_posix(val, &mut out[vo..]) {
                Some(n) => n,
                None => break,
            }
        } else if vo + vs <= out.len() {
            out[vo..vo + vs].copy_from_slice(val);
            vs
        } else {
            break;
        };
        out[w + 2..w + 4].copy_from_slice(&(vlen as u16).to_le_bytes());
        w += align4(4 + nl + vlen);
        pos += elen;
    }
}

/// Encode an arena into `raw` (zeroed, `inline ++ xnode` sized). Returns the
/// used length including the 4-byte terminator, or ENOSPC when it won't fit.
pub fn lx_encode(arena: &[u8], raw: &mut [u8]) -> Result<usize, i32> {
    if raw.len() < LX_HDR + 4 {
        return Err(ENOSPC);
    }
    raw[0..4].copy_from_slice(&LX_MAGIC.to_le_bytes());
    raw[4..8].copy_from_slice(&1u32.to_le_bytes());
    let mut pos = LX_HDR;
    for (_, idx, name, val) in entries(arena) {
        if pos + align4(4 + name.len() + val.len()) + 4 > raw.len() {
            return Err(ENOSPC);
        }
        raw[pos] = idx;
        raw[pos + 1] = name.len() as u8;
        raw[pos + 4..pos + 4 + name.len()].copy_from_slice(name);
        let vo = pos + 4 + name.len();
        let vlen = if idx == IDX_ACL_ACCESS || idx == IDX_ACL_DEFAULT {
            acl_posix_to_f2fs(val, &mut raw[vo..]).ok_or(EINVAL)?
        } else {
            raw[vo..vo + val.len()].copy_from_slice(val);
            val.len()
        };
        raw[pos + 2..pos + 4].copy_from_slice(&(vlen as u16).to_le_bytes());
        pos += align4(4 + name.len() + vlen);
        if pos + 4 > raw.len() {
            return Err(ENOSPC);
        }
    }
    Ok(pos + 4)
}
