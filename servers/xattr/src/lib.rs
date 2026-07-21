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
//   trusted.* root only; hidden (ENODATA / filtered) from others.

/// Gate for getxattr. `acl` = the stored access ACL, if any.
pub fn may_read_xattr(idx: u8, meta: &FileMeta, euid: u32, egid: u32, acl: Option<&[u8]>) -> Result<(), i32> {
    match idx {
        IDX_USER => {
            if !is_reg(meta.mode) && !is_dir(meta.mode) {
                return Err(ENODATA);
            }
            if access_check(meta, euid, egid, acl, true, false, false) {
                Ok(())
            } else {
                Err(EACCES)
            }
        }
        IDX_ACL_ACCESS | IDX_ACL_DEFAULT => Ok(()),
        IDX_TRUSTED => {
            if euid == 0 {
                Ok(())
            } else {
                Err(ENODATA)
            }
        }
        _ => Err(EOPNOTSUPP),
    }
}

/// Gate for setxattr/removexattr.
pub fn may_write_xattr(idx: u8, meta: &FileMeta, euid: u32, egid: u32, acl: Option<&[u8]>) -> Result<(), i32> {
    if is_lnk(meta.mode) {
        return Err(EPERM); // no user.* and no ACLs on symlinks
    }
    match idx {
        IDX_USER => {
            if !is_reg(meta.mode) && !is_dir(meta.mode) {
                return Err(EPERM);
            }
            if is_dir(meta.mode) && meta.mode & S_ISVTX != 0 && euid != meta.uid && euid != 0 {
                return Err(EPERM);
            }
            if access_check(meta, euid, egid, acl, false, true, false) {
                Ok(())
            } else {
                Err(EACCES)
            }
        }
        IDX_ACL_ACCESS | IDX_ACL_DEFAULT => {
            if euid != meta.uid && euid != 0 {
                return Err(EPERM);
            }
            if idx == IDX_ACL_DEFAULT && !is_dir(meta.mode) {
                return Err(EACCES);
            }
            Ok(())
        }
        IDX_TRUSTED => {
            if euid == 0 {
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

/// Unified permission check for open/faccessat: POSIX 1003.1e ACL walk when
/// a (non-trivial, stored) access ACL is present, classic mode bits
/// otherwise. Root (euid 0) bypasses R/W always; X needs at least one x bit
/// unless the target is a directory.
///
/// Group matching is `egid == gid` only (no supplementary groups exist).
pub fn access_check(
    meta: &FileMeta,
    euid: u32,
    egid: u32,
    acl: Option<&[u8]>,
    want_r: bool,
    want_w: bool,
    want_x: bool,
) -> bool {
    let want: u16 = (want_r as u16) << 2 | (want_w as u16) << 1 | want_x as u16;
    if euid == 0 {
        if want_x && !is_dir(meta.mode) {
            return meta.mode & 0o111 != 0;
        }
        return true;
    }
    if let Some(bytes) = acl {
        if acl_validate(bytes).is_ok() {
            return acl_walk(bytes, meta, euid, egid, want);
        }
        // Corrupt stored ACL: fall through to mode bits (fail-open to the
        // mode, which the invariant keeps at least as strict as the mask).
    }
    let bits = if euid == meta.uid {
        (meta.mode >> 6) & 7
    } else if egid == meta.gid {
        (meta.mode >> 3) & 7
    } else {
        meta.mode & 7
    };
    bits & want == want
}

fn acl_walk(value: &[u8], meta: &FileMeta, euid: u32, egid: u32, want: u16) -> bool {
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
                if egid == meta.gid {
                    group_found = true;
                    if masked(e.perm) & want == want {
                        return true;
                    }
                }
            }
            ACL_GROUP => {
                if egid == e.id {
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
