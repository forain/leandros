//! Sync files and dma-buf implicit fences — the kernel half (the pure
//! semantics are `sync_file_core.rs`).
//!
//! WHY. Zink (OpenGL on Vulkan) is how cosmic-comp and Firefox render on the
//! NVIDIA card (NVK has no GL of its own). Everything a GL client shares with
//! the compositor goes through two Linux mechanisms this layer did not have:
//!
//!   * **sync files from syncobjs** — `vkGetSemaphoreFdKHR(SYNC_FD)` and
//!     `vkImportSemaphoreFdKHR(SYNC_FD)` on NVK are
//!     DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD / FD_TO_HANDLE with the
//!     EXPORT/IMPORT_SYNC_FILE flags. Zink uses them for every exportable
//!     dma-buf it submits against and for EGL_ANDROID_native_fence_sync
//!     (smithay's per-frame render fence);
//!   * **implicit sync on dma-bufs** — zink imports its submission's sync
//!     file into each shared image (DMA_BUF_IOCTL_IMPORT_SYNC_FILE) and
//!     exports the image's fences before it reads one somebody else wrote
//!     (DMA_BUF_IOCTL_EXPORT_SYNC_FILE). That is how a client's frame and the
//!     compositor's sampling of it are ordered (`zink_batch.c`,
//!     `zink_synchronization.cpp`).
//!
//! THE FD. A sync file is an eventfd (the same object the virtgpu out-fence
//! already is): counter 0 until the fence signals, then 1 forever, so
//! poll(POLLIN) means exactly what it means for a Linux sync file. The VFS
//! records a **token** for it (`EvMeta.fence` with origin SYNC_FILE); this
//! module maps the token to its fence for as long as the fence is pending.
//! A token with no entry is a signalled sync file — a fence never
//! un-signals, so dropping an entry once it has signalled loses nothing and
//! needs no close hook from the VFS.
//!
//! WHO SIGNALS. `tick` (from `drm_tick`, 100 Hz, try-locks only) walks the
//! pending entries and signals each eventfd whose fence is done, through the
//! same slot-reference protocol as the out-fence (`arm` is handed a slot
//! reference, the signaller drops it). Only poll() waiters depend on this;
//! a GPU wait on an imported sync file goes through the syncobj and the
//! backend's in-fence path, not through the eventfd.
//!
//! THE GATE. Everything here answers ENOSYS (the historic answer) unless a
//! GPU UAPI backend is registered — today the NVIDIA nouveau layer — or the
//! kernel command line says `drm.syncfile=1` (a test switch for QEMU, where
//! drmsmoke exercises the ioctls on virtio-gpu). The virtio/Venus/virgl
//! desktop paths therefore see exactly the old behaviour: Mesa's WSI probes
//! these ioctls and would otherwise change its synchronisation strategy.
//!
//! LOCKS. `PENDING` and `RESV` are leaves, never held across a user-memory
//! access, a block or any other lock.

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use spin::Mutex;

use crate::syncobj_core::Fence;
use crate::sync_file_core::{self, Resv};

fn done(f: u64) -> bool {
    crate::drm_device_interface::fence_is_done(f)
}

// ── the gate ─────────────────────────────────────────────────────────────────

/// 0 = not read yet, 1 = off, 2 = on (cmdline `drm.syncfile=1`).
static FORCE: AtomicU8 = AtomicU8::new(0);

/// Are sync files and dma-buf implicit fences answered?
pub fn enabled() -> bool {
    if crate::gpu_uapi::active() {
        return true;
    }
    // The v3d backend is a native-UAPI driver like the gpu_uapi ones: Mesa's
    // v3d driver creates every pipe fence with
    // `drmSyncobjExportSyncFile(out_sync)` (v3d_fence.c) and treats a failure
    // as "no fence", so the first eglSwapBuffers has no front buffer and
    // kmscube dies with "Failed to get a new framebuffer BO". Only while
    // armed; the virtio paths keep the old answer.
    if crate::drm_device_interface::v3d_active() {
        return true;
    }
    let mut f = FORCE.load(Ordering::Relaxed);
    if f == 0 {
        f = if crate::nvidia_probe::cmdline_opt("drm.syncfile") == Some(1) { 2 } else { 1 };
        FORCE.store(f, Ordering::Relaxed);
    }
    f == 2
}

// ── sync files ───────────────────────────────────────────────────────────────

struct Pending {
    fence: Fence,
    /// The eventfd slot to signal (one slot reference is ours).
    slot: u64,
}

static PENDING: Mutex<BTreeMap<u64, Pending>> = Mutex::new(BTreeMap::new());
static PENDING_N: AtomicUsize = AtomicUsize::new(0);
/// Tokens are never reused: a stale token can only ever mean "signalled".
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
/// Bound on sync files whose fence has not signalled yet. Reaching it means
/// the GPU stopped retiring work; `arm` then refuses and the caller hands out
/// an already-signalled fd (the GPU-side wait still happens through the
/// syncobj, only poll() loses its accuracy).
const MAX_PENDING: usize = 4096;
static SIGNALLER: AtomicUsize = AtomicUsize::new(0);

/// Install the eventfd signaller (`fn(slot) -> bool`, try-locking, drops the
/// slot reference on success). Kernel, once, before the first `arm`.
pub fn set_signaller(f: fn(u64) -> bool) {
    SIGNALLER.store(f as usize, Ordering::Release);
}

