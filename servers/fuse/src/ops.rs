//! VFS-facing half: translate mount-port requests into FUSE requests.
use super::*;
use alloc::string::String;
use vfs_server::*;

const O_ACCMODE: u64 = 3;
const O_CREAT: u64 = 0o100;
const O_EXCL: u64 = 0o200;
const O_TRUNC: u64 = 0o1000;
const O_APPEND: u64 = 0o2000;
const O_NOFOLLOW: u64 = 0o400000;
const O_DIRECTORY: u64 = 0o200000;
const FUSE_SUPER_MAGIC: u64 = 0x6573_5546;

pub struct Cr { pub uid: u32, pub gid: u32, pub pid: u32 }

fn cstr<'a>(p: u64) -> &'a [u8] {
    unsafe {
        let ptr = p as *const u8;
        let mut n = 0;
        while *ptr.add(n) != 0 { n += 1; }
        core::slice::from_raw_parts(ptr, n)
    }
}
fn put(dst: u64, src: &[u8]) -> bool {
    src.is_empty() || unsafe { sched::uaccess::copy_raw(dst as *mut u8, src.as_ptr(), src.len()) } == 0
}
fn get(src: u64, n: usize) -> Option<Vec<u8>> {
    let mut v = alloc::vec![0u8; n];
    if n == 0 || unsafe { sched::uaccess::copy_raw(v.as_mut_ptr(), src as *const u8, n) } == 0 { Some(v) } else { None }
}
fn nul(name: &[u8]) -> Vec<u8> { let mut v = name.to_vec(); v.push(0); v }
fn e(r: i32) -> Message { reply(r as i64) }

fn rel<'a>(prefix: &str, path: &'a [u8]) -> &'a [u8] {
    let pb = prefix.as_bytes();
    if path.starts_with(pb) && (path.len() == pb.len() || path.get(pb.len()) == Some(&b'/') || pb == b"/") {
        &path[pb.len()..]
    } else { path }
}

fn forget(ci: usize, ids: &[u64], c: &Cr) {
    for &id in ids { if id > 1 { post(ci, FORGET, id, c, &[&1u64.to_le_bytes()]); } }
}

fn getattr(ci: usize, nodeid: u64, fh: Option<u64>, c: &Cr) -> Result<Attr, i32> {
    let mut b = [0u8; 16];
    if let Some(h) = fh { b[0] = 1; b[8..16].copy_from_slice(&h.to_le_bytes()); }
    let r = call(ci, GETATTR, nodeid, c, &[&b])?;
    if r.len() < 16 + ATTR_SZ { return Err(-5); }
    Ok(Attr::from(&r[16..]))
}

fn lookup(ci: usize, parent: u64, name: &[u8], c: &Cr, looked: &mut Vec<u64>) -> Result<(u64, Attr), i32> {
    let r = call(ci, LOOKUP, parent, c, &[&nul(name)])?;
    if r.len() < ENTRY_SZ { return Err(-5); }
    let id = u64_at(&r, 0);
    if id == 0 { return Err(-2); }
    looked.push(id);
    Ok((id, Attr::from(&r[40..])))
}

enum Hit { Found { dir: u64, name: Vec<u8>, id: u64, attr: Attr }, Parent { dir: u64, name: Vec<u8> } }

