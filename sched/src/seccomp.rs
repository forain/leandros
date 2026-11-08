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
//! The cBPF checker/interpreter is NOT here: lane bpfcore provides it as
//! `lib/seccomp-bpf`; `SockFilter` below is the placeholder instruction type.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicPtr, Ordering};

/// One classic-BPF instruction (`struct sock_filter`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SockFilter { pub code: u16, pub jt: u8, pub jf: u8, pub k: u32 }

/// Linux MAX_INSNS_PER_PATH.
pub const MAX_INSNS_PER_PATH: u32 = 32768;

pub const MODE_DISABLED: u32 = 0;
pub const MODE_STRICT: u32 = 1;
pub const MODE_FILTER: u32 = 2;

pub struct FilterNode {
    /// Verified program.
    pub prog: Box<[SockFilter]>,
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
    pub fn push(prog: Box<[SockFilter]>, prev: Option<Arc<FilterNode>>, log: bool)
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
