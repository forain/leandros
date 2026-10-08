//! The state of one DRM sync object, binary or timeline, with no kernel
//! dependency: the DRM layer (`drm_device_interface`) owns the handle table,
//! the locking and the blocking; this module only decides *what* a payload is
//! and whether a wait on it is satisfied. Host-tested in drivers/uapi-tests,
//! which includes this file verbatim.
//!
//! THE MODEL. Linux keeps one `dma_fence *` per syncobj. For a binary syncobj
//! that is a plain fence (or NULL before anything was submitted); for a
//! timeline syncobj it is the head of a `dma_fence_chain`, one link per
//! point, and a link only counts as signalled once its own fence AND every
//! earlier link have signalled. [`Payload`] is that chain:
//!
//!   * `chain` holds the links in ascending point order. A binary payload is
//!     the one-link chain `[(0, fence)]`; an empty chain is the NULL fence.
//!   * `base` is the highest point whose prefix was found signalled and
//!     garbage-collected (`gc`), so a long-lived timeline does not grow
//!     without bound. Every point <= `base` is signalled by definition.
//!
//! Fences are opaque `u64` ids (0 is never a fence). Whether one has
//! signalled is asked through a `done` oracle the caller supplies; that is
//! what lets the virtio watermark and per-channel GPU fences share this code.
//! A [`Fence::All`] stands for "these fences, all of them" — what a timeline
//! point means once it is taken out of its chain (TRANSFER, or a wait handed
//! to a GPU submission), since a point is only reached when everything before
//! it is.

use alloc::vec::Vec;

/// An opaque fence id. 0 never names a fence.
pub type FenceId = u64;

/// What a payload link (or a looked-up point) is waiting on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fence {
    /// Signalled already: a CPU signal, or everything it stood for retired.
    Signaled,
    /// One fence.
    One(FenceId),
    /// Signalled once every listed fence has.
    All(Vec<FenceId>),
}

impl Fence {
    /// Has this fence signalled?
    pub fn is_done(&self, done: &dyn Fn(FenceId) -> bool) -> bool {
        match self {
            Fence::Signaled => true,
            Fence::One(f) => done(*f),
            Fence::All(v) => v.iter().all(|f| done(*f)),
        }
    }

    /// The fence ids still to wait for (empty when signalled).
    pub fn pending(&self, done: &dyn Fn(FenceId) -> bool) -> Vec<FenceId> {
        match self {
            Fence::Signaled => Vec::new(),
            Fence::One(f) => if done(*f) { Vec::new() } else { alloc::vec![*f] },
            Fence::All(v) => v.iter().copied().filter(|f| !done(*f)).collect(),
        }
    }

    /// Normalise a list of fences: empty is `Signaled`, one is `One`.
    pub fn from_ids(mut ids: Vec<FenceId>) -> Fence {
        ids.retain(|&f| f != 0);
        ids.sort_unstable();
        ids.dedup();
        match ids.len() {
            0 => Fence::Signaled,
            1 => Fence::One(ids[0]),
            _ => Fence::All(ids),
        }
    }

    /// The single fence id a binary consumer (v3d's in-sync) can hold: 0 when
    /// signalled, otherwise the highest still-pending id. Only exact for a
    /// `One`; an `All` (which v3d never produces) is approximated by its
    /// newest member.
    pub fn as_single(&self, done: &dyn Fn(FenceId) -> bool) -> FenceId {
        match self {
            Fence::Signaled => 0,
            Fence::One(f) => *f,
            Fence::All(v) => v.iter().copied().filter(|f| !done(*f)).max().unwrap_or(0),
        }
    }
}

/// One link of a timeline chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Point {
    pub point: u64,
    pub fence: Fence,
}

/// Result of looking up a point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// No fence for that point exists yet (upstream: `-EINVAL` unless the
    /// caller asked to wait for submission).
    NotSubmitted,
    /// The fence that signals when the point is reached.
    Ready(Fence),
}

/// What a WAIT sees for one handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitState {
    Signaled,
    Pending,
    /// The wait can never succeed as asked (NULL fence / unsubmitted point
    /// without WAIT_FOR_SUBMIT): upstream answers EINVAL.
    Invalid,
}

/// `drm_syncobj_wait.flags`.
pub const WAIT_ALL: u32 = 1 << 0;
pub const WAIT_FOR_SUBMIT: u32 = 1 << 1;
pub const WAIT_AVAILABLE: u32 = 1 << 2;
pub const WAIT_DEADLINE: u32 = 1 << 3;
/// `drm_syncobj_timeline_array.flags` for QUERY.
pub const QUERY_LAST_SUBMITTED: u32 = 1 << 0;

/// A sync object's payload; see the module comment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Payload {
    chain: Vec<Point>,
    base: u64,
}