/// Walk `path` (mount-relative). `want_parent`: stop before the last component.
fn walk(ci: usize, path: &[u8], follow: bool, want_parent: bool, c: &Cr, looked: &mut Vec<u64>) -> Result<Hit, i32> {
    let mut stack: Vec<u64> = alloc::vec![1];
    let mut comps: VecDeque<Vec<u8>> = path.split(|&b| b == b'/').map(|s| s.to_vec()).collect();
    let mut links = 0;
    while let Some(cp) = comps.pop_front() {
        if cp.is_empty() || cp == b"." { continue; }
        if cp == b".." { if stack.len() > 1 { stack.pop(); } continue; }
        let last = comps.iter().all(|x| x.is_empty() || x == b".");
        let dir = *stack.last().unwrap();
        if last && want_parent { return Ok(Hit::Parent { dir, name: cp }); }
        let (id, attr) = lookup(ci, dir, &cp, c, looked)?;
        if attr.is_lnk() && (!last || follow) {
            links += 1;
            if links > 40 { return Err(-40); }
            let t = call(ci, READLINK, id, c, &[])?;
            if t.first() == Some(&b'/') { return Err(-2); }
            for s in t.split(|&b| b == b'/').rev() { comps.push_front(s.to_vec()); }
            continue;
        }
        if last { return Ok(Hit::Found { dir, name: cp, id, attr }); }
        if !attr.is_dir() { return Err(-20); }
        stack.push(id);
    }
    if want_parent { return Err(-22); }
    let a = getattr(ci, 1, None, c)?;
    Ok(Hit::Found { dir: 1, name: Vec::new(), id: 1, attr: a })
}

fn parent_of(ci: usize, path: &[u8], c: &Cr, looked: &mut Vec<u64>) -> Result<(u64, Vec<u8>), i32> {
    match walk(ci, path, false, true, c, looked)? {
        Hit::Parent { dir, name } => Ok((dir, name)),
        _ => Err(-22),
    }
}

fn fill_stat(a: &Attr, stat_ptr: u64) {
    let rd = a.rdev();
    let rdev = vfs_server::makedev(rd >> 8, rd & 0xff);
    write_stat_full_rdev(stat_ptr as usize, a.mode(), a.nlink() as u64, a.size(), a.ino(), a.uid(), a.gid(), rdev);
    let b = a.blocks();
    unsafe { sched::uaccess::copy_raw((stat_ptr as *mut u8).add(64), &b as *const u64 as *const u8, 8); }
    write_stat_times(stat_ptr as usize, a.atime(), a.mtime(), a.ctime());
}

fn setattr(ci: usize, id: u64, valid: u32, fh: Option<u64>, f: impl FnOnce(&mut [u8; 88]), c: &Cr) -> Result<Attr, i32> {
    let mut b = [0u8; 88];
    let mut v = valid;
    if let Some(h) = fh { v |= 64; b[8..16].copy_from_slice(&h.to_le_bytes()); }
    b[0..4].copy_from_slice(&v.to_le_bytes());
    f(&mut b);
    let r = call(ci, SETATTR, id, c, &[&b])?;
    if r.len() < 16 + ATTR_SZ { return Err(-5); }
    Ok(Attr::from(&r[16..]))
}

fn rc<T>(r: Result<T, i32>) -> Result<T, Message> { r.map_err(e) }

pub fn dispatch(msg: &Message, caller: u32, port: u32) -> Message {
    let Some(ci) = conn_by_port(port) else { return e(-19) };
    let (prefix, uid, allow, dead, rdonly) = {
        let mut g = CONNS.lock();
        let cn = conn_mut(&mut g, ci).unwrap();
        (cn.prefix, cn.uid, cn.allow_other, cn.dead, cn.rdonly)
    };
    let cred = vfs_server::cred_of(caller);
    let c = Cr { uid: cred.euid, gid: cred.egid, pid: caller };
    if msg.tag == VFS_CLOSE { return close(ci, arg(msg, 0) as usize, &c); }
    if dead { return e(ENOTCONN); }
    if !allow && cred.euid != uid { return e(-13); }
    if rdonly && matches!(msg.tag, VFS_WRITE | VFS_MKDIR | VFS_UNLINK | VFS_RMDIR | VFS_RENAME | VFS_FTRUNCATE
        | VFS_FALLOCATE | VFS_SYMLINK | VFS_LINK | VFS_CHMOD | VFS_LCHMOD | VFS_FCHMOD | VFS_CHOWN
        | VFS_LCHOWN | VFS_FCHOWN | VFS_SETXATTR | VFS_LSETXATTR | VFS_FSETXATTR | VFS_REMOVEXATTR
        | VFS_LREMOVEXATTR | VFS_FREMOVEXATTR | VFS_UTIMENS | VFS_LUTIMENS | VFS_FUTIMENS) {
        return e(-30);
    }
    let mut looked: Vec<u64> = Vec::new();
    let r = match run(ci, prefix, msg, &c, &mut looked) { Ok(m) | Err(m) => m };
    forget(ci, &looked, &c);
    r
}

