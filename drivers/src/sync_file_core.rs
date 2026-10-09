//! Pure logic of sync files and dma-buf implicit fences, with no kernel
//! dependency (host-tested in drivers/uapi-tests, which includes this file
//! verbatim). The kernel side — the fds, the tables, the locking and the
//! signalling — is `sync_file.rs`.
//!
//! THE LINUX MODEL, reduced to what userspace can observe:
//!
//!   * A **sync file** is an fd holding one fence (a `dma_fence`, possibly a
//!     `dma_fence_array` after SYNC_IOC_MERGE). It is readable (POLLIN) once
//!     the fence has signalled and never becomes unsignalled again. Here the
//!     fence is a `syncobj_core::Fence` — `All` is the array.
//!   * A **dma-buf** carries a reservation object (`dma_resv`): fences tagged
//!     with a usage. `DMA_BUF_IOCTL_IMPORT_SYNC_FILE` adds a sync file's fence
//!     with usage WRITE (flag DMA_BUF_SYNC_WRITE set) or READ (only
//!     DMA_BUF_SYNC_READ); it never removes an existing fence.
//!     `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` returns a sync file for every fence a
//!     user of that kind must wait for: a reader (flags == READ) waits for the
//!     writers, a writer (WRITE set) waits for everybody. With nothing to wait
//!     for it is an already-signalled stub — never an error.
//!
//! [`Resv`] is that reservation object. It only keeps fences that have not
//! signalled yet (`gc` drops the rest), which is what bounds it.

use alloc::vec::Vec;

use crate::syncobj_core::{Fence, FenceId};

/// `struct dma_buf_sync` / `dma_buf_export_sync_file` flag bits.
pub const DMA_BUF_SYNC_READ: u32 = 1 << 0;
pub const DMA_BUF_SYNC_WRITE: u32 = 2 << 0;
pub const DMA_BUF_SYNC_RW: u32 = DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE;

/// Upstream's flag check for both sync-file ioctls on a dma-buf: only R/W
/// bits, and at least one of them (`dma_buf_export_sync_file` /
/// `dma_buf_import_sync_file` answer EINVAL otherwise).
pub fn resv_flags_valid(flags: u32) -> bool {
    flags & !DMA_BUF_SYNC_RW == 0 && flags & DMA_BUF_SYNC_RW != 0
}

/// A dma-buf's reservation object: the unsignalled fences of its writers
/// and readers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resv {
    pub write: Vec<FenceId>,
    pub read: Vec<FenceId>,
}

impl Resv {
    pub fn new() -> Resv {
        Resv { write: Vec::new(), read: Vec::new() }
    }

    /// IMPORT_SYNC_FILE: add `fence` with the usage `flags` select (WRITE
    /// bit set -> a writer, else a reader).
    pub fn add(&mut self, fence: &Fence, flags: u32, done: &dyn Fn(FenceId) -> bool) {
        let ids = fence.pending(done);
        let v = if flags & DMA_BUF_SYNC_WRITE != 0 { &mut self.write } else { &mut self.read };
        for f in ids {
            if !v.contains(&f) {
                v.push(f);
            }
        }
    }

    /// EXPORT_SYNC_FILE: what a user of kind `flags` must wait for. A reader
    /// (READ only) waits for the writers; a writer waits for every fence.
    pub fn export(&self, flags: u32, done: &dyn Fn(FenceId) -> bool) -> Fence {
        let mut ids: Vec<FenceId> = self.write.iter().copied().filter(|&f| !done(f)).collect();
        if flags & DMA_BUF_SYNC_WRITE != 0 {
            ids.extend(self.read.iter().copied().filter(|&f| !done(f)));
        }
        Fence::from_ids(ids)
    }

    /// Drop every signalled fence.
    pub fn gc(&mut self, done: &dyn Fn(FenceId) -> bool) {
        self.write.retain(|&f| !done(f));
        self.read.retain(|&f| !done(f));
    }

    pub fn is_empty(&self) -> bool {
        self.write.is_empty() && self.read.is_empty()
    }
}

/// SYNC_IOC_MERGE: a fence that signals once both have (`dma_fence_array`
/// of the two, flattened and de-duplicated; signalled members dropped).
pub fn merge(a: &Fence, b: &Fence, done: &dyn Fn(FenceId) -> bool) -> Fence {
    let mut ids = a.pending(done);
    ids.extend(b.pending(done));
    Fence::from_ids(ids)
}

/// The fence a syncobj exports as a sync file (DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD
/// with EXPORT_SYNC_FILE): every pending fence of every link, which for a
/// binary payload is its one fence and for a timeline is "the last point
/// reached" (upstream exports the chain head). `None` for the NULL fence —
/// upstream answers EINVAL.
pub fn syncobj_export(links: &[crate::syncobj_core::Point], base: u64,
                      done: &dyn Fn(FenceId) -> bool) -> Option<Fence> {
    if links.is_empty() && base == 0 {
        return None;
    }
    let mut ids = Vec::new();
    for p in links {
        ids.extend(p.fence.pending(done));
    }
    Some(Fence::from_ids(ids))
}