impl Payload {
    /// The NULL fence (CREATE without SIGNALED).
    pub fn null() -> Payload {
        Payload { chain: Vec::new(), base: 0 }
    }

    /// A signalled stub (CREATE with SIGNALED).
    pub fn signaled() -> Payload {
        Payload { chain: alloc::vec![Point { point: 0, fence: Fence::Signaled }], base: 0 }
    }

    pub fn links(&self) -> &[Point] {
        &self.chain
    }

    pub fn base(&self) -> u64 {
        self.base
    }

    /// True for the NULL fence.
    pub fn is_null(&self) -> bool {
        self.chain.is_empty() && self.base == 0
    }

    /// Binary replace (`drm_syncobj_replace_fence`): SIGNAL installs
    /// `Fence::Signaled`, a submission installs its fence.
    pub fn replace(&mut self, fence: Fence) {
        self.chain.clear();
        self.base = 0;
        self.chain.push(Point { point: 0, fence });
    }

    /// RESET: back to the NULL fence.
    pub fn reset(&mut self) {
        self.chain.clear();
        self.base = 0;
    }

    /// The head fence (what a binary wait or a point-0 lookup sees): the
    /// newest link, `Signaled` when only collected history is left, `None`
    /// for the NULL fence.
    pub fn head(&self) -> Option<Fence> {
        match self.chain.last() {
            Some(p) => Some(p.fence.clone()),
            None if self.base > 0 => Some(Fence::Signaled),
            None => None,
        }
    }

    /// `drm_syncobj_add_point`: append point `point` carrying `fence`. Points
    /// normally arrive in increasing order; an out-of-order one is inserted
    /// in order (upstream warns and chains it anyway), and a point that is
    /// already present takes the new fence.
    pub fn add_point(&mut self, point: u64, fence: Fence) {
        match self.chain.binary_search_by(|p| p.point.cmp(&point)) {
            Ok(i) => self.chain[i].fence = fence,
            Err(i) => self.chain.insert(i, Point { point, fence }),
        }
    }

    /// The highest point submitted so far (QUERY with LAST_SUBMITTED).
    pub fn last_point(&self) -> u64 {
        self.chain.last().map_or(self.base, |p| p.point.max(self.base))
    }

    /// The highest point reached (QUERY): every link up to it signalled.
    /// A binary payload answers 0, as upstream does for a non-chain fence.
    pub fn query(&self, done: &dyn Fn(FenceId) -> bool) -> u64 {
        let mut v = self.base;
        for p in &self.chain {
            if !p.fence.is_done(done) {
                break;
            }
            v = v.max(p.point);
        }
        v
    }

    /// The fence that signals when `point` is reached. Point 0 is the binary
    /// view: the head fence.
    pub fn lookup(&self, point: u64, done: &dyn Fn(FenceId) -> bool) -> Lookup {
        if point == 0 {
            return match self.head() {
                Some(f) => Lookup::Ready(f),
                None => Lookup::NotSubmitted,
            };
        }
        if point <= self.base {
            return Lookup::Ready(Fence::Signaled);
        }
        let Some(i) = self.chain.iter().position(|p| p.point >= point) else {
            return Lookup::NotSubmitted;
        };
        let mut ids = Vec::new();
        for p in &self.chain[..=i] {
            ids.extend(p.fence.pending(done));
        }
        Lookup::Ready(Fence::from_ids(ids))
    }

    /// One handle's state for a WAIT / TIMELINE_WAIT at `point`.
    pub fn wait_state(&self, point: u64, flags: u32, done: &dyn Fn(FenceId) -> bool) -> WaitState {
        match self.lookup(point, done) {
            Lookup::NotSubmitted => {
                if flags & (WAIT_FOR_SUBMIT | WAIT_AVAILABLE) != 0 {
                    WaitState::Pending
                } else {
                    WaitState::Invalid
                }
            }
            Lookup::Ready(_) if flags & WAIT_AVAILABLE != 0 => WaitState::Signaled,
            Lookup::Ready(f) => {
                if f.is_done(done) { WaitState::Signaled } else { WaitState::Pending }
            }
        }
    }

    /// Drop the signalled prefix of a timeline (never a point-0 link, which
    /// is a binary payload and must keep reporting its fence).
    pub fn gc(&mut self, done: &dyn Fn(FenceId) -> bool) {
        let mut n = 0;
        while n < self.chain.len() && self.chain[n].point > 0 && self.chain[n].fence.is_done(done) {
            self.base = self.base.max(self.chain[n].point);
            n += 1;
        }
        if n > 0 {
            self.chain.drain(..n);
        }
    }

    /// Number of links (for bounding a timeline that is never waited on).
    pub fn len(&self) -> usize {
        self.chain.len()
    }
}