fn nodefile(ci: usize, fid: u64) -> Result<(u64, u64, bool, u64), Message> {
    let mut g = CONNS.lock();
    let cn = conn_mut(&mut g, ci).unwrap();
    match cn.files.get(fid as usize).and_then(|f| f.as_ref()) {
        Some(f) => Ok((f.nodeid, f.fh, f.dir, f.pos)),
        None => Err(e(-9)),
    }
}
fn setpos(ci: usize, fid: u64, pos: u64) {
    let mut g = CONNS.lock();
    if let Some(Some(f)) = conn_mut(&mut g, ci).unwrap().files.get_mut(fid as usize) { f.pos = pos; }
}
fn maxw(ci: usize) -> usize {
    let mut g = CONNS.lock();
    conn_mut(&mut g, ci).unwrap().max_write as usize
}

fn run(ci: usize, prefix: &str, msg: &Message, c: &Cr, looked: &mut Vec<u64>) -> Result<Message, Message> {
    let a = |n| arg(msg, n);
    match msg.tag {
        VFS_STAT | VFS_LSTAT => {
            let p = cstr(a(0));
            match rc(walk(ci, rel(prefix, p), msg.tag == VFS_STAT, false, c, looked))? {
                Hit::Found { attr, .. } => { fill_stat(&attr, a(1)); Ok(reply(0)) }
                _ => Err(e(-2)),
            }
        }
        VFS_FSTAT => {
            let (id, fh, dir, _) = nodefile(ci, a(0))?;
            let at = rc(getattr(ci, id, if dir { None } else { Some(fh) }, c))?;
            fill_stat(&at, a(1));
            Ok(reply(0))
        }
        VFS_OPEN => open(ci, prefix, cstr(a(0)), a(1), a(2), c, looked),
        VFS_READ => {
            let (id, fh, dir, mut pos) = nodefile(ci, a(0))?;
            if dir { return Err(e(-21)); }
            let (buf, count) = (a(1), (a(2) as usize).min(1 << 20));
            let mut done = 0usize;
            while done < count {
                let n = (count - done).min(maxw(ci));
                let mut ri = [0u8; 40];
                ri[0..8].copy_from_slice(&fh.to_le_bytes());
                ri[8..16].copy_from_slice(&pos.to_le_bytes());
                ri[16..20].copy_from_slice(&(n as u32).to_le_bytes());
                let r = match call(ci, READ, id, c, &[&ri]) { Ok(r) => r, Err(x) => { if done > 0 { break; } return Err(e(x)); } };
                let got = r.len().min(n);
                if !put(buf + done as u64, &r[..got]) { return Err(e(-14)); }
                done += got; pos += got as u64;
                if got < n { break; }
            }
            setpos(ci, a(0), pos);
            Ok(reply(done as i64))
        }
        VFS_WRITE => {
            let (id, fh, dir, mut pos) = nodefile(ci, a(0))?;
            if dir { return Err(e(-21)); }
            if a(3) != 0 { pos = rc(getattr(ci, id, Some(fh), c))?.size(); }
            let count = a(2) as usize;
            let mut done = 0usize;
            while done < count {
                let n = (count - done).min(maxw(ci));
                let Some(data) = get(a(1) + done as u64, n) else { return Err(e(-14)) };
                let mut wi = [0u8; 40];
                wi[0..8].copy_from_slice(&fh.to_le_bytes());
                wi[8..16].copy_from_slice(&pos.to_le_bytes());
                wi[16..20].copy_from_slice(&(n as u32).to_le_bytes());
                let r = match call(ci, WRITE, id, c, &[&wi, &data]) { Ok(r) => r, Err(x) => { if done > 0 { break; } return Err(e(x)); } };
                let w = (u32_at(&r, 0) as usize).min(n);
                done += w; pos += w as u64;
                if w < n { break; }
            }
            setpos(ci, a(0), pos);
            Ok(reply(done as i64))
        }
        VFS_LSEEK => {
            let (id, fh, dir, pos) = nodefile(ci, a(0))?;
            let off = a(1);
            let np = match a(2) {
                0 => off,
                1 => pos.wrapping_add(off),
                2 if !dir => rc(getattr(ci, id, Some(fh), c))?.size().wrapping_add(off),
                _ => return Err(e(-22)),
            };
            setpos(ci, a(0), np);
            Ok(reply(np as i64))
        }
        VFS_GETDENTS64 => getdents(ci, a(0), a(1), a(2) as usize, c),
        VFS_FSYNC => {
            let (id, fh, dir, _) = nodefile(ci, a(0))?;
            let mut b = [0u8; 16];
            b[0..8].copy_from_slice(&fh.to_le_bytes());
            b[8..12].copy_from_slice(&((a(1) == 2) as u32).to_le_bytes());
            match call(ci, if dir { FSYNCDIR } else { FSYNC }, id, c, &[&b]) {
                Err(-38) | Ok(_) => Ok(reply(0)),
                Err(x) => Err(e(x)),
            }
        }
        VFS_FTRUNCATE => {
            let (id, fh, _, _) = nodefile(ci, a(0))?;
            let len = a(1);
            rc(setattr(ci, id, 8, Some(fh), |b| b[16..24].copy_from_slice(&len.to_le_bytes()), c))?;
            Ok(reply(0))
        }
        VFS_FALLOCATE => {
            let (id, fh, _, _) = nodefile(ci, a(0))?;
            let mut b = [0u8; 32];
            b[0..8].copy_from_slice(&fh.to_le_bytes());
            b[8..16].copy_from_slice(&a(2).to_le_bytes());
            b[16..24].copy_from_slice(&a(3).to_le_bytes());
            b[24..28].copy_from_slice(&(a(1) as u32).to_le_bytes());
            rc(call(ci, FALLOCATE, id, c, &[&b]))?;
            Ok(reply(0))
        }
        VFS_STATFS => {
            let r = rc(call(ci, STATFS, 1, c, &[]))?;
            let v = StatfsVals {
                f_type: FUSE_SUPER_MAGIC, bsize: (u32_at(&r, 40) as u64).max(512), blocks: u64_at(&r, 0).max(1),
                bfree: u64_at(&r, 8), bavail: u64_at(&r, 16), files: u64_at(&r, 24), ffree: u64_at(&r, 32),
                fsid: 0, namelen: if u32_at(&r, 44) == 0 { 255 } else { u32_at(&r, 44) as u64 },
            };
            write_statfs(a(1) as usize, &v);
            Ok(reply(0))
        }
        VFS_MKDIR => {
            let (dir, name) = rc(parent_of(ci, rel(prefix, cstr(a(0))), c, looked))?;
            let mut b = [0u8; 8];
            b[0..4].copy_from_slice(&((a(1) as u32) & 0o7777).to_le_bytes());
            let r = rc(call(ci, MKDIR, dir, c, &[&b, &nul(&name)]))?;
            if r.len() >= 8 { looked.push(u64_at(&r, 0)); }
            Ok(reply(0))
        }
        VFS_UNLINK | VFS_RMDIR => {
            let (dir, name) = rc(parent_of(ci, rel(prefix, cstr(a(0))), c, looked))?;
            rc(call(ci, if msg.tag == VFS_UNLINK { UNLINK } else { RMDIR }, dir, c, &[&nul(&name)]))?;
            files_deleted(ci, cstr(a(0)));
            Ok(reply(0))
        }
        VFS_RENAME => {
            let (od, on) = rc(parent_of(ci, rel(prefix, cstr(a(0))), c, looked))?;
            let (nd, nn) = rc(parent_of(ci, rel(prefix, cstr(a(1))), c, looked))?;
            let flags = a(2) as u32;
            if flags == 0 {
                rc(call(ci, RENAME, od, c, &[&nd.to_le_bytes(), &nul(&on), &nul(&nn)]))?;
            } else {
                let mut h = [0u8; 16];
                h[0..8].copy_from_slice(&nd.to_le_bytes());
                h[8..12].copy_from_slice(&flags.to_le_bytes());
                rc(call(ci, RENAME2, od, c, &[&h, &nul(&on), &nul(&nn)]))?;
            }
            // A replaced target is gone (RENAME_EXCHANGE swaps both names).
            if flags & 2 == 0 { files_deleted(ci, cstr(a(1))); }
            files_renamed(ci, cstr(a(0)), cstr(a(1)), flags & 2 != 0);
            Ok(reply(0))
        }
        VFS_SYMLINK => {
            let target = cstr(a(0)).to_vec();
            let (dir, name) = rc(parent_of(ci, rel(prefix, cstr(a(1))), c, looked))?;
            let r = rc(call(ci, SYMLINK, dir, c, &[&nul(&name), &nul(&target)]))?;
            if r.len() >= 8 { looked.push(u64_at(&r, 0)); }
            Ok(reply(0))
        }
        VFS_READLINK => {
            let id = match rc(walk(ci, rel(prefix, cstr(a(0))), false, false, c, looked))? {
                Hit::Found { id, attr, .. } => { if !attr.is_lnk() { return Err(e(-22)); } id }
                _ => return Err(e(-2)),
            };
            let t = rc(call(ci, READLINK, id, c, &[]))?;
            let n = t.len().min(a(2) as usize);
            if !put(a(1), &t[..n]) { return Err(e(-14)); }
            Ok(reply(n as i64))
        }
        VFS_LINK => {
            let id = match rc(walk(ci, rel(prefix, cstr(a(0))), false, false, c, looked))? {
                Hit::Found { id, .. } => id, _ => return Err(e(-2)),
            };
            let (nd, nn) = rc(parent_of(ci, rel(prefix, cstr(a(1))), c, looked))?;
            let r = rc(call(ci, LINK, nd, c, &[&id.to_le_bytes(), &nul(&nn)]))?;
            if r.len() >= 8 { looked.push(u64_at(&r, 0)); }
            Ok(reply(0))
        }
        VFS_CHMOD | VFS_LCHMOD | VFS_CHOWN | VFS_LCHOWN | VFS_UTIMENS | VFS_LUTIMENS => {
            let follow = matches!(msg.tag, VFS_CHMOD | VFS_CHOWN | VFS_UTIMENS);
            let id = match rc(walk(ci, rel(prefix, cstr(a(0))), follow, false, c, looked))? {
                Hit::Found { id, .. } => id, _ => return Err(e(-2)),
            };
            attr_change(ci, id, None, msg, c)
        }
        VFS_FCHMOD | VFS_FCHOWN | VFS_FUTIMENS => {
            let (id, fh, dir, _) = nodefile(ci, a(0))?;
            attr_change(ci, id, if dir { None } else { Some(fh) }, msg, c)
        }
        VFS_ACCESS => {
            let id = match rc(walk(ci, rel(prefix, cstr(a(0))), true, false, c, looked))? {
                Hit::Found { id, .. } => id, _ => return Err(e(-2)),
            };
            let mut b = [0u8; 8];
            b[0..4].copy_from_slice(&(a(1) as u32).to_le_bytes());
            match call(ci, ACCESS, id, c, &[&b]) { Err(-38) | Ok(_) => Ok(reply(0)), Err(x) => Err(e(x)) }
        }
        VFS_FD_PATH => {
            let path = {
                let mut g = CONNS.lock();
                match conn_mut(&mut g, ci).unwrap().files.get(a(0) as usize).and_then(|f| f.as_ref()) {
                    Some(f) => f.path.clone(), None => return Err(e(-9)),
                }
            };
            let n = path.len().min(a(2) as usize);
            if !put(a(1), &path[..n]) { return Err(e(-14)); }
            Ok(reply(n as i64))
        }
        VFS_SETXATTR | VFS_LSETXATTR | VFS_FSETXATTR | VFS_GETXATTR | VFS_LGETXATTR | VFS_FGETXATTR
        | VFS_LISTXATTR | VFS_LLISTXATTR | VFS_FLISTXATTR | VFS_REMOVEXATTR | VFS_LREMOVEXATTR | VFS_FREMOVEXATTR =>
            xattr(ci, prefix, msg, c, looked),
        _ => Err(e(-22)),
    }
}

