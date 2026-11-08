//! seccomp filter storage (lane seccore, steps S1/S2 of design-seccomp).
//!
//! A thread's filter is a refcounted chain of programs (newest -> oldest).
//! The slot lives inline in `Task` (stable address, like `sig_hint`) so the
//! syscall fast path can test it with one Acquire load and no lock.
//!
//! Invariant making the lock-free read safe: a thread's head is only ever
//! replaced by a node whose chain CONTAINS the old head (self-install pushes
//! on top; TSYNC requires ancestry), so the old node stays alive through
//! `prev` while the owning thread may still be walking it. All writers hold
//! `RUN_QUEUE`; the only lock-free reader is the owning thread.
//!
//! The cBPF checker/interpreter lives in the pure crate `lib/seccomp-bpf`
//! (`check_filter`, `run_stack`).

use alloc::sync::Arc;
use core::sync::atomic::{AtomicPtr, Ordering};

pub use seccomp_bpf::{Program, SeccompData, SockFilter};

/// Linux MAX_INSNS_PER_PATH.
pub const MAX_INSNS_PER_PATH: u32 = 32768;

pub const MODE_DISABLED: u32 = 0;
pub const MODE_STRICT: u32 = 1;
pub const MODE_FILTER: u32 = 2;

pub struct FilterNode {
    /// Verified program.
    pub prog: Program,
    /// Older filter (chain = newest -> oldest).
    pub prev: Option<Arc<FilterNode>>,
    /// Sum over the chain of (len + 4); bounded by `MAX_INSNS_PER_PATH`.
    pub path_insns: u32,
    /// Chain length (for /proc Seccomp_filters).
    pub depth: u32,
    /// SECCOMP_FILTER_FLAG_LOG.
    pub log: bool,
}

impl FilterNode {
    /// New node on top of `prev`; `None` if the per-path budget is exceeded.
    pub fn push(prog: Program, prev: Option<Arc<FilterNode>>, log: bool)
        -> Option<Arc<FilterNode>>
    {
        let (pi, d) = prev.as_ref().map(|p| (p.path_insns, p.depth)).unwrap_or((0, 0));
        let path_insns = pi.checked_add(prog.len() as u32 + 4)?;
        if path_insns > MAX_INSNS_PER_PATH { return None; }
        Some(Arc::new(FilterNode { prog, prev, path_insns, depth: d + 1, log }))
    }
}

static STRICT_SENTINEL: u8 = 0;
fn strict_ptr() -> *mut FilterNode { &STRICT_SENTINEL as *const u8 as *mut FilterNode }

/// Inline in `Task`. null = mode 0; `strict_ptr()` = strict; else
/// `Arc::into_raw` of the newest node (the slot owns one strong count).
pub struct SeccompSlot {
    head: AtomicPtr<FilterNode>,
}

impl SeccompSlot {
    pub const fn new() -> Self { Self { head: AtomicPtr::new(core::ptr::null_mut()) } }

    /// Lock-free fast-path test: any seccomp state at all?
    #[inline]
    pub fn active(&self) -> bool { !self.head.load(Ordering::Acquire).is_null() }

    pub fn mode(&self) -> u32 {
        let p = self.head.load(Ordering::Acquire);
        if p.is_null() { MODE_DISABLED } else if p == strict_ptr() { MODE_STRICT } else { MODE_FILTER }
    }

    /// Borrow the newest node. Only the owning thread may call this without
    /// RUN_QUEUE (see the module invariant).
    pub fn head(&self) -> Option<&FilterNode> {
        let p = self.head.load(Ordering::Acquire);
        if p.is_null() || p == strict_ptr() { None } else { Some(unsafe { &*p }) }
    }

    /// Strong reference to the newest node (caller holds RUN_QUEUE).
    pub fn head_arc(&self) -> Option<Arc<FilterNode>> {
        let p = self.head.load(Ordering::Acquire);
        if p.is_null() || p == strict_ptr() { return None; }
        unsafe { Arc::increment_strong_count(p as *const FilterNode); Some(Arc::from_raw(p)) }
    }

    /// Replace the head with `node` (which must contain the old head in its
    /// chain). Caller holds RUN_QUEUE. The slot's old count is released only
    /// after the swap; the old node stays alive through `node.prev`.
    pub fn install(&self, node: Arc<FilterNode>) {
        let old = self.head.swap(Arc::into_raw(node) as *mut FilterNode, Ordering::AcqRel);
        Self::release(old);
    }

    /// Enter strict mode (caller holds RUN_QUEUE; only from mode 0).
    pub fn set_strict(&self) {
        let old = self.head.swap(strict_ptr(), Ordering::AcqRel);
        Self::release(old);
    }

