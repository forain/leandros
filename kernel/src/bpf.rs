//! bpf(2) for the cgroup v2 device controller (design-bpfdev, lane bpfsys).
//!
//! Only BPF_PROG_TYPE_CGROUP_DEVICE programs exist; every other program type
//! and command answers EINVAL like an unknown command on Linux. The verifier
//! and interpreter are in `lib/ebpf-cgdev`; program and attachment state in
//! `sched::bpfcg`. Attaching enforces nothing yet (lane bpfenf).

use alloc::vec::Vec;
use sched::bpfcg;
use vfs_server as vfs;
use sched::uaccess as ua;

const BPF_PROG_LOAD: usize = 5;
const BPF_PROG_ATTACH: usize = 8;
const BPF_PROG_DETACH: usize = 9;
const BPF_PROG_GET_FD_BY_ID: usize = 13;
const BPF_PROG_QUERY: usize = 16;

const PROG_TYPE_CGROUP_DEVICE: u32 = 15;
const BPF_CGROUP_DEVICE: u32 = 6;
const BPF_F_QUERY_EFFECTIVE: u32 = 1;
const MAX_INSNS: u32 = 4096;
const PAGE: usize = 4096;
/// Bytes of `union bpf_attr` this implementation looks at.
const ATTR_LEN: usize = 128;

const CAP_NET_ADMIN: u32 = 12;
const CAP_SYS_ADMIN: u32 = 21;
const CAP_BPF: u32 = 39;

const EPERM: isize = -1;
const E2BIG: isize = -7;
const EFAULT: isize = -14;
const EINVAL: isize = -22;
const ENOSPC: isize = -28;

struct Attr([u8; ATTR_LEN]);
impl Attr {
    fn u32(&self, off: usize) -> u32 { u32::from_ne_bytes(self.0[off..off + 4].try_into().unwrap()) }
    fn u64(&self, off: usize) -> u64 { u64::from_ne_bytes(self.0[off..off + 8].try_into().unwrap()) }
}

/// Copy `min(size, ATTR_LEN)` bytes of the user attr; E2BIG when `size`
/// exceeds a page or the part we do not understand is not all zero
/// (`bpf_check_uarg_tail_zero`).
fn read_attr(uattr: usize, size: usize) -> Result<Attr, isize> {
    if size > PAGE { return Err(E2BIG); }
    let n = size.min(ATTR_LEN);
    let mut a = Attr([0u8; ATTR_LEN]);
    if n != 0 && unsafe { ua::copy_from_user(a.0.as_mut_ptr(), uattr, n) } != 0 { return Err(EFAULT); }
    let mut off = ATTR_LEN;
    while off < size {
        let m = (size - off).min(64);
        let mut buf = [0u8; 64];
        if unsafe { ua::copy_from_user(buf.as_mut_ptr(), uattr + off, m) } != 0 { return Err(EFAULT); }
        if buf[..m].iter().any(|&b| b != 0) { return Err(E2BIG); }
        off += m;
    }
    Ok(a)
}

fn put_user_bytes(dst: usize, src: &[u8]) -> bool {
    src.is_empty() || unsafe { ua::copy_to_user(dst, src.as_ptr(), src.len()) } == 0
}

fn capable_any(a: u32, b: u32) -> bool { sched::caps::host_capable(a) || sched::caps::host_capable(b) }

pub fn sys_bpf(cmd: usize, uattr: usize, size: usize) -> isize {
    if size == 0 && cmd != BPF_PROG_LOAD && cmd != BPF_PROG_ATTACH && cmd != BPF_PROG_DETACH
        && cmd != BPF_PROG_GET_FD_BY_ID && cmd != BPF_PROG_QUERY { return EINVAL; }
    match cmd {
        BPF_PROG_LOAD | BPF_PROG_ATTACH | BPF_PROG_DETACH | BPF_PROG_GET_FD_BY_ID | BPF_PROG_QUERY => {}
        _ => return EINVAL,
    }
    let attr = match read_attr(uattr, size) { Ok(a) => a, Err(e) => return e };
    match cmd {
        BPF_PROG_LOAD => prog_load(&attr),
        BPF_PROG_ATTACH => prog_attach(&attr),
        BPF_PROG_DETACH => prog_detach(&attr),
        BPF_PROG_GET_FD_BY_ID => prog_get_fd_by_id(&attr),
        _ => prog_query(&attr, uattr, size),
    }
}