fn attr_change(ci: usize, id: u64, fh: Option<u64>, msg: &Message, c: &Cr) -> Result<Message, Message> {
    let a = |n| arg(msg, n);
    match msg.tag {
        VFS_CHMOD | VFS_LCHMOD | VFS_FCHMOD => {
            let m = a(1) as u32 & 0o7777;
            rc(setattr(ci, id, 1, fh, |b| b[68..72].copy_from_slice(&m.to_le_bytes()), c))?;
        }
        VFS_CHOWN | VFS_LCHOWN | VFS_FCHOWN => {
            let (u, g) = (a(1) as u32, a(2) as u32);
            let mut v = 0;
            if u != u32::MAX { v |= 2; }
            if g != u32::MAX { v |= 4; }
            if v != 0 {
                rc(setattr(ci, id, v, fh, |b| {
                    b[76..80].copy_from_slice(&u.to_le_bytes());
                    b[80..84].copy_from_slice(&g.to_le_bytes());
                }, c))?;
            }
        }
        _ => {
            const NOW: i64 = (1 << 30) - 1;
            const OMIT: i64 = (1 << 30) - 2;
            let explicit = a(5) & UTIMENS_EXPLICIT != 0;
            let (as_, an, ms_, mn) = (a(1) as i64, a(2) as i64, a(3) as i64, a(4) as i64);
            let mut v = 0u32;
            let mut b = [0u8; 88];
            if !explicit || an == NOW { v |= 16 | 128; } else if an != OMIT {
                v |= 16; b[32..40].copy_from_slice(&as_.to_le_bytes()); b[56..60].copy_from_slice(&(an as u32).to_le_bytes());
            }
            if !explicit || mn == NOW { v |= 32 | 256; } else if mn != OMIT {
                v |= 32; b[40..48].copy_from_slice(&ms_.to_le_bytes()); b[60..64].copy_from_slice(&(mn as u32).to_le_bytes());
            }
            if v != 0 {
                rc(setattr(ci, id, v, fh, |x| { let (h, t) = (x[0..8].to_vec(), b); let _ = h; x[16..].copy_from_slice(&t[16..]); }, c))?;
            }
        }
    }
    Ok(reply(0))
}