    /// Copy `src`'s state into this (fresh, unpublished) slot. Called by
    /// clone/fork under the same RUN_QUEUE hold that inserts the child, so a
    /// concurrent TSYNC cannot slip between the copy and the insert.
    pub fn inherit_from(&self, src: &SeccompSlot) {
        let p = src.head.load(Ordering::Acquire);
        let np = if p.is_null() || p == strict_ptr() { p } else {
            unsafe { Arc::increment_strong_count(p as *const FilterNode); }
            p
        };
        let old = self.head.swap(np, Ordering::AcqRel);
        Self::release(old);
    }

    fn release(p: *mut FilterNode) {
        if !p.is_null() && p != strict_ptr() {
            unsafe { drop(Arc::from_raw(p as *const FilterNode)); }
        }
    }
}

impl Drop for SeccompSlot {
    fn drop(&mut self) { Self::release(*self.head.get_mut()); }
}

impl SeccompSlot {
    /// Run the whole chain (newest -> oldest) for one syscall; the lowest
    /// precedence verdict wins. `None` when no filter is installed.
    /// Lock-free: owning thread only.
    pub fn evaluate(&self, d: &SeccompData) -> Option<u32> {
        let h = self.head()?;
        let mut n = Some(h);
        let it = core::iter::from_fn(|| { let c = n?; n = c.prev.as_deref(); Some(&c.prog) });
        Some(seccomp_bpf::run_stack(it, d))
    }
}

// ── Install + syscall hook (lane sechook, S3) ──────────────────────────────

pub use seccomp_bpf::{
    check_filter, SECCOMP_RET_ACTION_FULL, SECCOMP_RET_ALLOW, SECCOMP_RET_DATA, SECCOMP_RET_ERRNO,
    SECCOMP_RET_KILL_PROCESS, SECCOMP_RET_KILL_THREAD, SECCOMP_RET_LOG, SECCOMP_RET_TRACE,
    SECCOMP_RET_TRAP, SECCOMP_RET_USER_NOTIF,
};

#[cfg(target_arch = "x86_64")]
pub const AUDIT_ARCH_NATIVE: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
pub const AUDIT_ARCH_NATIVE: u32 = 0xc000_00b7;

pub const FLAG_TSYNC: u32 = 1;
pub const FLAG_LOG: u32 = 2;
pub const FLAG_SPEC_ALLOW: u32 = 4;

const SIGKILL: u32 = 9;
const SIGSYS: u32 = 31;
const EPERM: isize = 1;
const EACCES: isize = 13;
const EFAULT: isize = 14;
const EINVAL: isize = 22;
const ENOSYS: isize = 38;

/// Mode of the calling thread (PR_GET_SECCOMP).
pub fn current_mode() -> u32 {
    let rq = crate::RUN_QUEUE.lock();
    rq.find_pid(crate::current_pid()).map_or(0, |t| t.seccomp.mode())
}

/// seccomp(SECCOMP_SET_MODE_STRICT) / prctl(PR_SET_SECCOMP, 1).
pub fn set_mode_strict() -> isize {
    let rq = crate::RUN_QUEUE.lock();
    match rq.find_pid(crate::current_pid()) {
        Some(t) => match t.seccomp.mode() {
            MODE_DISABLED => { t.seccomp.set_strict(); 0 }
            MODE_STRICT => 0,
            _ => -EINVAL,
        },
        None => -EINVAL,
    }
}

/// seccomp(SECCOMP_SET_MODE_FILTER, flags, fprog) / prctl(PR_SET_SECCOMP, 2,
/// fprog) with `flags = 0`. `fprog` is a user `struct sock_fprog`.
pub fn set_mode_filter(flags: u32, fprog: usize) -> isize {
    if flags & !(FLAG_LOG | FLAG_SPEC_ALLOW) != 0 { return -EINVAL; } // TSYNC: lane sectsync
    let mut hdr = [0u8; 16]; // { u16 len; pad; u64 filter }
    if unsafe { crate::uaccess::copy_from_user(hdr.as_mut_ptr(), fprog, 16) } != 0 { return -EFAULT; }
    let len = u16::from_ne_bytes([hdr[0], hdr[1]]) as usize;
    let ptr = u64::from_ne_bytes(hdr[8..16].try_into().unwrap()) as usize;
    if len == 0 || len > seccomp_bpf::BPF_MAXINSNS { return -EINVAL; }
    if !crate::no_new_privs() && !crate::caps::capable(21 /* CAP_SYS_ADMIN */) { return -EACCES; }
    let mut insns: alloc::vec::Vec<SockFilter> = alloc::vec::Vec::new();
    insns.resize(len, SockFilter { code: 0, jt: 0, jf: 0, k: 0 });
    if unsafe { crate::uaccess::copy_from_user(insns.as_mut_ptr() as *mut u8, ptr, len * 8) } != 0 {
        return -EFAULT;
    }
    let prog = match check_filter(&insns) { Ok(p) => p, Err(_) => return -EINVAL };
    let rq = crate::RUN_QUEUE.lock();
    let Some(t) = rq.find_pid(crate::current_pid()) else { return -EINVAL };
    if t.seccomp.mode() == MODE_STRICT { return -EINVAL; }
    match FilterNode::push(prog, t.seccomp.head_arc(), flags & FLAG_LOG != 0) {
        Some(n) => { t.seccomp.install(n); 0 }
        None => -12, // ENOMEM: path budget exceeded (Linux)
    }
}