/// Sync files whose fence signalled WITH AN ERROR (a watchdog write-off), by
/// token. The pending entry disappears at signalling, so the error is kept
/// here; tokens are never reused. Bounded ring, oldest overwritten; 0 = empty.
const ERR_TOKENS_N: usize = 256;
static ERR_TOKENS: Mutex<[u64; ERR_TOKENS_N]> = Mutex::new([0; ERR_TOKENS_N]);
static ERR_NEXT: AtomicUsize = AtomicUsize::new(0);

/// Does `f` name a fence that was written off with an error?
pub fn fence_has_error(f: &Fence) -> bool {
    f.pending(&|_| false).iter().any(|&id| crate::virtio_gpu::fence_errored(id))
}

/// Record that sync file `token` holds an errored fence.
pub fn mark_errored(token: u64) {
    let mut t = ERR_TOKENS.lock();
    if t.iter().any(|&x| x == token) { return; }
    let i = ERR_NEXT.fetch_add(1, Ordering::Relaxed) % ERR_TOKENS_N;
    t[i] = token;
}

/// Did sync file `token`'s fence signal with an error (Linux: fence status < 0)?
pub fn token_errored(token: u64) -> bool {
    if ERR_TOKENS.lock().iter().any(|&x| x == token) { return true; }
    let t = PENDING.lock();
    t.get(&token).map_or(false, |p| fence_has_error(&p.fence) && p.fence.is_done(&done))
}

pub fn new_token() -> u64 {
    NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
}

/// Track `token`'s fence and signal eventfd `slot` when it is done. False if
/// the fence is already done (or the table is full): the caller signals the
/// eventfd itself and keeps its slot reference to drop.
pub fn arm(token: u64, fence: Fence, slot: u64) -> bool {
    let fence = Fence::from_ids(fence.pending(&done));
    if fence == Fence::Signaled {
        return false;
    }
    let mut t = PENDING.lock();
    if t.len() >= MAX_PENDING {
        return false;
    }
    t.insert(token, Pending { fence, slot });
    PENDING_N.store(t.len(), Ordering::Relaxed);
    true
}

/// The fence sync file `token` holds now (`Signaled` once it has signalled).
pub fn fence_of(token: u64) -> Fence {
    let t = PENDING.lock();
    match t.get(&token) {
        Some(p) => Fence::from_ids(p.fence.pending(&done)),
        None => Fence::Signaled,
    }
}

/// SYNC_IOC_MERGE.
pub fn merge(a: &Fence, b: &Fence) -> Fence {
    sync_file_core::merge(a, b, &done)
}

pub fn is_signaled(f: &Fence) -> bool {
    f.is_done(&done)
}

/// Pending sync files (diagnostics).
pub fn pending_count() -> usize {
    PENDING_N.load(Ordering::Relaxed)
}

/// Signal every sync file whose fence is done. Tick context: try-locks only,
/// one relaxed load when nothing is pending.
pub fn tick() {
    if PENDING_N.load(Ordering::Relaxed) == 0 {
        return;
    }
    let p = SIGNALLER.load(Ordering::Acquire);
    if p == 0 {
        return;
    }
    // SAFETY: only ever written by `set_signaller` from a `fn(u64) -> bool`.
    let signal: fn(u64) -> bool = unsafe { core::mem::transmute::<usize, fn(u64) -> bool>(p) };
    let mut t = match PENDING.try_lock() {
        Some(g) => g,
        None => return,
    };
    t.retain(|tok, e| {
        if !e.fence.is_done(&done) { return true; }
        // Keep the error of a written-off fence past the entry's removal.
        if fence_has_error(&e.fence) { if let Some(mut et) = ERR_TOKENS.try_lock() {
            if !et.iter().any(|&x| x == *tok) {
                let i = ERR_NEXT.fetch_add(1, Ordering::Relaxed) % ERR_TOKENS_N;
                et[i] = *tok;
            }
        } else { return true; } }
        !signal(e.slot)
    });
    PENDING_N.store(t.len(), Ordering::Relaxed);
}

// ── dma-buf reservation objects ──────────────────────────────────────────────

/// BO object id (the dmabuf's lifetime identity, `dmabuf_obj`) -> its
/// unsignalled implicit fences. Object ids are not reused while a dmabuf fd
/// holds the object, and an entry only lives while it has a pending fence.
static RESV: Mutex<BTreeMap<u32, Resv>> = Mutex::new(BTreeMap::new());
/// Past this many entries every access sweeps the whole map.
const RESV_SWEEP: usize = 256;

/// DMA_BUF_IOCTL_IMPORT_SYNC_FILE on the dmabuf of object `obj`.
pub fn resv_import(obj: u32, fence: &Fence, flags: u32) {
    if fence.is_done(&done) {
        return;
    }
    let mut m = RESV.lock();
    m.entry(obj).or_insert_with(Resv::new).add(fence, flags, &done);
    if m.len() > RESV_SWEEP {
        m.retain(|_, r| {
            r.gc(&done);
            !r.is_empty()
        });
    }
}

/// DMA_BUF_IOCTL_EXPORT_SYNC_FILE on the dmabuf of object `obj`.
pub fn resv_export(obj: u32, flags: u32) -> Fence {
    let mut m = RESV.lock();
    let f = match m.get_mut(&obj) {
        Some(r) => {
            r.gc(&done);
            let f = r.export(flags, &done);
            if r.is_empty() {
                m.remove(&obj);
            }
            f
        }
        None => Fence::Signaled,
    };
    f
}

/// Is the dmabuf of `obj` free of fences a user of kind `flags` must wait
/// for (dma-buf poll semantics)?
pub fn resv_idle(obj: u32, flags: u32) -> bool {
    resv_export(obj, flags) == Fence::Signaled
}