fn xattr(ci: usize, prefix: &str, msg: &Message, c: &Cr, looked: &mut Vec<u64>) -> Result<Message, Message> {
    let a = |n| arg(msg, n);
    let fform = matches!(msg.tag, VFS_FSETXATTR | VFS_FGETXATTR | VFS_FLISTXATTR | VFS_FREMOVEXATTR);
    let follow = matches!(msg.tag, VFS_SETXATTR | VFS_GETXATTR | VFS_LISTXATTR | VFS_REMOVEXATTR);
    let id = if fform { nodefile(ci, a(0))?.0 } else {
        match rc(walk(ci, rel(prefix, cstr(a(0))), follow, false, c, looked))? {
            Hit::Found { id, .. } => id, _ => return Err(e(-2)),
        }
    };
    match msg.tag {
        VFS_SETXATTR | VFS_LSETXATTR | VFS_FSETXATTR => {
            let name = cstr(a(1)).to_vec();
            let size = a(3) as usize;
            let Some(val) = get(a(2), size) else { return Err(e(-14)) };
            let mut h = [0u8; 8];
            h[0..4].copy_from_slice(&(size as u32).to_le_bytes());
            h[4..8].copy_from_slice(&(a(4) as u32).to_le_bytes());
            rc(call(ci, SETXATTR, id, c, &[&h, &nul(&name), &val]))?;
            Ok(reply(0))
        }
        VFS_GETXATTR | VFS_LGETXATTR | VFS_FGETXATTR => {
            let name = cstr(a(1)).to_vec();
            let size = a(3) as u32;
            let mut h = [0u8; 8];
            h[0..4].copy_from_slice(&size.to_le_bytes());
            let r = rc(call(ci, GETXATTR, id, c, &[&h, &nul(&name)]))?;
            if size == 0 { return Ok(reply(u32_at(&r, 0) as i64)); }
            if r.len() > size as usize { return Err(e(-34)); }
            if !put(a(2), &r) { return Err(e(-14)); }
            Ok(reply(r.len() as i64))
        }
        VFS_LISTXATTR | VFS_LLISTXATTR | VFS_FLISTXATTR => {
            let size = a(2) as u32;
            let mut h = [0u8; 8];
            h[0..4].copy_from_slice(&size.to_le_bytes());
            let r = rc(call(ci, LISTXATTR, id, c, &[&h]))?;
            if size == 0 { return Ok(reply(u32_at(&r, 0) as i64)); }
            if r.len() > size as usize { return Err(e(-34)); }
            if !put(a(1), &r) { return Err(e(-14)); }
            Ok(reply(r.len() as i64))
        }
        _ => {
            let name = cstr(a(1)).to_vec();
            rc(call(ci, REMOVEXATTR, id, c, &[&nul(&name)]))?;
            Ok(reply(0))
        }
    }
}