fn prog_load(a: &Attr) -> isize {
    if !capable_any(CAP_BPF, CAP_SYS_ADMIN) { return EPERM; }
    if a.u32(0) != PROG_TYPE_CGROUP_DEVICE || a.u32(44) != 0 { return EINVAL; }
    let cnt = a.u32(4);
    if cnt == 0 || cnt > MAX_INSNS { return EINVAL; }
    let (insns, license) = (a.u64(8) as usize, a.u64(16) as usize);
    let (log_level, log_size, log_buf) = (a.u32(24), a.u32(28) as usize, a.u64(32) as usize);
    if log_level != 0 && (log_buf == 0 || log_size < 128) { return EINVAL; }
    // The license string must be readable (Linux strncpy_from_user -> EFAULT).
    let mut lic = [0u8; 1];
    if license == 0 || unsafe { ua::copy_from_user(lic.as_mut_ptr(), license, 1) } != 0 { return EFAULT; }
    let mut raw: Vec<u8> = alloc::vec![0u8; cnt as usize * 8];
    if unsafe { ua::copy_from_user(raw.as_mut_ptr(), insns, raw.len()) } != 0 { return EFAULT; }
    let prog = match bpfcg::Program::load(&raw) {
        Ok(p) => p,
        Err(e) => {
            if log_level != 0 {
                let msg = alloc::format!("{:?}\n", e);
                let mut out = msg.into_bytes();
                let fits = out.len() < log_size;
                out.truncate(log_size - 1);
                out.push(0);
                if !put_user_bytes(log_buf, &out) { return EFAULT; }
                if !fits { return ENOSPC; }
            }
            let n = e.errno() as isize;
            return if n > 0 { -n } else { n };
        }
    };
    if log_level != 0 && !put_user_bytes(log_buf, &[0u8]) { return EFAULT; }
    let id = match bpfcg::prog_new(prog) { Ok(i) => i, Err(e) => return e as isize };
    let fd = vfs::install_bpf_prog(sched::current_pid(), id);
    if fd < 0 { bpfcg::prog_put(id); }
    fd
}

/// Resolve the cgroup directory fd and attach type shared by ATTACH/DETACH/QUERY.
fn target(fd: u32, atype: u32) -> Result<sched::cgroup::CgId, isize> {
    if atype != BPF_CGROUP_DEVICE { return Err(EINVAL); }
    vfs::cgroup_of_fd(sched::current_pid(), fd as usize).map_err(|e| e as isize)
}

fn prog_attach(a: &Attr) -> isize {
    if !capable_any(CAP_NET_ADMIN, CAP_SYS_ADMIN) { return EPERM; }
    let me = sched::current_pid();
    let cg = match target(a.u32(0), a.u32(8)) { Ok(c) => c, Err(e) => return e };
    let flags = a.u32(12);
    let prog = match vfs::bpf_prog_of_fd(me, a.u32(4) as usize) { Ok(i) => i, Err(e) => return e as isize };
    let replace = if flags & bpfcg::BPF_F_REPLACE != 0 {
        match vfs::bpf_prog_of_fd(me, a.u32(16) as usize) { Ok(i) => Some(i), Err(e) => return e as isize }
    } else { None };
    match bpfcg::attach(cg, prog, flags, replace) { Ok(()) => 0, Err(e) => e as isize }
}

fn prog_detach(a: &Attr) -> isize {
    if !capable_any(CAP_NET_ADMIN, CAP_SYS_ADMIN) { return EPERM; }
    let cg = match target(a.u32(0), a.u32(8)) { Ok(c) => c, Err(e) => return e };
    let prog = match vfs::bpf_prog_of_fd(sched::current_pid(), a.u32(4) as usize) {
        Ok(i) => Some(i),
        Err(_) if a.u32(4) == 0 => None,
        Err(e) => return e as isize,
    };
    match bpfcg::detach(cg, prog) { Ok(()) => 0, Err(e) => e as isize }
}

fn prog_get_fd_by_id(a: &Attr) -> isize {
    if !capable_any(CAP_BPF, CAP_SYS_ADMIN) { return EPERM; }
    if a.u32(8) != 0 { return EINVAL; }
    let id = a.u32(0);
    if let Err(e) = bpfcg::get_by_id(id) { return e as isize; }
    let fd = vfs::install_bpf_prog(sched::current_pid(), id);
    if fd < 0 { bpfcg::prog_put(id); }
    fd
}

/// Writes `attach_flags` (@12) and `prog_cnt` (@24) back; copies at most the
/// caller's count of ids and answers ENOSPC only when they did not all fit.
fn prog_query(a: &Attr, uattr: usize, size: usize) -> isize {
    if !capable_any(CAP_NET_ADMIN, CAP_SYS_ADMIN) { return EPERM; }
    let qflags = a.u32(8);
    if qflags & !BPF_F_QUERY_EFFECTIVE != 0 { return EINVAL; }
    let cg = match target(a.u32(0), a.u32(4)) { Ok(c) => c, Err(e) => return e };
    let (aflags, ids) = match bpfcg::query(cg, qflags & BPF_F_QUERY_EFFECTIVE != 0) {
        Ok(v) => v, Err(e) => return e as isize,
    };
    let (user_ids, user_cnt) = (a.u64(16) as usize, a.u32(24) as usize);
    let total = ids.len() as u32;
    if size >= 16 && !put_user_bytes(uattr + 12, &aflags.to_ne_bytes()) { return EFAULT; }
    if size >= 28 && !put_user_bytes(uattr + 24, &total.to_ne_bytes()) { return EFAULT; }
    if user_ids == 0 { return 0; }
    let n = ids.len().min(user_cnt);
    let mut out: Vec<u8> = Vec::with_capacity(n * 4);
    for id in &ids[..n] { out.extend_from_slice(&id.to_ne_bytes()); }
    if !put_user_bytes(user_ids, &out) { return EFAULT; }
    if ids.len() > user_cnt { ENOSPC } else { 0 }
}