/// seccomp(SECCOMP_GET_ACTION_AVAIL): 0 if the kernel knows the action.
pub fn action_avail(action: u32) -> isize {
    match action {
        SECCOMP_RET_KILL_PROCESS | SECCOMP_RET_KILL_THREAD | SECCOMP_RET_TRAP | SECCOMP_RET_ERRNO
        | SECCOMP_RET_TRACE | SECCOMP_RET_LOG | SECCOMP_RET_ALLOW => 0,
        _ => -95, // EOPNOTSUPP
    }
}

fn strict_allows(nr: usize) -> bool {
    #[cfg(target_arch = "aarch64")]
    { matches!(nr, 63 | 64 | 93 | 94 | 139) } // read write exit exit_group rt_sigreturn
    #[cfg(target_arch = "x86_64")]
    { matches!(nr, 0 | 1 | 60 | 231 | 15) }
}

/// Syscall-entry hook. `None` = run the syscall; `Some(ret)` = skip it and
/// return `ret`. Lock-free when the thread has no seccomp state.
#[inline]
pub fn on_syscall(nr: usize, args: [usize; 6], ip: u64) -> Option<isize> {
    let s = crate::current_seccomp()?;
    on_syscall_slow(s, nr, args, ip)
}

#[inline(never)]
fn on_syscall_slow(s: &SeccompSlot, nr: usize, args: [usize; 6], ip: u64) -> Option<isize> {
    if s.mode() == MODE_STRICT {
        if strict_allows(nr) { return None; }
        crate::exit_group_signal(SIGKILL);
    }
    let d = SeccompData {
        nr: nr as i32, arch: AUDIT_ARCH_NATIVE, instruction_pointer: ip,
        args: [args[0] as u64, args[1] as u64, args[2] as u64,
               args[3] as u64, args[4] as u64, args[5] as u64],
    };
    let ret = s.evaluate(&d)?;
    let data = ret & SECCOMP_RET_DATA;
    match ret & SECCOMP_RET_ACTION_FULL {
        SECCOMP_RET_ALLOW => None,
        SECCOMP_RET_LOG => { log_syscall(nr); None }
        SECCOMP_RET_ERRNO => Some(-(data.min(4095) as isize)),
        // No tracer / no listener attached: Linux answers ENOSYS.
        SECCOMP_RET_TRACE | SECCOMP_RET_USER_NOTIF => Some(-ENOSYS),
        // The syscall is not executed. SIGSYS is forced with Linux's siginfo;
        // the frame return register holds Linux's "rolled back" value (x86:
        // the syscall number, which chromium's handler checks against REG_RAX;
        // aarch64: the original x0) so the handler sees what it would on Linux.
        SECCOMP_RET_TRAP => {
            if crate::signal::force_sigsys_seccomp(nr as i32, AUDIT_ARCH_NATIVE, ip, data as u32) {
                #[cfg(target_arch = "x86_64")] { Some(nr as isize) }
                #[cfg(target_arch = "aarch64")] { Some(args[0] as isize) }
            } else {
                log_syscall(nr);
                crate::exit_group_signal(SIGSYS)
            }
        }
        // Kills only the calling thread; the last thread takes the process
        // down with SIGSYS.
        SECCOMP_RET_KILL_THREAD => {
            log_syscall(nr);
            if crate::thread_count(crate::current_tgid()) <= 1 {
                crate::exit_group_signal(SIGSYS)
            }
            crate::exit(128 + SIGSYS as i32)
        }
        _ => { log_syscall(nr); crate::exit_group_signal(SIGSYS) } // KILL_PROCESS + unknown
    }
}

fn log_syscall(nr: usize) {
    extern "C" { fn serial_print(s: *const u8, len: usize); }
    let m = alloc::format!("seccomp: pid {} syscall {}\n", crate::current_pid(), nr);
    unsafe { serial_print(m.as_ptr(), m.len()); }
}