fn alloc_file(ci: usize, f: OFile) -> i64 {
    let mut g = CONNS.lock();
    let cn = conn_mut(&mut g, ci).unwrap();
    match cn.files.iter().position(|x| x.is_none()) {
        Some(i) => { cn.files[i] = Some(f); i as i64 }
        None => { cn.files.push(Some(f)); (cn.files.len() - 1) as i64 }
    }
}

/// Open files name the path they were opened by (there are no dentries), so
/// namespace changes are applied to those paths: a rename moves `old` and
/// everything below it to `new`; readlink of /proc/self/fd/N follows it.
fn files_renamed(ci: usize, old: &[u8], new: &[u8], exchange: bool) {
    let mut g = CONNS.lock();
    let Some(cn) = conn_mut(&mut g, ci) else { return };
    let moved = |p: &[u8], from: &[u8], to: &[u8]| -> Option<Vec<u8>> {
        if p == from { Some(to.to_vec()) }
        else if p.len() > from.len() && p.starts_with(from) && p[from.len()] == b'/' {
            let mut v = to.to_vec(); v.extend_from_slice(&p[from.len()..]); Some(v)
        } else { None }
    };
    for f in cn.files.iter_mut().flatten() {
        if let Some(v) = moved(&f.path, old, new) { f.path = v; }
        else if exchange { if let Some(v) = moved(&f.path, new, old) { f.path = v; } }
    }
}

/// `path` was removed: descriptors opened by it read as `<path> (deleted)`.
fn files_deleted(ci: usize, path: &[u8]) {
    let mut g = CONNS.lock();
    let Some(cn) = conn_mut(&mut g, ci) else { return };
    for f in cn.files.iter_mut().flatten() {
        if f.path == path { f.path.extend_from_slice(b" (deleted)"); }
    }
}

fn open(ci: usize, prefix: &str, path: &[u8], flags: u64, mode: u64, c: &Cr, looked: &mut Vec<u64>) -> Result<Message, Message> {
    let rp = rel(prefix, path);
    let acc = flags & O_ACCMODE;
    let writable = acc != 0;
    let nofollow = flags & O_NOFOLLOW != 0;
    let mut open_flags = (acc | (flags & O_APPEND)) as u32;
    let mut keep: Option<u64> = None; // node whose lookup the open file keeps
    let (id, fh, dir) = match walk(ci, rp, !nofollow, false, c, looked) {
        Ok(Hit::Found { id, attr, .. }) => {
            if flags & O_CREAT != 0 && flags & O_EXCL != 0 { return Err(e(-17)); }
            if attr.is_lnk() { return Err(e(-40)); }
            let dir = attr.is_dir();
            if dir && writable { return Err(e(-21)); }
            if !dir && flags & O_DIRECTORY != 0 { return Err(e(-20)); }
            if !dir && flags & O_TRUNC != 0 && writable {
                rc(setattr(ci, id, 8, None, |_| {}, c))?;
            }
            let mut oi = [0u8; 8];
            if dir { open_flags = 0; }
            oi[0..4].copy_from_slice(&open_flags.to_le_bytes());
            let r = rc(call(ci, if dir { OPENDIR } else { OPEN }, id, c, &[&oi]))?;
            keep = Some(id);
            (id, u64_at(&r, 0), dir)
        }
        Err(-2) if flags & O_CREAT != 0 => {
            let (pd, name) = rc(parent_of(ci, rp, c, looked))?;
            let mut ci_ = [0u8; 16];
            ci_[0..4].copy_from_slice(&open_flags.to_le_bytes());
            ci_[4..8].copy_from_slice(&((mode as u32) & 0o7777 | 0o100000).to_le_bytes());
            let r = rc(call(ci, CREATE, pd, c, &[&ci_, &nul(&name)]))?;
            if r.len() < ENTRY_SZ + 16 { return Err(e(-5)); }
            let id = u64_at(&r, 0);
            keep = Some(id);
            (id, u64_at(&r, ENTRY_SZ), false)
        }
        Err(x) => return Err(e(x)),
        Ok(_) => return Err(e(-2)),
    };
    if let Some(k) = keep { if let Some(i) = looked.iter().position(|&x| x == k) { looked.remove(i); } }
    let mut p = Vec::from(path);
    if p.is_empty() { p.push(b'/'); }
    Ok(reply(alloc_file(ci, OFile { nodeid: id, fh, dir, writable, append: flags & O_APPEND != 0, pos: 0, path: p })))
}

fn close(ci: usize, fid: usize, c: &Cr) -> Message {
    let f = {
        let mut g = CONNS.lock();
        let Some(cn) = conn_mut(&mut g, ci) else { return e(-9) };
        match cn.files.get_mut(fid).and_then(|f| f.take()) { Some(f) => f, None => return e(-9) }
    };
    let mut b = [0u8; 24];
    b[0..8].copy_from_slice(&f.fh.to_le_bytes());
    if !f.dir { let _ = call(ci, FLUSH, f.nodeid, c, &[&b]); }
    let _ = call(ci, if f.dir { RELEASEDIR } else { RELEASE }, f.nodeid, c, &[&b]);
    forget(ci, &[f.nodeid], c);
    reply(0)
}

fn getdents(ci: usize, fid: u64, buf: u64, count: usize, c: &Cr) -> Result<Message, Message> {
    let (id, fh, dir, mut pos) = nodefile(ci, fid)?;
    if !dir { return Err(e(-20)); }
    let mut ri = [0u8; 40];
    ri[0..8].copy_from_slice(&fh.to_le_bytes());
    ri[8..16].copy_from_slice(&pos.to_le_bytes());
    ri[16..20].copy_from_slice(&16384u32.to_le_bytes());
    let r = rc(call(ci, READDIR, id, c, &[&ri]))?;
    let mut out: Vec<u8> = Vec::new();
    let mut o = 0usize;
    while o + 24 <= r.len() {
        let (ino, off, nl, ty) = (u64_at(&r, o), u64_at(&r, o + 8), u32_at(&r, o + 16) as usize, u32_at(&r, o + 20));
        if o + 24 + nl > r.len() { break; }
        let reclen = (19 + nl + 1 + 7) & !7;
        if out.len() + reclen > count { break; }
        let st = out.len();
        out.resize(st + reclen, 0);
        out[st..st + 8].copy_from_slice(&ino.to_le_bytes());
        out[st + 8..st + 16].copy_from_slice(&off.to_le_bytes());
        out[st + 16..st + 18].copy_from_slice(&(reclen as u16).to_le_bytes());
        out[st + 18] = ty as u8;
        out[st + 19..st + 19 + nl].copy_from_slice(&r[o + 24..o + 24 + nl]);
        pos = off;
        o += (24 + nl + 7) & !7;
    }
    if !put(buf, &out) { return Err(e(-14)); }
    setpos(ci, fid, pos);
    Ok(reply(out.len() as i64))
}

#[allow(dead_code)]
fn _unused(_: String) {}
