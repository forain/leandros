//! G3: the i915 DRM UAPI subset Mesa 25.3 iris (and ANV after it) drive a
//! Gen9 GPU with, answered on /dev/dri/card0 and renderD128 through the
//! `gpu_uapi` seam — the same seam the nouveau layer for NVK uses. Once
//! registered, DRM_IOCTL_VERSION says `i915` 1.6.0 and the nodes' sysfs PCI
//! identity is 8086:<device>, which is what makes Mesa's loader pick iris.
//!
//! THE MODEL (Linux i915 with full 48-bit PPGTT, execlists, softpin only):
//!
//! * GEM objects are physically contiguous, zeroed, cached system memory
//!   (one buddy block each). The mmap token MMAP_OFFSET hands out is the
//!   physical base, as for the NVK layer's sysmem buffers; every CPU mapping
//!   is write-back, which is coherent on these LLC parts.
//! * Every open has a default context 0 with its own VM. VM_CREATE makes
//!   more; CONTEXT_GETPARAM(VM) names a context's VM with a new id (iris'
//!   "global VM"); contexts created with SETPARAM(VM) share it. A VM is one
//!   4-level `hw::Ppgtt` plus `vmbind::VmMap` (which object is where).
//! * EXECBUFFER2 accepts softpinned objects only (iris and ANV pin all).
//!   Each object is bound at the address it names (moved/evicted with the
//!   i915 rules), in-fences of the fence array / timeline extension are
//!   waited for on the CPU, the batch runs, out-syncobjs get the request's
//!   fence: `EXT_FENCE | engine << 48 | seqno`.
//! * The engine is `Mode::Fake` (`i915.fake`, QEMU): `softcs` walks the
//!   batch through the real page tables and performs its memory writes,
//!   synchronously. `Mode::Hw` (`i915.uapi=1` on the laptop) registers the
//!   node and answers everything except execution, which is not wired to
//!   the execlist core yet and answers ENODEV (iris: device lost) — no
//!   user batch reaches the hardware from this layer.
//!
//! Logging: `i915.uapi_log` 0 silent, 1 (default) errors and lifecycle,
//! 2 every ioctl (klog only), 3 every ioctl on the console too.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use sched::ksync::KMutex;

use crate::gpu_uapi::{UapiBackend, UapiPrime, EXT_FENCE};
use crate::DriverError;

use super::gt::KStore;
use super::hw::{self, Cache, Ppgtt, PtStore, Sseu};
use super::uapi_abi::*;
use super::vmbind::VmMap;
use super::{ilog, softcs};

const E2BIG: i32 = 7;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    /// Software command streamer (QEMU testing).
    Fake,
    /// Real hardware: execution not wired yet (ENODEV).
    Hw,
}

/// Engines this layer can expose: (uapi class, instance) -> our index, the
/// same order as `exec`'s engines.
const ENGINES: [(u16, u16); 3] = [(0, 0), (1, 0), (2, 0)]; // rcs0, bcs0, vcs0
const NENG: usize = ENGINES.len();
/// `ENGINES` index of vcs0: exposed only when its bring-up passed.
const VCS0: usize = 2;
/// Bitmask over `ENGINES` of the engines userspace sees (ENGINE_INFO,
/// HAS_BSD/HAS_BLT, legacy rings, engine maps). Set once by `register`.
static EXPOSED: AtomicU32 = AtomicU32::new(0b011);

fn exposed() -> Vec<(u16, u16)> {
    let m = EXPOSED.load(Ordering::Relaxed);
    ENGINES.iter().enumerate().filter(|&(i, _)| m & 1 << i != 0).map(|(_, &e)| e).collect()
}

/// Our engine index for an exposed (class, instance).
fn engine_index(e: (u16, u16)) -> Option<usize> {
    let i = ENGINES.iter().position(|&x| x == e)?;
    if EXPOSED.load(Ordering::Relaxed) & 1 << i != 0 { Some(i) } else { None }
}

struct Info {
    facts: Facts,
    mode: Mode,
    mmio: usize,
    slot: (u8, u8, u8),
    subsys: (u16, u16),
    revision: u8,
}

static INFO: spin::Mutex<Option<Info>> = spin::Mutex::new(None);
static MODE_FAKE: AtomicU32 = AtomicU32::new(0);
static LOG: AtomicU32 = AtomicU32::new(1);
static IOCTLS: AtomicU64 = AtomicU64::new(0);
static ERRORS: AtomicU64 = AtomicU64::new(0);
static EXECS: AtomicU64 = AtomicU64::new(0);
static RESETS: AtomicU32 = AtomicU32::new(0);
/// Execbuffers that waited for another engine's request (implicit sync).
static CROSS_WAITS: AtomicU64 = AtomicU64::new(0);
/// EXECBUFFER2 relocation entries written / skipped (presumed offset right).
static RELOCS_PATCHED: AtomicU64 = AtomicU64::new(0);
static RELOCS_SKIPPED: AtomicU64 = AtomicU64::new(0);
/// Unpinned exec objects placed (or moved) by the kernel.
static PLACED: AtomicU64 = AtomicU64::new(0);

// ── the uapi error ring: the last failing ioctls, always on ────────────────
//
// `errors` alone does not say what failed, and uapi_log=2 needs a cmdline
// change on the laptop. Every failing ioctl leaves (nr, errno, pid, a short
// static reason, an argument) here; /proc/i915 prints the ring. Reasons are
// noted by the failing code with `why()` into a per-pid slot that the
// dispatcher clears on entry and reads on failure: no allocation, two spin
// locks per *failing* ioctl only (the entry clear is one store under a lock).

const ERR_RING: usize = 8;

#[derive(Clone, Copy)]
struct ErrRec { seq: u64, nr: u32, errno: i32, pid: u32, why: &'static str, arg: u64 }

static ERR_LOG: spin::Mutex<[Option<ErrRec>; ERR_RING]> = spin::Mutex::new([None; ERR_RING]);
/// (pid, reason, arg) noted by the current failing path, hashed by pid.
static WHY: spin::Mutex<[(u32, &'static str, u64); ERR_RING]> = spin::Mutex::new([(0, "", 0); ERR_RING]);

/// Note why the current ioctl fails; returns `e` for `Err(why(...))`.
fn why(e: i32, reason: &'static str, arg: u64) -> i32 {
    let pid = sched::current_pid();
    WHY.lock()[pid as usize % ERR_RING] = (pid, reason, arg);
    e
}

/// `Why` (errno, reason) from the pure checks -> errno, noted.
fn whyw(w: Why, arg: u64) -> i32 { why(w.0, w.1, arg) }

fn why_clear() {
    let pid = sched::current_pid();
    let mut w = WHY.lock();
    if w[pid as usize % ERR_RING].0 == pid { w[pid as usize % ERR_RING] = (0, "", 0); }
}

fn err_record(seq: u64, nr: u32, e: i32, fallback_arg: u64) {
    let pid = sched::current_pid();
    let (reason, arg) = {
        let mut w = WHY.lock();
        let s = &mut w[pid as usize % ERR_RING];
        let r = if s.0 == pid && !s.1.is_empty() { (s.1, s.2) } else { ("", fallback_arg) };
        *s = (0, "", 0);
        r
    };
    ERR_LOG.lock()[seq as usize % ERR_RING] = Some(ErrRec { seq, nr, errno: e, pid, why: reason, arg });
}

/// The ring, oldest first, as `/proc/i915` lines.
/// False (nothing written) when the uapi layer is not registered.
pub(super) fn proc_errors(s: &mut alloc::string::String) -> bool {
    use core::fmt::Write;
    if INFO.lock().is_none() { return false; }
    let _ = writeln!(s, "uapi_ioctls {}", IOCTLS.load(Ordering::Relaxed));
    let _ = writeln!(s, "uapi_errors {}", ERRORS.load(Ordering::Relaxed));
    let _ = writeln!(s, "uapi_execs {}", EXECS.load(Ordering::Relaxed));
    let _ = writeln!(s, "uapi_relocs_patched {}", RELOCS_PATCHED.load(Ordering::Relaxed));
    let _ = writeln!(s, "uapi_relocs_skipped {}", RELOCS_SKIPPED.load(Ordering::Relaxed));
    let _ = writeln!(s, "uapi_placed {}", PLACED.load(Ordering::Relaxed));
    let mut recs: Vec<ErrRec> = ERR_LOG.lock().iter().flatten().copied().collect();
    recs.sort_unstable_by_key(|r| r.seq);
    for r in recs {
        let _ = writeln!(s, "uapi_err #{} ioctl {:#x} {} -{} pid {} {} arg {:#x}", r.seq, r.nr, ioctl_name(r.nr), r.errno,
                         r.pid, if r.why.is_empty() { "-" } else { r.why }, r.arg);
    }
    true
}

fn ioctl_name(nr: u32) -> &'static str {
    match nr {
        NR_GETPARAM => "GETPARAM", NR_SETPARAM => "SETPARAM", NR_QUERY => "QUERY",
        NR_GEM_CREATE => "GEM_CREATE", NR_GEM_CREATE_EXT => "GEM_CREATE_EXT",
        NR_GEM_MMAP_OFFSET => "MMAP_OFFSET", NR_GEM_MMAP => "GEM_MMAP", NR_GEM_PWRITE => "PWRITE",
        NR_GEM_PREAD => "PREAD", NR_GEM_SET_DOMAIN => "SET_DOMAIN", NR_GEM_SET_CACHING => "SET_CACHING",
        NR_GEM_GET_CACHING => "GET_CACHING", NR_GEM_SET_TILING => "SET_TILING", NR_GEM_GET_TILING => "GET_TILING",
        NR_GEM_MADVISE => "MADVISE", NR_GEM_BUSY => "BUSY", NR_GEM_WAIT => "WAIT", NR_GEM_USERPTR => "USERPTR",
        NR_GEM_GET_APERTURE => "GET_APERTURE", NR_GEM_CONTEXT_CREATE => "CONTEXT_CREATE",
        NR_GEM_CONTEXT_DESTROY => "CONTEXT_DESTROY", NR_GEM_CONTEXT_GETPARAM => "CONTEXT_GETPARAM",
        NR_GEM_CONTEXT_SETPARAM => "CONTEXT_SETPARAM", NR_GEM_VM_CREATE => "VM_CREATE", NR_GEM_VM_DESTROY => "VM_DESTROY",
        NR_GEM_EXECBUFFER2 => "EXECBUFFER2", NR_REG_READ => "REG_READ", NR_GET_RESET_STATS => "GET_RESET_STATS",
        _ => "?",
    }
}

// ── i915.ptecheck=1: verify every exec object's PPGTT PTEs ───────────────────
//
// At every EXECBUFFER2, after binding, each exec object's [offset, offset +
// size) is walked in the context VM's page tables and every PTE compared with
// what the binding must be (present | RW | object phys + i * 4 KiB | the PAT
// bits of its cache level). Also notes every PRIME FD_TO_HANDLE (does the
// handle the importer got name the dma-buf's object?) and every dma-buf mmap
// (are the frames the object's own pages?). Mismatches are logged rate-limited,
// and a per-second summary (`ptecheck: N execs, M objects, P pages checked, X
// bad`) shows the checker ran. Runs in fake mode too: a regression check.
static PTECHECK: AtomicU32 = AtomicU32::new(0);
static PTC_EXECS: AtomicU64 = AtomicU64::new(0);
static PTC_OBJS: AtomicU64 = AtomicU64::new(0);
static PTC_PAGES: AtomicU64 = AtomicU64::new(0);
static PTC_BAD: AtomicU64 = AtomicU64::new(0);
static PTC_NOTES: AtomicU64 = AtomicU64::new(0);
static PTC_LAST_NS: AtomicU64 = AtomicU64::new(0);
/// Mismatch lines logged (cap, then only the summary counts).
static PTC_LOGGED: AtomicU32 = AtomicU32::new(0);
const PTC_LOG_MAX: u32 = 64;
const PTC_NOTE_MAX: u64 = 400;

fn ptecheck_on() -> bool { PTECHECK.load(Ordering::Relaxed) != 0 }

/// Rate limit for mismatch lines: always the first PTC_LOG_MAX.
fn ptc_may_log() -> bool { PTC_LOGGED.fetch_add(1, Ordering::Relaxed) < PTC_LOG_MAX }

fn ptc_summary() {
    let now = sched::monotonic_ns();
    let last = PTC_LAST_NS.load(Ordering::Relaxed);
    if now.wrapping_sub(last) < 1_000_000_000 { return; }
    if PTC_LAST_NS.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() { return; }
    ilog!("ptecheck: {} execs, {} objects, {} pages checked, {} bad PTEs, {} notes (cumulative)",
          PTC_EXECS.load(Ordering::Relaxed), PTC_OBJS.load(Ordering::Relaxed), PTC_PAGES.load(Ordering::Relaxed),
          PTC_BAD.load(Ordering::Relaxed), PTC_NOTES.load(Ordering::Relaxed));
}

/// Walk the PTEs of every exec object (state lock held).
fn ptecheck_exec(st: &State, open_id: u32, vm_key: u32, objs: &[ExecObject2], list: &[(u32, u64, u64)]) {
    let Some(vm) = st.vms.get(&vm_key) else { return };
    PTC_EXECS.fetch_add(1, Ordering::Relaxed);
    for (i, &(obj, off, size)) in list.iter().enumerate() {
        let Some(b) = st.bos.get(&obj) else { continue };
        let cache = if b.scanout_uc { Cache::Uncached } else { cached() };
        PTC_OBJS.fetch_add(1, Ordering::Relaxed);
        let pages = size / 4096;
        PTC_PAGES.fetch_add(pages, Ordering::Relaxed);
        let mut bad = 0u64;
        let mut first: Option<(u64, u64, u64)> = None;
        for p in 0..pages {
            let want = hw::ppgtt_pte(b.phys + p * 4096, cache, false);
            let got = vm.pt.lookup(&KStore, off + p * 4096);
            if got != want {
                bad += 1;
                if first.is_none() { first = Some((p, got, want)); }
            }
        }
        if bad == 0 { continue; }
        PTC_BAD.fetch_add(bad, Ordering::Relaxed);
        if let Some((p, got, want)) = first {
            if ptc_may_log() {
                let rec = vm.map.covering(off);
                ilog!("ptecheck: MISMATCH open {} vm {} handle {:#x} obj {:#x} off {:#x} size {:#x} phys {:#x}: {}/{} pages bad, \
                       first page {} pte {:#x} want {:#x} (map at off: {:?}; obj recorded at {:?})",
                      open_id, vm_key, objs[i].handle, obj, off, size, b.phys, bad, pages, p, got, want, rec, vm.map.find(obj));
            }
        }
    }
}

/// PRIME FD_TO_HANDLE result (kernel syscall layer, `i915.ptecheck=1`):
/// `obj` is what the dma-buf fd holds, `handle` what the importer got.
pub fn ptecheck_fd_to_handle(fd: i32, open_id: u32, obj: Option<u32>, handle: u32) {
    if !ptecheck_on() { return; }
    let st = STATE.lock();
    let mapped = st.opens.get(&open_id).and_then(|o| o.handles.get(&handle).copied());
    let ok = obj.is_some() && mapped == obj;
    let n = PTC_NOTES.fetch_add(1, Ordering::Relaxed);
    let bo = obj.and_then(|o| st.bos.get(&o)).map(|b| (b.phys, b.size, b.handles, b.exports));
    drop(st);
    if !ok { PTC_BAD.fetch_add(1, Ordering::Relaxed); }
    if n < PTC_NOTE_MAX || (!ok && ptc_may_log()) {
        ilog!("ptecheck: FD_TO_HANDLE open {} fd {} -> obj {:?} handle {:#x} (handle names obj {:?}){} bo(phys,size,handles,exports) {:?}",
              open_id, fd, obj.map(|o| alloc::format!("{:#x}", o)), handle, mapped.map(|o| alloc::format!("{:#x}", o)),
              if ok { "" } else { " MISMATCH" }, bo);
    }
}

/// A dma-buf mmap (`i915.ptecheck=1`): are `frames` the object's pages at
/// byte offset `off`?
pub fn ptecheck_dmabuf_mmap(obj: u32, off: u64, frames: &[usize]) {
    if !ptecheck_on() { return; }
    let st = STATE.lock();
    let Some(b) = st.bos.get(&obj) else { return };
    let (phys, size) = (b.phys, b.size);
    drop(st);
    let bad = frames.iter().enumerate().position(|(i, &f)| f as u64 != phys + off + i as u64 * 4096);
    let n = PTC_NOTES.fetch_add(1, Ordering::Relaxed);
    if bad.is_some() { PTC_BAD.fetch_add(1, Ordering::Relaxed); }
    if n < PTC_NOTE_MAX || (bad.is_some() && ptc_may_log()) {
        ilog!("ptecheck: dma-buf mmap obj {:#x} phys {:#x} size {:#x} off {:#x} len {:#x} frame0 {:#x}{}",
              obj, phys, size, off, frames.len() * 4096, frames.first().copied().unwrap_or(0),
              match bad { Some(i) => alloc::format!(" MISMATCH at page {} ({:#x})", i, frames[i]), None => alloc::string::String::new() });
    }
}

/// The cache level of a normal object's PPGTT PTE: Linux's PPAT_CACHED
/// (index 4) by default; `i915.ppat_cached=0` selects index 0 (WB LLC, what
/// every object got before lane/gpucoh).
fn cached() -> Cache {
    if super::opt("i915.ppat_cached") == Some(0) { Cache::CachedPde } else { Cache::Cached }
}

/// Per-engine last submitted / completed seqno.
static SUBMITTED: [AtomicU64; NENG] = [const { AtomicU64::new(0) }; NENG];
static COMPLETED: [AtomicU64; NENG] = [const { AtomicU64::new(0) }; NENG];

macro_rules! ulog {
    ($lvl:expr, $($a:tt)*) => {
        if LOG.load(Ordering::Relaxed) >= $lvl {
            let s = alloc::format!($($a)*);
            if $lvl >= 2 && LOG.load(Ordering::Relaxed) < 3 { super::dump(&s) } else { super::log(&s) }
        }
    };
}

fn err(e: i32) -> DriverError { DriverError::Errno(e) }

// ── fences ───────────────────────────────────────────────────────────────────

const SEQ_MASK: u64 = (1 << 48) - 1;

fn fence_id(eng: usize, seq: u64) -> u64 { EXT_FENCE | (eng as u64) << 48 | (seq & SEQ_MASK) }

/// `UapiBackend::fence_done`: lock-free.
fn fence_done(f: u64) -> bool {
    let eng = ((f >> 48) & 0x7FFF) as usize;
    if eng >= NENG { return true; }
    let seq = f & SEQ_MASK;
    // The status page first-hand: no wait for the poll thread's retire.
    COMPLETED[eng].load(Ordering::Acquire) >= seq
        || (seq <= SUBMITTED[eng].load(Ordering::Acquire) && super::exec::hw_done(eng, seq))
}

fn pending(f: u64) -> bool { f != 0 && !fence_done(f) }

/// The last request of each engine that used an object (or ran in a VM):
/// one fence per engine, because a later request on another engine says
/// nothing about an earlier one here (rcs then bcs: the bcs fence can
/// signal while the rcs request still runs). Linux keeps a dma-resv
/// fence per timeline for the same reason.
#[derive(Clone, Copy, Default)]
struct Fences([u64; NENG]);

impl Fences {
    fn set(&mut self, f: u64) {
        let eng = ((f >> 48) & 0x7FFF) as usize;
        if eng < NENG { self.0[eng] = f; }
    }
    fn pending(&self) -> bool { self.0.iter().any(|&f| pending(f)) }
    /// A pending fence on another engine than `eng` (cross-engine implicit sync).
    fn other(&self, eng: usize) -> impl Iterator<Item = u64> + '_ {
        self.0.iter().enumerate().filter(move |&(e, &f)| e != eng && pending(f)).map(|(_, &f)| f)
    }
    /// Wait (CPU) for every engine's fence.
    fn wait(&self, timeout_ns: u64) -> Result<(), i32> {
        let t0 = sched::monotonic_ns();
        for &f in &self.0 {
            let used = sched::monotonic_ns().saturating_sub(t0);
            let left = if timeout_ns == u64::MAX { u64::MAX } else { timeout_ns.saturating_sub(used) };
            if pending(f) && left == 0 { return Err(ETIME); }
            wait_one(f, left)?;
        }
        Ok(())
    }
    /// The pending fence GEM_BUSY reports (the latest-engine one).
    fn busy(&self) -> Option<u64> { self.0.iter().rev().copied().find(|&f| pending(f)) }
}

// ── state ────────────────────────────────────────────────────────────────────

/// GEM handles are per open and the lowest free number from 1, as Linux's
/// idr hands them out: iris sizes a per-submission array by the largest
/// handle (`index_for_handle`), so a sparse high handle space (the NVK
/// layer's 0x4000_0000 base) makes it allocate gigabytes. The DRM layer's
/// own dumb/blob handles count up from 0x4000 globally; a clash needs one
/// open holding >16K live i915 objects and a dumb buffer at once.
fn alloc_handle(o: &Open) -> u32 {
    let mut want = 1u32;
    for &h in o.handles.keys() {
        if h != want { break; }
        want += 1;
    }
    want
}

struct Bo {
    phys: u64,
    order: usize,
    size: u64,
    handles: u32,
    exports: u32,
    tiling: u32,
    stride: u32,
    caching: u32,
    madv: u32,
    /// Fences of the last request per engine that used it.
    last_fence: Fences,
    /// Every reference is gone; freed once `last_fence` signalled.
    dead: bool,
    /// Wrapped in a KMS framebuffer with `i915.kms_uncached` on: bound
    /// uncached (PPAT 3) in every PPGTT from then on, as Linux moves a
    /// scanout object to I915_CACHE_NONE (the display does not snoop the LLC).
    scanout_uc: bool,
    /// Went through PRIME (exported or imported as a dma-buf): iris then
    /// treats it as external and renders it with the PTE MOCS, so a
    /// `scanout_uc` object is written uncached and needs no flip clflush.
    prime: bool,
    /// A CPU mapping was handed out (MMAP_OFFSET of any type: every i915
    /// mapping is write-back), so CPU stores may sit dirty in the caches at
    /// any time: each flip of it clflushes. Sticky.
    cpu_wb: bool,
    /// A one-shot cached CPU write since the last flip flush (PWRITE through
    /// the HHDM, SET_DOMAIN with the CPU write domain).
    cpu_dirty: bool,
    /// `Open::gen` of the open that created it: per-creator accounting for
    /// the test GETPARAMs, so a churn test can check ITS objects come back
    /// while a compositor is churning its own on the same device.
    creator: u32,
}

struct Vm {
    pt: Ppgtt,
    map: VmMap,
    refs: u32,
    /// Fence of the last request that ran in it: its page tables outlive
    /// the last reference until then (`dead_vms`).
    last_fence: Fences,
}

#[derive(Clone)]
struct Ctx {
    vm: u32,
    engines: Option<Vec<(u16, u16)>>,
    priority: i64,
    recoverable: bool,
    bannable: bool,
    persistence: bool,
    ringsize: u64,
    banned: bool,
    /// Batches of this context that hung or faulted (GET_RESET_STATS).
    guilty: u32,
}

struct Open {
    /// Execution-core identity of this open's contexts (`exec::Key.0`),
    /// unique for the life of the boot. NOT the open_id: the VFS hands out
    /// open_ids lowest-free-slot first, so a new open reuses the id of one
    /// that just closed, while the closed open's LRCs may still be queued
    /// on an engine (a process killed mid-stream). Keyed by open_id, the
    /// new open's context 1 picked up the dead context's LRC, and the
    /// execution core freed it at the next idle point: every later batch
    /// ran in a fresh golden context, without the 3D state (base addresses,
    /// pipeline) iris had emitted once — renders went nowhere, readbacks
    /// read zeros (i915gltest comp-* after the blit case's killed hog).
    gen: u32,
    handles: BTreeMap<u32, u32>,
    ctxs: BTreeMap<u32, Ctx>,
    vm_ids: BTreeMap<u32, u32>,
    next_ctx: u32,
    next_vm: u32,
}

struct State {
    opens: BTreeMap<u32, Open>,
    bos: BTreeMap<u32, Bo>,
    vms: BTreeMap<u32, Vm>,
    next_obj: u32,
    next_vm_key: u32,
    /// Next `Open::gen` (0 is the kernel's key space).
    next_gen: u32,
    zombies: Vec<u32>,
    /// Unreferenced VMs a request may still be walking.
    dead_vms: Vec<Vm>,
    /// Fake mode: the registers each logical context (engine, `exec::Key`)
    /// loaded with MI_LOAD_REGISTER_IMM, as its LRC image would hold them on
    /// hardware — so the soft engine models per-context state and a QEMU run
    /// catches two logical contexts sharing one image.
    fake_regs: BTreeMap<(usize, super::exec::Key), BTreeMap<u32, u32>>,
}

static STATE: KMutex<State> = KMutex::new(State {
    opens: BTreeMap::new(), bos: BTreeMap::new(), vms: BTreeMap::new(), next_obj: 1, next_vm_key: 1, next_gen: 1, zombies: Vec::new(), dead_vms: Vec::new(),
    fake_regs: BTreeMap::new(),
});
/// mmap token (= physical base) -> size. A spin-locked leaf: resolved from
/// inside the DRM device lock.
static MMAP_TOKENS: spin::Mutex<BTreeMap<u64, u64>> = spin::Mutex::new(BTreeMap::new());

/// The execution-core identity of a request on context `ctx_id` (engine-map
/// slot `slot`): see `exec::Key`. `i915.ctxslot=0` drops the slot again
/// (render and compute batches of one iris context share an image; bisect
/// only — it breaks iris).
fn lrc_key(gen: u32, ctx_id: u32, ctx: &Ctx, slot: u32) -> super::exec::Key {
    let slot = if ctx.engines.is_some() && super::opt("i915.ctxslot") != Some(0) { slot } else { 0 };
    (gen, ctx_id, slot)
}

/// Forget the fake-mode register files of an open's contexts (`ctx` None:
/// all of them).
fn st_drop_fake_regs(gen: u32, ctx: Option<u32>) {
    STATE.lock().fake_regs.retain(|&(_, k), _| !(k.0 == gen && ctx.map_or(true, |c| k.1 == c)));
}

fn new_ctx(vm: u32) -> Ctx {
    Ctx { vm, engines: None, priority: 0, recoverable: true, bannable: true, persistence: true, ringsize: 16 << 10,
          banned: false, guilty: 0 }
}

impl State {
    fn vm_create(&mut self) -> Result<u32, i32> {
        let pt = Ppgtt::new(&mut KStore).map_err(|_| ENOMEM)?;
        let key = self.next_vm_key;
        self.next_vm_key += 1;
        self.vms.insert(key, Vm { pt, map: VmMap::new(), refs: 1, last_fence: Fences::default() });
        Ok(key)
    }

    fn vm_ref(&mut self, key: u32) { if let Some(v) = self.vms.get_mut(&key) { v.refs += 1; } }

    fn vm_unref(&mut self, key: u32) {
        let Some(v) = self.vms.get_mut(&key) else { return };
        v.refs -= 1;
        if v.refs == 0 {
            let mut v = self.vms.remove(&key).expect("vm");
            // The page tables point at objects only; the objects live on.
            let _ = v.map.drain();
            if v.last_fence.pending() { self.dead_vms.push(v); } else { v.pt.destroy(&mut KStore); }
        }
    }

    fn open(&mut self, open_id: u32) -> Result<&mut Open, i32> {
        if open_id == 0 { return Err(EINVAL); }
        if !self.opens.contains_key(&open_id) {
            let vm = self.vm_create()?;
            let mut ctxs = BTreeMap::new();
            ctxs.insert(0, new_ctx(vm));
            // `i915.ctxkey=0` keys LRCs by the recycled open_id again (bisect only).
            let gen = if super::opt("i915.ctxkey") == Some(0) { open_id } else { let g = self.next_gen; self.next_gen += 1; g };
            self.opens.insert(open_id, Open { gen, handles: BTreeMap::new(), ctxs, vm_ids: BTreeMap::new(), next_ctx: 1, next_vm: 1 });
            ulog!(2, "open {}: first i915 ioctl (default context, VM key {})", open_id, vm);
        }
        Ok(self.opens.get_mut(&open_id).expect("open"))
    }

    fn obj_of(&mut self, open_id: u32, handle: u32) -> Result<u32, i32> {
        self.open(open_id)?.handles.get(&handle).copied().ok_or(ENOENT)
    }

    fn bo_unref(&mut self, obj: u32, handle: bool) {
        let Some(b) = self.bos.get_mut(&obj) else { return };
        if handle { b.handles = b.handles.saturating_sub(1); } else { b.exports = b.exports.saturating_sub(1); }
        if b.handles == 0 && b.exports == 0 && !b.dead {
            b.dead = true;
            self.zombies.push(obj);
            self.reap();
        }
    }

    /// Free dead objects the GPU is done with: unbind from every VM first,
    /// so no PTE points at a freed page.
    fn reap(&mut self) {
        let mut j = 0;
        while j < self.dead_vms.len() {
            if self.dead_vms[j].last_fence.pending() { j += 1; continue; }
            let v = self.dead_vms.swap_remove(j);
            v.pt.destroy(&mut KStore);
        }
        let mut i = 0;
        while i < self.zombies.len() {
            let obj = self.zombies[i];
            if self.bos.get(&obj).map_or(false, |b| b.last_fence.pending()) { i += 1; continue; }
            self.zombies.swap_remove(i);
            let Some(b) = self.bos.remove(&obj) else { continue };
            crate::drm_offset::release(b.phys as u64);
            for v in self.vms.values_mut() {
                if let Some((s, l)) = v.map.remove_obj(obj) { unmap_range(&mut v.pt, s, l); }
            }
            MMAP_TOKENS.lock().remove(&b.phys);
            mm::buddy::free(b.phys as usize, b.order);
            ulog!(2, "bo {} freed ({} KiB)", obj, b.size >> 10);
        }
    }

    fn ctx(&mut self, open_id: u32, id: u32) -> Result<&mut Ctx, i32> {
        self.open(open_id)?.ctxs.get_mut(&id).ok_or(ENOENT)
    }
}

fn unmap_range(pt: &mut Ppgtt, start: u64, len: u64) {
    let mut o = 0;
    while o < len { pt.unmap(&mut KStore, start + o); o += 4096; }
    KStore.flush();
}

// ── user memory ──────────────────────────────────────────────────────────────

// Every user access goes through `sched::uaccess` (SMAP-safe: a raw access
// outside it faults; fault-tolerant on a bad pointer). The ioctl argument
// layouts are plain integers, so a struct that cannot be read is all zeros
// (handle 0 / null pointers / length 0 — each rejected by its caller), and
// a store to an unwritable address is dropped.
fn read_user<T: Copy>(addr: u64) -> T {
    sched::uaccess::read_user::<T>(addr as usize).unwrap_or_else(|| unsafe { core::mem::zeroed() })
}
fn write_user<T: Copy>(addr: u64, v: T) { let _ = sched::uaccess::write_user(addr as usize, v); }

/// `n` user structs at `addr`, copied fault-tolerantly (EFAULT on a bad
/// pointer instead of a kernel fault).
fn read_array<T: Copy>(addr: u64, n: u64, max: u64) -> Result<Vec<T>, i32> {
    if n == 0 { return Ok(Vec::new()); }
    if n > max || addr == 0 { return Err(why(EINVAL, "array too long or null", n)); }
    let bytes = n as usize * core::mem::size_of::<T>();
    let mut v: Vec<T> = Vec::with_capacity(n as usize);
    if unsafe { sched::uaccess::copy_from_user(v.as_mut_ptr() as *mut u8, addr as usize, bytes) } != 0 {
        return Err(why(EFAULT, "bad user array", addr));
    }
    unsafe { v.set_len(n as usize) };
    Ok(v)
}

fn facts() -> Result<Facts, i32> {
    let g = INFO.lock();
    let i = g.as_ref().ok_or(ENODEV)?;
    let mut f = i.facts.clone();
    f.ram_total = mm::buddy::total_pages() as u64 * 4096;
    f.ram_free = mm::buddy::free_pages() as u64 * 4096;
    Ok(f)
}

fn mode() -> Mode { if MODE_FAKE.load(Ordering::Relaxed) != 0 { Mode::Fake } else { Mode::Hw } }

/// RCS timestamp ticks (REG_READ, the soft engine's timestamp writes).
fn timestamp() -> u64 {
    let (hz, m) = {
        let g = INFO.lock();
        match g.as_ref() { Some(i) => (i.facts.cs_timestamp_hz as u64, if i.mode == Mode::Hw { i.mmio } else { 0 }), None => (12_000_000, 0) }
    };
    if m != 0 {
        let mm = super::Mmio { va: m };
        if super::forcewake_get(&mm).is_ok() {
            let ts = hw::reg::RENDER_RING_BASE + hw::reg::RING_TIMESTAMP;
            let (mut hi, mut lo);
            loop {
                hi = mm.r(ts + 4);
                lo = mm.r(ts);
                if mm.r(ts + 4) == hi { break; }
            }
            super::forcewake_put(&mm);
            return (hi as u64) << 32 | lo as u64;
        }
    }
    let ns = super::now_ns() as u128;
    (ns * hz as u128 / 1_000_000_000) as u64
}

// ── dispatch ─────────────────────────────────────────────────────────────────

fn ioctl(cmd: u32, arg: usize, open_id: u32) -> Option<Result<usize, DriverError>> {
    let nr = cmd & 0xFF;
    if !(0x40..0xA0).contains(&nr) { return None; }
    let size = ioc_size(cmd) as usize;
    let a = arg as u64;
    IOCTLS.fetch_add(1, Ordering::Relaxed);
    stat_tick();
    why_clear();
    let need = |n: usize| if a == 0 || size < n { Err(why(EINVAL, "short or null argument", size as u64)) } else { Ok(()) };
    let r: Result<usize, i32> = match nr {
        NR_GETPARAM => need(16).and_then(|_| getparam(a, open_id)),
        NR_SETPARAM => Err(why(EINVAL, "SETPARAM unsupported", 0)),
        NR_QUERY => need(16).and_then(|_| query(a)),
        NR_GEM_CREATE => need(16).and_then(|_| gem_create(a, open_id, false)),
        NR_GEM_CREATE_EXT => need(24).and_then(|_| gem_create(a, open_id, true)),
        NR_GEM_MMAP_OFFSET => need(32).and_then(|_| mmap_offset(a, open_id)),
        NR_GEM_MMAP => Err(why(ENODEV, "legacy GEM_MMAP unsupported", 0)),
        NR_GEM_PWRITE => need(32).and_then(|_| gem_rw(a, open_id, true)),
        NR_GEM_PREAD => need(32).and_then(|_| gem_rw(a, open_id, false)),
        NR_GEM_SET_DOMAIN => need(12).and_then(|_| set_domain(a, open_id)),
        NR_GEM_SW_FINISH => Ok(0),
        NR_GEM_SET_CACHING => need(8).and_then(|_| caching(a, open_id, true)),
        NR_GEM_GET_CACHING => need(8).and_then(|_| caching(a, open_id, false)),
        NR_GEM_SET_TILING => need(16).and_then(|_| tiling(a, open_id, true)),
        NR_GEM_GET_TILING => need(16).and_then(|_| tiling(a, open_id, false)),
        NR_GEM_MADVISE => need(12).and_then(|_| madvise(a, open_id)),
        NR_GEM_BUSY => need(8).and_then(|_| busy(a, open_id)),
        NR_GEM_WAIT => need(16).and_then(|_| gem_wait(a, open_id)),
        NR_GEM_USERPTR => Err(why(ENODEV, "USERPTR unsupported", 0)),
        NR_GEM_GET_APERTURE => need(16).and_then(|_| aperture(a)),
        NR_GEM_CONTEXT_CREATE => need(8).and_then(|_| ctx_create(a, open_id, size >= 16)),
        NR_GEM_CONTEXT_DESTROY => need(8).and_then(|_| ctx_destroy(a, open_id)),
        NR_GEM_CONTEXT_GETPARAM => need(24).and_then(|_| ctx_getparam(a, open_id)),
        NR_GEM_CONTEXT_SETPARAM => need(24).and_then(|_| ctx_setparam_ioctl(a, open_id)),
        NR_GEM_VM_CREATE => need(16).and_then(|_| vm_create(a, open_id)),
        NR_GEM_VM_DESTROY => need(16).and_then(|_| vm_destroy(a, open_id)),
        NR_GEM_EXECBUFFER2 => need(64).and_then(|_| execbuffer2(a, open_id)),
        NR_REG_READ => need(16).and_then(|_| reg_read(a)),
        NR_GET_RESET_STATS => need(24).and_then(|_| reset_stats(a, open_id)),
        _ => Err(why(EINVAL, "unsupported ioctl", cmd as u64)),
    };
    match r {
        Ok(v) => { ulog!(2, "open {}: ioctl {:#x} size {} -> {}", open_id, nr, size, v); Some(Ok(v)) }
        Err(e) => {
            err_record(ERRORS.fetch_add(1, Ordering::Relaxed), nr, e, err_arg(nr, a, size));
            if e == ENOMEM || e == ENOSPC { ENOMEMS.fetch_add(1, Ordering::Relaxed); }
            // Probing answers (EINVAL/ENODEV/ENOENT) are routine; only a
            // failure that means "out of something" is worth remembering.
            if e != EINVAL && e != ENODEV && e != ENOENT {
                LAST_ERR.store((nr as u64) << 32 | e as u32 as u64, Ordering::Relaxed);
            }
            // Probing answers (ENODEV/EINVAL on optional features) are normal.
            let lvl = if e == EIO || e == ENOMEM || e == EFAULT { 1 } else { 2 };
            ulog!(lvl, "open {}: ioctl {:#x} (size {}, cmd {:#x}) -> -{}", open_id, nr, size, cmd, e);
            Some(Err(err(e)))
        }
    }
}

/// The argument a failing ioctl most likely failed on, when its code noted
/// none: the param of (CONTEXT_)GETPARAM/SETPARAM, the first QUERY item.
fn err_arg(nr: u32, a: u64, size: usize) -> u64 {
    let rd = |off: u64, n: usize| -> u64 {
        if a == 0 || size < off as usize + n { return 0; }
        let mut v = 0u64;
        let _ = unsafe { sched::uaccess::copy_from_user(&mut v as *mut u64 as *mut u8, (a + off) as usize, n) };
        v
    };
    match nr {
        NR_GETPARAM => rd(0, 4),
        NR_GEM_CONTEXT_GETPARAM | NR_GEM_CONTEXT_SETPARAM => rd(8, 8),
        NR_QUERY => { let items = rd(8, 8); if items == 0 { 0 } else { let mut v = 0u64; let _ = unsafe { sched::uaccess::copy_from_user(&mut v as *mut u64 as *mut u8, items as usize, 4) }; v } }
        NR_GEM_EXECBUFFER2 => rd(40, 8), // flags
        _ => 0,
    }
}

fn getparam(a: u64, open_id: u32) -> Result<usize, i32> {
    let g: Getparam = read_user(a);
    let v = match leandros_param(g.param, open_id) { Some(v) => v, None => super::uapi_abi::getparam(g.param, &facts()?)? };
    if g.value_ptr == 0 { return Err(EFAULT); }
    write_user::<i32>(g.value_ptr, v);
    Ok(0)
}

fn query(a: u64) -> Result<usize, i32> {
    let q: Query = read_user(a);
    if q.flags != 0 { return Err(EINVAL); }
    let f = facts()?;
    let items: Vec<QueryItem> = read_array(q.items_ptr, q.num_items as u64, 64)?;
    for (i, it) in items.iter().enumerate() {
        let blob = query_blob(it.query_id, it.flags, &f);
        let (len, copy) = query_length(it.length, &blob);
        if copy {
            let b = blob.as_ref().expect("blob");
            if it.data_ptr == 0 { return Err(EFAULT); }
            if unsafe { sched::uaccess::copy_to_user(it.data_ptr as usize, b.as_ptr(), b.len()) } != 0 {
                return Err(EFAULT);
            }
        }
        // `length` is the i32 at offset 8 of the item.
        write_user::<i32>(q.items_ptr + i as u64 * 24 + 8, len);
    }
    Ok(0)
}

fn aperture(a: u64) -> Result<usize, i32> {
    let f = facts()?;
    write_user(a, GemGetAperture { aper_size: f.aperture, aper_available_size: f.aperture });
    Ok(0)
}

fn reg_read(a: u64) -> Result<usize, i32> {
    let r: RegRead = read_user(a);
    // Linux's whitelist on gen9: only the RCS timestamp, 8 bytes; the low
    // bits select the 8B_WA (1) / 2x32 (2) read flavours.
    if r.offset & !3 != (hw::reg::RENDER_RING_BASE + hw::reg::RING_TIMESTAMP) as u64 || r.offset & 3 == 3 {
        return Err(EINVAL);
    }
    write_user(a + 8, timestamp());
    Ok(0)
}

// ── GEM objects ──────────────────────────────────────────────────────────────

/// Largest object: one buddy block of 2^18 pages (1 GiB).
const MAX_BO: u64 = 1 << 30;
/// First i915 object number (`i915.objbase`, see the GEM_CREATE path).
const OBJ_BASE: u32 = 0x4000_0000;

fn gem_create(a: u64, open_id: u32, ext: bool) -> Result<usize, i32> {
    let c: GemCreateExt = if ext { read_user(a) } else {
        let c: GemCreate = read_user(a);
        GemCreateExt { size: c.size, handle: 0, flags: 0, extensions: 0 }
    };
    if ext {
        if c.flags & !GEM_CREATE_EXT_FLAG_NEEDS_CPU_ACCESS != 0 { return Err(EINVAL); }
        let mut p = c.extensions;
        let mut n = 0;
        while p != 0 {
            n += 1;
            if n > 8 { return Err(E2BIG); }
            let e: UserExtension = read_user(p);
            match e.name {
                GEM_CREATE_EXT_MEMORY_REGIONS => {
                    let m: CreateExtMemoryRegions = read_user(p);
                    if m.pad != 0 || m.num_regions == 0 { return Err(EINVAL); }
                    let regions: Vec<MemoryClassInstance> = read_array(m.regions, m.num_regions as u64, 8)?;
                    if regions.iter().any(|r| r.class != MEMORY_CLASS_SYSTEM || r.instance != 0) { return Err(EINVAL); }
                }
                GEM_CREATE_EXT_PROTECTED_CONTENT => return Err(ENODEV),
                _ => return Err(EINVAL),
            }
            p = e.next_extension;
        }
    }
    if c.size == 0 { return Err(EINVAL); }
    let size = c.size.checked_add(4095).ok_or(E2BIG)? & !4095;
    if size > MAX_BO { return Err(E2BIG); }
    let order = (size / 4096).next_power_of_two().trailing_zeros() as usize;
    let mut st = STATE.lock();
    st.open(open_id)?;
    st.reap();
    let phys = mm::buddy::alloc(order).ok_or(ENOMEM)? as u64;
    unsafe { ptr::write_bytes(mm::phys_to_virt(phys as usize) as *mut u8, 0, size as usize); }
    // A dma-buf records only a bare object number, and the DRM layer's
    // PRIME import / fd-close release try the backend FIRST with it
    // (`prime_import_blob`, `bo_release_exported`): an exported dumb
    // buffer's key (a small handle number) or a blob obj that equals one
    // of our object numbers resolved to the wrong i915 object — the
    // importer got a handle to an unrelated BO, and closing such an fd
    // dropped an export reference of ours (free while in use). Our numbers
    // start at 0x4000_0000 so the spaces are disjoint; `i915.objbase=0`
    // restores numbering from 1.
    if st.next_obj == 1 && super::opt("i915.objbase").unwrap_or(1) != 0 { st.next_obj = OBJ_BASE; }
    let obj = st.next_obj;
    st.next_obj += 1;
    CREATES.fetch_add(1, Ordering::Relaxed);
    st.bos.insert(obj, Bo { phys, order, size, handles: 1, exports: 0, tiling: 0, stride: 0, caching: CACHING_CACHED,
                            madv: MADV_WILLNEED, last_fence: Fences::default(), dead: false, scanout_uc: false, prime: false, cpu_wb: false, cpu_dirty: false,
                            creator: 0 });
    let o = st.open(open_id)?;
    let h = alloc_handle(o);
    o.handles.insert(h, obj);
    let gen = o.gen;
    if let Some(b) = st.bos.get_mut(&obj) { b.creator = gen; }
    drop(st);
    MMAP_TOKENS.lock().insert(phys, size);
    // size (u64 @0) and handle (u32 @8) in both layouts.
    write_user::<u64>(a, size);
    write_user::<u32>(a + 8, h);
    ulog!(2, "open {}: GEM_CREATE{} {} KiB -> handle {:#x} (obj {}, phys {:#x})", open_id, if ext { "_EXT" } else { "" },
          size >> 10, h, obj, phys);
    Ok(0)
}

fn with_bo<R>(open_id: u32, handle: u32, f: impl FnOnce(&mut Bo) -> Result<R, i32>) -> Result<R, i32> {
    let mut st = STATE.lock();
    let obj = st.obj_of(open_id, handle)?;
    let b = st.bos.get_mut(&obj).ok_or(ENOENT)?;
    f(b)
}

fn mmap_offset(a: u64, open_id: u32) -> Result<usize, i32> {
    let m: GemMmapOffset = read_user(a);
    if m.extensions != 0 || m.pad != 0 { return Err(EINVAL); }
    match m.flags {
        MMAP_OFFSET_WB | MMAP_OFFSET_WC | MMAP_OFFSET_UC => {}
        // No aperture (GTT) mappings, no FIXED outside discrete parts.
        MMAP_OFFSET_GTT => return Err(ENODEV),
        _ => return Err(EINVAL),
    }
    let (phys, size) = with_bo(open_id, m.handle, |b| { b.cpu_wb = true; Ok((b.phys, b.size)) })?;
    // Opaque offset, never the physical base (resolved in `handle_ioctl_mmap`).
    let off = crate::drm_offset::offset_for(phys as u64, size as u64).ok_or(EINVAL)?;
    write_user::<u64>(a + 8, off);
    Ok(0)
}

fn gem_rw(a: u64, open_id: u32, write: bool) -> Result<usize, i32> {
    let p: GemPrw = read_user(a);
    let (phys, size, f) = with_bo(open_id, p.handle, |b| {
        if write { b.cpu_dirty = true; }
        Ok((b.phys, b.size, b.last_fence))
    })?;
    if p.offset.checked_add(p.size).map_or(true, |e| e > size) { return Err(EINVAL); }
    if p.size == 0 { return Ok(0); }
    if p.data_ptr == 0 { return Err(EFAULT); }
    f.wait(u64::MAX)?;
    let k = (mm::phys_to_virt(phys as usize) as u64 + p.offset) as *mut u8;
    let left = unsafe {
        if write { sched::uaccess::copy_from_user(k, p.data_ptr as usize, p.size as usize) }
        else { sched::uaccess::copy_to_user(p.data_ptr as usize, k as *const u8, p.size as usize) }
    };
    if left != 0 { return Err(EFAULT); }
    Ok(0)
}

/// Wait (CPU) for one fence; `timeout_ns` relative, u64::MAX = 10 s bound.
fn wait_one(f: u64, timeout_ns: u64) -> Result<(), i32> {
    if !pending(f) { return Ok(()); }
    if timeout_ns == 0 { return Err(ETIME); }
    let now = sched::monotonic_ns();
    let deadline = if timeout_ns == u64::MAX { now + 10_000_000_000 } else { now.saturating_add(timeout_ns) };
    match crate::drm_device_interface::wait_fences(&[f], deadline) {
        Ok(()) => Ok(()),
        Err(DriverError::Io) => Err(EINTR),
        Err(_) => Err(ETIME),
    }
}

fn set_domain(a: u64, open_id: u32) -> Result<usize, i32> {
    let d: GemSetDomain = read_user(a);
    if d.write_domain != 0 && d.write_domain != d.read_domains { return Err(EINVAL); }
    let f = with_bo(open_id, d.handle, |b| {
        if d.write_domain & GEM_DOMAIN_CPU != 0 { b.cpu_dirty = true; }
        Ok(b.last_fence)
    })?;
    // Moving to a CPU-visible domain waits for the GPU (LLC: no flush).
    if d.read_domains & (GEM_DOMAIN_CPU | GEM_DOMAIN_GTT | GEM_DOMAIN_WC) != 0 || d.write_domain != 0 {
        f.wait(u64::MAX)?;
    }
    Ok(0)
}

fn caching(a: u64, open_id: u32, set: bool) -> Result<usize, i32> {
    let c: GemCaching = read_user(a);
    if set && c.caching > CACHING_DISPLAY { return Err(EINVAL); }
    let v = with_bo(open_id, c.handle, |b| { if set { b.caching = c.caching; } Ok(b.caching) })?;
    if !set { write_user::<u32>(a + 4, v); }
    Ok(0)
}

fn tiling(a: u64, open_id: u32, set: bool) -> Result<usize, i32> {
    if set {
        let t: GemTiling = read_user(a);
        if t.tiling_mode > TILING_Y { return Err(EINVAL); }
        let stride = if t.tiling_mode == TILING_NONE { 0 } else { t.stride };
        with_bo(open_id, t.handle, |b| { b.tiling = t.tiling_mode; b.stride = stride; Ok(()) })?;
        write_user(a, GemTiling { handle: t.handle, tiling_mode: t.tiling_mode, stride, swizzle_mode: BIT_6_SWIZZLE_NONE });
    } else {
        let t: GemGetTiling = read_user(a);
        let mode = with_bo(open_id, t.handle, |b| Ok(b.tiling))?;
        write_user(a, GemGetTiling { handle: t.handle, tiling_mode: mode, swizzle_mode: BIT_6_SWIZZLE_NONE,
                                     phys_swizzle_mode: BIT_6_SWIZZLE_NONE });
    }
    Ok(0)
}

fn madvise(a: u64, open_id: u32) -> Result<usize, i32> {
    let m: GemMadvise = read_user(a);
    if m.madv > MADV_DONTNEED { return Err(EINVAL); }
    with_bo(open_id, m.handle, |b| { b.madv = m.madv; Ok(()) })?;
    // Nothing is ever purged: the pages are always retained.
    write_user::<u32>(a + 8, 1);
    Ok(0)
}

fn busy(a: u64, open_id: u32) -> Result<usize, i32> {
    let b: GemBusy = read_user(a);
    let f = with_bo(open_id, b.handle, |o| Ok(o.last_fence))?;
    let v = if let Some(f) = f.busy() {
        let eng = ((f >> 48) & 0x7FFF) as usize;
        let class = ENGINES.get(eng).map_or(0, |e| e.0 as u32);
        (1 << (16 + class)) | (class + 1)
    } else { 0 };
    write_user::<u32>(a + 4, v);
    Ok(0)
}

fn gem_wait(a: u64, open_id: u32) -> Result<usize, i32> {
    let w: GemWait = read_user(a);
    if w.flags != 0 { return Err(EINVAL); }
    let f = with_bo(open_id, w.bo_handle, |b| Ok(b.last_fence))?;
    let t0 = sched::monotonic_ns();
    let r = f.wait(if w.timeout_ns < 0 { u64::MAX } else { w.timeout_ns as u64 });
    if w.timeout_ns > 0 {
        let left = (w.timeout_ns as u64).saturating_sub(sched::monotonic_ns() - t0);
        write_user::<i64>(a + 8, left as i64);
    }
    r.map(|_| 0)
}

// ── contexts and VMs ─────────────────────────────────────────────────────────

fn ctx_create(a: u64, open_id: u32, ext: bool) -> Result<usize, i32> {
    let c: ContextCreateExt = if ext { read_user(a) } else { ContextCreateExt { ctx_id: 0, flags: 0, extensions: 0 } };
    if c.flags & !(CONTEXT_CREATE_FLAGS_USE_EXTENSIONS | CONTEXT_CREATE_FLAGS_SINGLE_TIMELINE) != 0 { return Err(EINVAL); }
    // Collect the SETPARAM chain before taking the lock (user memory).
    let mut params = Vec::new();
    if c.flags & CONTEXT_CREATE_FLAGS_USE_EXTENSIONS != 0 {
        let mut p = c.extensions;
        while p != 0 {
            if params.len() >= 16 { return Err(E2BIG); }
            let e: ContextCreateExtSetparam = read_user(p);
            if e.base.name != CONTEXT_CREATE_EXT_SETPARAM { return Err(EINVAL); }
            if e.param.ctx_id != 0 { return Err(EINVAL); }
            params.push((e.param, param_bytes(&e.param)?));
            p = e.base.next_extension;
        }
    }
    let mut st = STATE.lock();
    st.open(open_id)?;
    let vm = st.vm_create()?;
    let mut ctx = new_ctx(vm);
    for (p, bytes) in &params {
        if let Err(e) = apply_param(&mut st, open_id, &mut ctx, p, bytes) {
            st.vm_unref(ctx.vm);
            return Err(e);
        }
    }
    let o = st.open(open_id)?;
    let id = o.next_ctx;
    o.next_ctx += 1;
    o.ctxs.insert(id, ctx);
    drop(st);
    write_user::<u32>(a, id);
    ulog!(2, "open {}: context {} created ({} params)", open_id, id, params.len());
    Ok(0)
}

/// The bytes behind a pointer-valued parameter (ENGINES), read up front.
fn param_bytes(p: &ContextParam) -> Result<Vec<u8>, i32> {
    if p.param == CONTEXT_PARAM_ENGINES && p.size != 0 {
        if p.size > 8 + 4 * 64 { return Err(EINVAL); }
        return read_array::<u8>(p.value, p.size as u64, 8 + 4 * 64);
    }
    Ok(Vec::new())
}

fn apply_param(st: &mut State, open_id: u32, ctx: &mut Ctx, p: &ContextParam, bytes: &[u8]) -> Result<(), i32> {
    let flag = |v: u64| if p.size != 0 { Err(EINVAL) } else { Ok(v != 0) };
    match p.param {
        CONTEXT_PARAM_VM => {
            if p.size != 0 { return Err(EINVAL); }
            let key = *st.open(open_id)?.vm_ids.get(&(p.value as u32)).ok_or(ENOENT)?;
            st.vm_ref(key);
            let old = core::mem::replace(&mut ctx.vm, key);
            st.vm_unref(old);
        }
        CONTEXT_PARAM_ENGINES => {
            ctx.engines = if p.size == 0 { None } else { Some(parse_engine_map(bytes, &exposed())?) };
        }
        CONTEXT_PARAM_RECOVERABLE => ctx.recoverable = flag(p.value)?,
        CONTEXT_PARAM_BANNABLE => ctx.bannable = flag(p.value)?,
        CONTEXT_PARAM_PERSISTENCE => ctx.persistence = flag(p.value)?,
        CONTEXT_PARAM_NO_ERROR_CAPTURE | CONTEXT_PARAM_NO_ZEROMAP => { flag(p.value)?; }
        CONTEXT_PARAM_PRIORITY => {
            let v = p.value as i64;
            if p.size != 0 || !(-1023..=1023).contains(&v) { return Err(EINVAL); }
            ctx.priority = v;
        }
        CONTEXT_PARAM_RINGSIZE => {
            if p.size != 0 || p.value < 4096 || p.value > 512 << 10 || p.value & 4095 != 0 { return Err(EINVAL); }
            ctx.ringsize = p.value;
        }
        CONTEXT_PARAM_PROTECTED_CONTENT => if flag(p.value)? { return Err(ENODEV) },
        CONTEXT_PARAM_SSEU => return Err(ENODEV),
        _ => return Err(EINVAL),
    }
    Ok(())
}

fn ctx_setparam_ioctl(a: u64, open_id: u32) -> Result<usize, i32> {
    let p: ContextParam = read_user(a);
    let bytes = param_bytes(&p)?;
    let mut st = STATE.lock();
    let mut ctx = st.ctx(open_id, p.ctx_id)?.clone();
    apply_param(&mut st, open_id, &mut ctx, &p, &bytes)?;
    *st.ctx(open_id, p.ctx_id)? = ctx;
    if p.param == CONTEXT_PARAM_ENGINES {
        // Linux replaces the context's engines with new logical contexts
        // (fresh state); ours are created again at the next submission.
        let gen = st.open(open_id)?.gen;
        drop(st);
        super::exec::release(|k| k.0 == gen && k.1 == p.ctx_id);
        st_drop_fake_regs(gen, Some(p.ctx_id));
    }
    Ok(0)
}

fn ctx_getparam(a: u64, open_id: u32) -> Result<usize, i32> {
    let mut p: ContextParam = read_user(a);
    let mut st = STATE.lock();
    let c = st.ctx(open_id, p.ctx_id)?.clone();
    match p.param {
        CONTEXT_PARAM_VM => {
            // A new id naming the context's VM (Linux: one id per query).
            st.vm_ref(c.vm);
            let o = st.open(open_id)?;
            let id = o.next_vm;
            o.next_vm += 1;
            o.vm_ids.insert(id, c.vm);
            p.size = 0;
            p.value = id as u64;
        }
        CONTEXT_PARAM_ENGINES => {
            let map = c.engines.unwrap_or_default();
            let need = if map.is_empty() { 0 } else { 8 + 4 * map.len() as u32 };
            if p.size != 0 && need != 0 {
                if p.size < need { return Err(EINVAL); }
                let mut v = alloc::vec![0u8; need as usize];
                for (i, e) in map.iter().enumerate() {
                    v[8 + 4 * i..10 + 4 * i].copy_from_slice(&e.0.to_le_bytes());
                    v[10 + 4 * i..12 + 4 * i].copy_from_slice(&e.1.to_le_bytes());
                }
                drop(st);
                if unsafe { sched::uaccess::copy_to_user(p.value as usize, v.as_ptr(), v.len()) } != 0 {
                    return Err(EFAULT);
                }
            }
            p.size = need;
        }
        CONTEXT_PARAM_RECOVERABLE => { p.size = 0; p.value = c.recoverable as u64; }
        CONTEXT_PARAM_BANNABLE => { p.size = 0; p.value = c.bannable as u64; }
        CONTEXT_PARAM_PERSISTENCE => { p.size = 0; p.value = c.persistence as u64; }
        CONTEXT_PARAM_PRIORITY => { p.size = 0; p.value = c.priority as u64; }
        CONTEXT_PARAM_RINGSIZE => { p.size = 0; p.value = c.ringsize; }
        CONTEXT_PARAM_SSEU => {
            drop(st);
            if p.size != 0 {
                if (p.size as usize) < CONTEXT_SSEU_SIZE || p.value == 0 { return Err(EINVAL); }
                let user: [u8; CONTEXT_SSEU_SIZE] = read_user(p.value);
                let out = context_sseu(&user, &facts()?.sseu, c.engines.as_deref(), &exposed())?;
                write_user(p.value, out);
            }
            p.size = CONTEXT_SSEU_SIZE as u32;
        }
        x => { p.size = 0; p.value = context_getparam_static(x).ok_or(EINVAL)?; }
    }
    write_user(a, p);
    Ok(0)
}

fn ctx_destroy(a: u64, open_id: u32) -> Result<usize, i32> {
    let d: ContextDestroy = read_user(a);
    if d.pad != 0 { return Err(EINVAL); }
    if d.ctx_id == 0 { return Err(ENOENT); }
    let mut st = STATE.lock();
    let o = st.open(open_id)?;
    let gen = o.gen;
    let c = o.ctxs.remove(&d.ctx_id).ok_or(ENOENT)?;
    st.vm_unref(c.vm);
    drop(st);
    super::exec::release(|k| k.0 == gen && k.1 == d.ctx_id);
    st_drop_fake_regs(gen, Some(d.ctx_id));
    Ok(0)
}

fn vm_create(a: u64, open_id: u32) -> Result<usize, i32> {
    let v: VmControl = read_user(a);
    if v.extensions != 0 || v.flags != 0 { return Err(EINVAL); }
    let mut st = STATE.lock();
    st.open(open_id)?;
    let key = st.vm_create()?;
    let o = st.open(open_id)?;
    let id = o.next_vm;
    o.next_vm += 1;
    o.vm_ids.insert(id, key);
    drop(st);
    write_user::<u32>(a + 12, id);
    Ok(0)
}

fn vm_destroy(a: u64, open_id: u32) -> Result<usize, i32> {
    let v: VmControl = read_user(a);
    if v.extensions != 0 || v.flags != 0 { return Err(EINVAL); }
    let mut st = STATE.lock();
    let key = st.open(open_id)?.vm_ids.remove(&v.vm_id).ok_or(ENOENT)?;
    st.vm_unref(key);
    Ok(0)
}

fn reset_stats(a: u64, open_id: u32) -> Result<usize, i32> {
    let r: ResetStats = read_user(a);
    if r.flags != 0 || r.pad != 0 { return Err(EINVAL); }
    let guilty = STATE.lock().ctx(open_id, r.ctx_id)?.guilty;
    write_user(a, ResetStats { ctx_id: r.ctx_id, flags: 0, reset_count: RESETS.load(Ordering::Relaxed),
                               batch_active: guilty, batch_pending: 0, pad: 0 });
    Ok(0)
}

// ── EXECBUFFER2 ──────────────────────────────────────────────────────────────

/// Dwords the soft engine runs before it calls a batch a runaway (a hang).
const SOFTCS_LIMIT: u32 = 1 << 22;

/// One syncobj of a submission: (handle, point, wait, signal).
type SyncRef = (u32, u64, bool, bool);

fn exec_syncs(e: &Execbuffer2, plan: &ExecPlan) -> Result<Vec<SyncRef>, i32> {
    let mut v = Vec::new();
    if plan.fence_array {
        let fences: Vec<ExecFence> = read_array(e.cliprects_ptr, e.num_cliprects as u64, 4096)?;
        for f in fences {
            if f.flags & !(EXEC_FENCE_WAIT | EXEC_FENCE_SIGNAL) != 0 { return Err(EINVAL); }
            v.push((f.handle, 0, f.flags & EXEC_FENCE_WAIT != 0, f.flags & EXEC_FENCE_SIGNAL != 0));
        }
    }
    if plan.extensions {
        let mut p = e.cliprects_ptr;
        let mut n = 0;
        while p != 0 {
            n += 1;
            if n > 4 { return Err(E2BIG); }
            let x: ExecTimelineFences = read_user(p);
            if x.base.name != EXECBUFFER_EXT_TIMELINE_FENCES { return Err(EINVAL); }
            let fences: Vec<ExecFence> = read_array(x.handles_ptr, x.fence_count, 4096)?;
            let points: Vec<u64> = read_array(x.values_ptr, x.fence_count, 4096)?;
            for (f, pt) in fences.iter().zip(points) {
                if f.flags & !(EXEC_FENCE_WAIT | EXEC_FENCE_SIGNAL) != 0 { return Err(EINVAL); }
                v.push((f.handle, pt, f.flags & EXEC_FENCE_WAIT != 0, f.flags & EXEC_FENCE_SIGNAL != 0));
            }
            p = x.base.next_extension;
        }
    }
    Ok(v)
}

/// GPU memory of one VM as the soft command streamer sees it: only pages
/// bound to an object are reachable; the scratch page is a fault.
struct VmMem<'a> { pt: &'a Ppgtt, regs: &'a mut BTreeMap<u32, u32> }

impl VmMem<'_> {
    fn kva(&self, va: u64) -> Option<usize> {
        let phys = self.pt.translate(&KStore, va & !3)?;
        if phys & !4095 == self.pt.scratch[0] { return None; }
        Some(mm::phys_to_virt(phys as usize))
    }
}

impl softcs::GpuMem for VmMem<'_> {
    fn read32(&self, va: u64) -> Option<u32> {
        self.kva(va).map(|k| unsafe { ptr::read_volatile(k as *const u32) })
    }
    fn write32(&mut self, va: u64, v: u32) -> bool {
        match self.kva(va) { Some(k) => { unsafe { ptr::write_volatile(k as *mut u32, v) }; true } None => false }
    }
    fn timestamp(&self) -> u64 { timestamp() }
    fn reg_write(&mut self, reg: u32, v: u32) { self.regs.insert(reg, v); }
    fn reg_read(&self, reg: u32) -> Option<u32> { self.regs.get(&reg).copied() }
}

fn execbuffer2(a: u64, open_id: u32) -> Result<usize, i32> {
    let e: Execbuffer2 = read_user(a);
    let ctx_id = e.rsvd1 as u32;
    if e.rsvd1 >> 32 != 0 { return Err(why(EINVAL, "exec: rsvd1 high bits", e.rsvd1)); }
    if e.flags & (EXEC_FENCE_IN | EXEC_FENCE_OUT) != 0 { return Err(why(EINVAL, "exec: sync_file fences unsupported", e.flags)); }
    let user_engines = {
        let mut st = STATE.lock();
        st.ctx(open_id, ctx_id).map_err(|e| why(e, "exec: no such context", ctx_id as u64))?.engines.as_ref().map(|m| m.len() as u32)
    };
    let legacy = |r: u64| legacy_engine(r).and_then(engine_index).is_some();
    let plan = check_execbuf_why(&e, user_engines, &legacy).map_err(|w| whyw(w, e.flags))?;
    let objs: Vec<ExecObject2> = read_array(e.buffers_ptr, e.buffer_count as u64, 1 << 16)?;
    let syncs = exec_syncs(&e, &plan)?;
    // Relocation entries, copied before any lock is taken.
    let mut nrel = 0u64;
    let mut relocs: Vec<Vec<RelocEntry>> = Vec::with_capacity(objs.len());
    for o in &objs {
        nrel += o.relocation_count as u64;
        if nrel > MAX_RELOCS { return Err(why(EINVAL, "reloc: too many", nrel)); }
        relocs.push(read_array(o.relocs_ptr, o.relocation_count as u64, MAX_RELOCS)?);
    }

    // In-fences, then out-syncobj existence: before anything is bound.
    let mut wait = Vec::new();
    for &(h, pt, w, s) in &syncs {
        if w {
            match crate::drm_device_interface::syncobj_in_fences(h, open_id, pt) {
                Ok(f) => wait.extend(f),
                Err(DriverError::NotFound) => return Err(ENOENT),
                Err(_) => return Err(EINVAL),
            }
        }
        if s && !crate::drm_device_interface::syncobj_exists(h, open_id) { return Err(ENOENT); }
    }
    if !wait.is_empty() {
        let deadline = sched::monotonic_ns() + 10_000_000_000;
        match crate::drm_device_interface::wait_fences(&wait, deadline) {
            Ok(()) => {}
            Err(DriverError::Io) => return Err(EINTR),
            Err(_) => { ulog!(1, "open {}: execbuffer in-fences not signalled in 10 s", open_id); return Err(ETIME); }
        }
    }

    // Cross-engine implicit sync: one engine runs its requests in order,
    // but nothing orders rcs against bcs. An object a request on the other
    // engine still uses (a blit writing what this batch samples, or the
    // reverse) is waited for here, as Linux awaits the dma-resv fences
    // (EXEC_OBJECT_ASYNC opts out, as there).
    if mode() == Mode::Hw {
        let other: Vec<u64> = {
            let mut st = STATE.lock();
            let ctx = st.ctx(open_id, ctx_id)?.clone();
            let eng_cls = match &ctx.engines {
                Some(m) => *m.get(plan.engine as usize).ok_or(EINVAL)?,
                None => legacy_engine(plan.engine as u64).ok_or(EINVAL)?,
            };
            let eng = engine_index(eng_cls).ok_or(EINVAL)?;
            let mut v = Vec::new();
            for o in objs.iter().filter(|o| o.flags & EXEC_OBJECT_ASYNC == 0) {
                let Ok(obj) = st.obj_of(open_id, o.handle) else { continue };
                if let Some(b) = st.bos.get(&obj) { v.extend(b.last_fence.other(eng)); }
            }
            v
        };
        if !other.is_empty() {
            if CROSS_WAITS.fetch_add(1, Ordering::Relaxed) == 0 {
                ulog!(1, "open {}: first cross-engine implicit-sync wait ({} fences)", open_id, other.len());
            }
            let deadline = sched::monotonic_ns() + 10_000_000_000;
            match crate::drm_device_interface::wait_fences(&other, deadline) {
                Ok(()) => {}
                Err(DriverError::Io) => return Err(EINTR),
                Err(_) => return Err(ETIME),
            }
        }
    }

    // Place, bind and relocate under the state lock. Relocations are CPU
    // writes into the objects: an object still in use by the GPU is waited
    // for first (Linux: `i915_gem_object_prepare_write`), with the lock
    // dropped, and the pass starts again.
    let lut = e.flags & EXEC_HANDLE_LUT != 0;
    let any_relocs = relocs.iter().any(|r| !r.is_empty());
    let mut tries = 0;
    let (mut st, ctx, eng, list, batch_off, batch_len, patches) = loop {
        let mut st = STATE.lock();
        st.reap();
        let ctx = st.ctx(open_id, ctx_id).map_err(|e| why(e, "exec: no such context", ctx_id as u64))?.clone();
        if ctx.banned { return Err(why(EIO, "exec: context banned", ctx_id as u64)); }
        let eng_cls = match &ctx.engines {
            Some(m) => *m.get(plan.engine as usize).ok_or_else(|| why(EINVAL, "exec: engine slot", plan.engine as u64))?,
            None => legacy_engine(plan.engine as u64).ok_or_else(|| why(EINVAL, "exec: legacy ring", plan.engine as u64))?,
        };
        let eng = engine_index(eng_cls).ok_or_else(|| why(EINVAL, "exec: engine not exposed", eng_cls.0 as u64))?;
        // Resolve and check every object, then place the unpinned ones.
        let mut reqs: Vec<PlaceReq> = Vec::with_capacity(objs.len());
        for o in &objs {
            let obj = st.obj_of(open_id, o.handle).map_err(|e| why(e, "exec: unknown handle", o.handle as u64))?;
            let size = st.bos.get(&obj).ok_or_else(|| why(ENOENT, "exec: object gone", o.handle as u64))?.size;
            let off = check_exec_object_why(o, hw::PPGTT_SIZE, size).map_err(|w| whyw(w, o.handle as u64))?;
            if reqs.iter().any(|r| r.obj == obj) { return Err(why(EINVAL, "exec: object listed twice", o.handle as u64)); }
            reqs.push(PlaceReq { obj, size, flags: o.flags, offset: off, alignment: o.alignment, pad_to_size: o.rsvd1 });
        }
        let addrs = {
            let vm = st.vms.get(&ctx.vm).ok_or_else(|| why(ENOENT, "exec: VM gone", ctx.vm as u64))?;
            assign_offsets(&reqs, &vm.map, hw::PPGTT_SIZE).map_err(|w| whyw(w, 0))?
        };
        let list: Vec<(u32, u64, u64)> = reqs.iter().zip(&addrs).map(|(r, &a)| (r.obj, a, r.size)).collect();
        let (_, batch_off, batch_size) = list[plan.batch_index as usize];
        let batch_len = if e.batch_len == 0 { batch_size.saturating_sub(plan.batch_start as u64) } else { e.batch_len as u64 };
        if plan.batch_start as u64 + batch_len > batch_size || batch_len < 4 {
            return Err(why(EINVAL, "exec: batch_start/len outside the batch", batch_len));
        }
        // I915_EXEC_NO_RELOC: userspace promises every presumed address is
        // what it last got back, so only an object that moved forces the
        // scan (Linux `__EXEC_HAS_RELOC`).
        let moved = objs.iter().zip(&addrs).any(|(o, &a)| noncanonical(o.offset) != a);
        let patches = if !any_relocs || (e.flags & EXEC_NO_RELOC != 0 && !moved) { Vec::new() } else {
            let handles: Vec<u32> = objs.iter().map(|o| o.handle).collect();
            let sizes: Vec<u64> = reqs.iter().map(|r| r.size).collect();
            plan_relocs(&relocs, &handles, &sizes, &addrs, lut).map_err(|w| whyw(w, 0))?
        };
        if any_relocs {
            RELOCS_SKIPPED.fetch_add(nrel - patches.len() as u64, Ordering::Relaxed);
        }
        if !patches.is_empty() && tries < 3 {
            let mut busy: Vec<u64> = Vec::new();
            for p in &patches {
                if let Some(b) = st.bos.get(&list[p.obj].0) { busy.extend(b.last_fence.0.iter().copied().filter(|&f| pending(f))); }
            }
            if !busy.is_empty() {
                busy.sort_unstable();
                busy.dedup();
                drop(st);
                tries += 1;
                let deadline = sched::monotonic_ns() + 10_000_000_000;
                match crate::drm_device_interface::wait_fences(&busy, deadline) {
                    Ok(()) => {}
                    Err(DriverError::Io) => return Err(why(EINTR, "reloc: wait for idle interrupted", 0)),
                    Err(_) => return Err(why(ETIME, "reloc: object busy 10 s", 0)),
                }
                continue;
            }
        }
        for (o, &a) in objs.iter().zip(&addrs) {
            if o.flags & EXEC_OBJECT_PINNED == 0 && noncanonical(o.offset) != a { PLACED.fetch_add(1, Ordering::Relaxed); }
        }
        {
            let State { vms, bos, .. } = &mut *st;
            let vm = vms.get_mut(&ctx.vm).ok_or(ENOENT)?;
            for &(obj, off, size) in &list {
                let p = vm.map.plan(obj, off, size);
                if !p.bind { continue; }
                for &(s, l) in &p.unbind { unmap_range(&mut vm.pt, s, l); }
                let (phys, uc) = bos.get(&obj).map(|b| (b.phys, b.scanout_uc)).ok_or(ENOENT)?;
                let cache = if uc { Cache::Uncached } else { cached() };
                if vm.pt.map_range(&mut KStore, off, phys, size, cache).is_err() {
                    // Leave the map consistent: whatever got written is unbound.
                    unmap_range(&mut vm.pt, off, size);
                    vm.map.commit(obj, off, size, &super::vmbind::Plan { unbind: p.unbind.clone(), bind: false });
                    return Err(why(ENOMEM, "exec: page tables", off));
                }
                vm.map.commit(obj, off, size, &p);
            }
            // The relocations themselves: 64-bit writes into the objects.
            for p in &patches {
                let Some(b) = bos.get(&list[p.obj].0) else { continue };
                let pa = b.phys + p.offset;
                unsafe { ptr::write_unaligned(mm::phys_to_virt(pa as usize) as *mut u64, p.value) };
                if b.scanout_uc { super::kms::flush_for_display(pa, 8); }
            }
        }
        RELOCS_PATCHED.fetch_add(patches.len() as u64, Ordering::Relaxed);
        break (st, ctx, eng, list, batch_off, batch_len, patches);
    };
    if ptecheck_on() {
        ptecheck_exec(&st, open_id, ctx.vm, &objs, &list);
        ptc_summary();
    }

    let mode = mode();
    if mode == Mode::Hw {
        if !super::exec::ready() {
            drop(st);
            ulog!(1, "open {}: EXECBUFFER2 refused: hardware execution is off (needs i915.exec=1 and a successful bring-up)", open_id);
            return Err(why(ENODEV, "exec: hardware execution off", 0));
        }
        // Real engine: queue the batch behind this engine's earlier requests.
        let gen = st.open(open_id)?.gen;
        let seq = SUBMITTED[eng].fetch_add(1, Ordering::AcqRel) + 1;
        let fence = fence_id(eng, seq);
        let start = batch_off + plan.batch_start as u64;
        let pml4 = {
            let vm = st.vms.get_mut(&ctx.vm).ok_or(ENOENT)?;
            vm.last_fence.set(fence);
            vm.pt.pml4
        };
        for &(obj, _, _) in &list {
            if let Some(b) = st.bos.get_mut(&obj) { b.last_fence.set(fence); }
        }
        let key = lrc_key(gen, ctx_id, &ctx, plan.engine);
        if let Err(e) = super::exec::submit(eng, key, seq, start, pml4, ctx.ringsize as u32) {
            COMPLETED[eng].fetch_max(seq, Ordering::AcqRel);
            return Err(why(e, "exec: submit", eng as u64));
        }
        drop(st);
        exec_writeback(&e, &objs, &list, &patches);
        let n = EXECS.fetch_add(1, Ordering::Relaxed) + 1;
        ulog!(2, "open {}: exec #{} ctx {} engine {} batch {:#x}+{} -> seqno {}", open_id, n, ctx_id, eng, start, batch_len, seq);
        for &(h, pt, _, s) in &syncs {
            if s { let _ = crate::drm_device_interface::syncobj_install(h, open_id, pt, fence); }
        }
        return Ok(0);
    }
    // Fake engine: run the batch now, through the context's page tables,
    // with the logical context's own register file.
    let seq = SUBMITTED[eng].fetch_add(1, Ordering::AcqRel) + 1;
    let fence = fence_id(eng, seq);
    let start = batch_off + plan.batch_start as u64;
    let key = lrc_key(st.open(open_id)?.gen, ctx_id, &ctx, plan.engine);
    let out = {
        let State { vms, fake_regs, .. } = &mut *st;
        let vm = vms.get(&ctx.vm).ok_or(ENOENT)?;
        let mut mem = VmMem { pt: &vm.pt, regs: fake_regs.entry((eng, key)).or_default() };
        softcs::run(&mut mem, start, SOFTCS_LIMIT)
    };
    let ok = matches!(out, softcs::Outcome::Done(_));
    for &(obj, _, _) in &list {
        if let Some(b) = st.bos.get_mut(&obj) { b.last_fence.set(fence); }
    }
    if !ok {
        RESETS.fetch_add(1, Ordering::Relaxed);
        if let Ok(c) = st.ctx(open_id, ctx_id) {
            c.guilty += 1;
            if !c.recoverable { c.banned = true; }
        }
    }
    COMPLETED[eng].fetch_max(seq, Ordering::AcqRel);
    drop(st);
    exec_writeback(&e, &objs, &list, &patches);
    let n = EXECS.fetch_add(1, Ordering::Relaxed) + 1;
    if ok { ulog!(2, "open {}: exec #{} ctx {} {} batch {:#x}+{} -> {:?}", open_id, n, ctx_id, eng, batch_off, batch_len, out); }
    else { ulog!(1, "open {}: exec #{} ctx {} engine {} batch {:#x}: soft engine stopped: {:?} (counted as a GPU hang)", open_id, n, ctx_id, eng, start, out); }
    for &(h, pt, _, s) in &syncs {
        if s { let _ = crate::drm_device_interface::syncobj_install(h, open_id, pt, fence); }
    }
    crate::drm_device_interface::gpu_fence_irq_hint();
    Ok(0)
}

/// Tell userspace where things are, as Linux does: the canonical address of
/// every unpinned object the kernel placed somewhere other than its
/// `offset` said (exec_object.offset), and the presumed offset of every
/// relocation it wrote. A bad pointer here is ignored: the batch already ran.
fn exec_writeback(e: &Execbuffer2, objs: &[ExecObject2], list: &[(u32, u64, u64)], patches: &[RelocPatch]) {
    const OBJ_SZ: u64 = core::mem::size_of::<ExecObject2>() as u64;
    const OFFSET_AT: u64 = 24; // exec_object2.offset
    const PRESUMED_AT: u64 = 16; // relocation_entry.presumed_offset
    for (i, (o, &(_, a, _))) in objs.iter().zip(list).enumerate() {
        if o.flags & EXEC_OBJECT_PINNED != 0 || o.offset == canonical(a) { continue; }
        sched::uaccess::write_user((e.buffers_ptr + i as u64 * OBJ_SZ + OFFSET_AT) as usize, canonical(a));
    }
    for p in patches {
        let at = objs[p.obj].relocs_ptr + p.entry as u64 * core::mem::size_of::<RelocEntry>() as u64 + PRESUMED_AT;
        sched::uaccess::write_user(at as usize, p.presumed);
    }
}

/// Requests the execution core finished (poll thread, no core lock held).
pub(super) fn retire(evs: &[super::exec::Retired]) {
    let mut guilty = Vec::new();
    for r in evs {
        COMPLETED[r.eng].fetch_max(r.seq, Ordering::AcqRel);
        if let Some(k) = r.guilty { guilty.push((k, r.eng, r.seq)); }
    }
    if !guilty.is_empty() {
        let mut st = STATE.lock();
        for ((gen, c, _), eng, seq) in guilty {
            RESETS.fetch_add(1, Ordering::Relaxed);
            let mut banned = false;
            // The key names the open by its `gen`; a closed open matches nothing.
            let o = st.opens.iter().find(|(_, op)| op.gen == gen).map_or(0, |(&id, _)| id);
            if let Some(cx) = st.opens.get_mut(&o).and_then(|op| op.ctxs.get_mut(&c)) {
                cx.guilty += 1;
                if !cx.recoverable { cx.banned = true; banned = true; }
            }
            ulog!(1, "open {}: context {} request {} on engine {} hung and was reset{}", o, c, seq, eng,
                  if banned { " (context banned: not recoverable)" } else { "" });
        }
        st.reap();
    }
    crate::drm_device_interface::gpu_fence_irq_hint();
}

// ── mmap, PRIME, teardown ────────────────────────────────────────────────────

static MMAP_MISSES: AtomicU32 = AtomicU32::new(0);

/// L1.14: only for an open holding a handle on the token's BO (its own or a
/// PRIME import); another open's token is refused like an unknown one.
/// STATE is taken: 0x1007 runs without the DRM device lock.
fn mmap(token: u64, open_id: u32) -> Option<(u64, bool)> {
    if MMAP_TOKENS.lock().contains_key(&token) {
        let st = STATE.lock();
        let mine = st.opens.get(&open_id).map_or(false, |o| {
            o.handles.values().any(|obj| st.bos.get(obj).map_or(false, |b| b.phys as u64 == token))
        });
        return if mine { Some((token, false)) } else { None };
    }
    // Not ours. The DRM layer may still own it (dumb/blob); if not, the
    // kernel fails the mmap (it used to hand back a zero private mapping).
    if MMAP_MISSES.fetch_add(1, Ordering::Relaxed) < 16 {
        ulog!(1, "mmap token {:#x} is not an i915 token", token);
    }
    None
}

/// GEM_CLOSE: ours only if this open has the handle; anything else falls
/// through to the DRM layer (dumb/blob handles, or its unknown-handle answer).
fn gem_close(handle: u32, open_id: u32) -> Option<Result<usize, DriverError>> {
    let mut st = STATE.lock();
    let obj = st.opens.get_mut(&open_id).and_then(|o| o.handles.remove(&handle))?;
    st.bo_unref(obj, true);
    drop(st);
    ulog!(2, "open {}: GEM_CLOSE {:#x}", open_id, handle);
    Some(Ok(0))
}

fn prime_export(handle: u32, open_id: u32) -> Option<UapiPrime> {
    let mut st = STATE.lock();
    let obj = *st.opens.get(&open_id)?.handles.get(&handle)?;
    let b = st.bos.get_mut(&obj)?;
    b.exports += 1;
    b.prime = true;
    EXPORTS.fetch_add(1, Ordering::Relaxed);
    ulog!(2, "open {}: PRIME export {:#x} (obj {}, {} KiB)", open_id, handle, obj, b.size >> 10);
    Some(UapiPrime { phys: b.phys as usize, order: b.order, pages: (b.size / 4096) as usize, len: b.size as usize, obj })
}

fn prime_import(obj: u32, open_id: u32) -> Option<u32> {
    if open_id == 0 { return None; }
    let mut st = STATE.lock();
    if st.bos.get(&obj).map_or(true, |b| b.dead) { return None; }
    let o = st.open(open_id).ok()?;
    if let Some((&h, _)) = o.handles.iter().find(|(_, &v)| v == obj) {
        if let Some(b) = st.bos.get_mut(&obj) { b.prime = true; }
        return Some(h);
    }
    let h = alloc_handle(o);
    o.handles.insert(h, obj);
    let b = st.bos.get_mut(&obj)?;
    b.handles += 1;
    b.prime = true;
    IMPORTS.fetch_add(1, Ordering::Relaxed);
    ulog!(2, "open {}: PRIME import obj {} -> handle {:#x}", open_id, obj, h);
    Some(h)
}

fn bo_release(obj: u32) -> bool {
    let mut st = STATE.lock();
    if !st.bos.contains_key(&obj) { return false; }
    st.bo_unref(obj, false);
    true
}

// ── accounting: the periodic stat line and the test-only GETPARAMs ──────────

/// i915 ioctls that failed with ENOMEM/ENOSPC since boot.
static ENOMEMS: AtomicU64 = AtomicU64::new(0);
/// Last failing ioctl that was not a routine probe answer: `nr << 32 | errno`.
static LAST_ERR: AtomicU64 = AtomicU64::new(0);
/// PRIME exports, imports and GEM_CREATEs since boot (for the trend line).
static CREATES: AtomicU64 = AtomicU64::new(0);
static EXPORTS: AtomicU64 = AtomicU64::new(0);
static IMPORTS: AtomicU64 = AtomicU64::new(0);
static STAT_LAST_NS: AtomicU64 = AtomicU64::new(0);
/// The VFS tmpfs-pool census, registered by `servers/drm` (this crate does
/// not depend on the VFS): `[used, cap, hwm, anon, memfd, dmabuf, shared, full, orphan]`.
static TMPFS_CENSUS: AtomicUsize = AtomicUsize::new(0);

pub fn set_tmpfs_census(f: fn() -> Option<[u64; 9]>) { TMPFS_CENSUS.store(f as usize, Ordering::Release); }

fn tmpfs_census() -> Option<[u64; 9]> {
    let f = TMPFS_CENSUS.load(Ordering::Acquire);
    if f == 0 { return None; }
    let f: fn() -> Option<[u64; 9]> = unsafe { core::mem::transmute(f) };
    f()
}

/// Private GETPARAM numbers (outside Linux's range; Mesa never asks), all
/// scoped to the CALLING open so other clients of the device do not move
/// them: 1 objects it created that are not yet freed (closed-but-busy and
/// exported ones included), 2 their buddy-allocated KiB, 3 how many of them
/// are zombies (waiting on a fence), 4 KiB bound in its VMs, 5 its VMs.
/// Global: 6 tmpfs orphans (nameless slots no fd or queued fd refers to:
/// always 0 unless something leaked), 7 tmpfs slots in use, 8 opens of this
/// device. Read by i915test's churn case.
const LEANDROS_PARAM_BASE: i32 = 0x4C45_0000;

#[derive(Clone, Copy, Default)]
struct Snap { objs: u64, kib: u64, req_kib: u64, zombies: u64, bound_kib: u64, vms: u64, dead_vms: u64, opens: u64,
              handles: u64, exports: u64 }

fn snap() -> Snap {
    let st = STATE.lock();
    let mut s = Snap { objs: st.bos.len() as u64, zombies: st.zombies.len() as u64, vms: st.vms.len() as u64,
                       dead_vms: st.dead_vms.len() as u64, opens: st.opens.len() as u64, ..Default::default() };
    for b in st.bos.values() {
        s.kib += 4u64 << b.order;
        s.req_kib += b.size >> 10;
        s.handles += b.handles as u64;
        s.exports += b.exports as u64;
    }
    for v in st.vms.values() { s.bound_kib += v.map.bound_bytes() >> 10; }
    s
}

fn leandros_param(p: i32, open_id: u32) -> Option<i32> {
    if p & !0xFF != LEANDROS_PARAM_BASE { return None; }
    let n = p & 0xFF;
    let v = match n {
        1..=5 | 8 => {
            let st = STATE.lock();
            let o = st.opens.get(&open_id);
            let gen = o.map_or(u32::MAX, |o| o.gen);
            let mine = st.bos.values().filter(|b| b.creator == gen);
            match n {
                1 => mine.count() as u64,
                2 => mine.map(|b| 4u64 << b.order).sum(),
                3 => mine.filter(|b| b.dead).count() as u64,
                4 | 5 => {
                    let keys: Vec<u32> = o.map_or(Vec::new(), |o| {
                        let mut k: Vec<u32> = o.ctxs.values().map(|c| c.vm).chain(o.vm_ids.values().copied()).collect();
                        k.sort_unstable();
                        k.dedup();
                        k
                    });
                    if n == 5 { keys.len() as u64 } else {
                        keys.iter().filter_map(|k| st.vms.get(k)).map(|v| v.map.bound_bytes() >> 10).sum()
                    }
                }
                _ => st.opens.len() as u64,
            }
        }
        6 => tmpfs_census()?[8],
        7 => tmpfs_census()?[0],
        _ => return None,
    };
    Some(v.min(i32::MAX as u64) as i32)
}

/// One `[i915] stat` line every 10 s while the device is in use (driven by
/// ioctl traffic, so an idle device prints nothing). The trend across lines
/// is the point: objects/MiB/VA that only grow is a leak, `tmpfs` near its
/// cap is the global memfd/dma-buf pool, `enomem` and `last` name the first
/// ioctl that ran out of something. `i915.stat_s=0` turns it off.
fn stat_tick() {
    let period = super::opt("i915.stat_s").unwrap_or(10);
    if period == 0 { return; }
    let now = super::now_ns();
    let last = STAT_LAST_NS.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < period * 1_000_000_000 { return; }
    if STAT_LAST_NS.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() { return; }
    if last == 0 { return; } // first ioctl: arm only
    let s = snap();
    let le = LAST_ERR.load(Ordering::Relaxed);
    let free_mib = (mm::buddy::free_pages() as u64 * 4096) >> 20;
    let big = mm::buddy::largest_free_order().map_or(alloc::string::String::from("?"), |o| alloc::format!("{}", 4u64 << o));
    let tmp = match tmpfs_census() {
        Some(c) => alloc::format!("tmpfs {}/{} (hwm {}, anon {}, orphan {}, memfd {}, dmabuf {}, shared {}, full {})",
                                  c[0], c[1], c[2], c[3], c[8], c[4], c[5], c[6], c[7]),
        None => alloc::string::String::from("tmpfs busy"),
    };
    ilog!("stat: objs {} ({} MiB, req {} MiB) zombies {} handles {} exports {} | vms {}+{} dead, VA {} MiB | opens {} | \
           +create {} +export {} +import {} | enomem {} errors {} last {} | free {} MiB, max block {} KiB | {}",
          s.objs, s.kib >> 10, s.req_kib >> 10, s.zombies, s.handles, s.exports, s.vms, s.dead_vms, s.bound_kib >> 10,
          s.opens, CREATES.swap(0, Ordering::Relaxed), EXPORTS.swap(0, Ordering::Relaxed),
          IMPORTS.swap(0, Ordering::Relaxed), ENOMEMS.load(Ordering::Relaxed), ERRORS.load(Ordering::Relaxed),
          if le == 0 { alloc::string::String::from("-") } else { alloc::format!("{:#x}/-{}", le >> 32, le as u32) },
          free_mib, big, tmp);
}

/// Should a re-export of `obj` reuse its live dma-buf (Linux: one dma_buf per
/// GEM object)? Only for i915 objects; `i915.primededup=0` mints a new one
/// per export again (bisect).
pub fn prime_dedup(obj: u32) -> bool {
    super::opt("i915.primededup").unwrap_or(1) != 0 && STATE.lock().bos.get(&obj).map_or(false, |b| !b.dead)
}

fn release_open(open_id: u32) {
    let mut st = STATE.lock();
    let Some(o) = st.opens.remove(&open_id) else { return };
    let (nh, nc, gen) = (o.handles.len(), o.ctxs.len(), o.gen);
    for (_, c) in o.ctxs { st.vm_unref(c.vm); }
    for (_, key) in o.vm_ids { st.vm_unref(key); }
    for (_, obj) in o.handles { st.bo_unref(obj, true); }
    let live = st.bos.len();
    st.fake_regs.retain(|&(_, k), _| k.0 != gen);
    drop(st);
    super::exec::release(|k| k.0 == gen);
    ulog!(2, "open {}: closed ({} handles, {} contexts; {} objects live)", open_id, nh, nc, live);
}

// ── sysfs identity ───────────────────────────────────────────────────────────

fn put(out: &mut [u8], s: &[u8]) -> usize {
    let n = s.len().min(out.len());
    out[..n].copy_from_slice(&s[..n]);
    n
}

fn sysfs_attr(attr: &[u8], out: &mut [u8]) -> Option<usize> {
    let g = INFO.lock();
    let i = g.as_ref()?;
    let (ven, dev, sv, sd, rev, class) = (0x8086u16, i.facts.device_id, i.subsys.0, i.subsys.1, i.revision, 0x030000u32);
    let (b, d, f) = i.slot;
    let s = match attr {
        b"vendor" => alloc::format!("0x{:04x}\n", ven),
        b"device" => alloc::format!("0x{:04x}\n", dev),
        b"subsystem_vendor" => alloc::format!("0x{:04x}\n", sv),
        b"subsystem_device" => alloc::format!("0x{:04x}\n", sd),
        b"revision" => alloc::format!("0x{:02x}\n", rev),
        b"class" => alloc::format!("0x{:06x}\n", class),
        b"uevent" => alloc::format!(
            "DRIVER=i915\nPCI_CLASS={:X}\nPCI_ID={:04X}:{:04X}\nPCI_SUBSYS_ID={:04X}:{:04X}\nPCI_SLOT_NAME=0000:{:02x}:{:02x}.{}\nMODALIAS=pci:v{:08X}d{:08X}sv{:08X}sd{:08X}bc{:02X}sc{:02X}i{:02X}\n",
            class, ven, dev, sv, sd, b, d, f, ven, dev, sv, sd, class >> 16, (class >> 8) & 0xff, class & 0xff),
        b"config" => {
            let mut c = [0u8; 64];
            c[0..2].copy_from_slice(&ven.to_le_bytes());
            c[2..4].copy_from_slice(&dev.to_le_bytes());
            c[8] = rev;
            c[11] = 0x03;
            c[0x2c..0x2e].copy_from_slice(&sv.to_le_bytes());
            c[0x2e..0x30].copy_from_slice(&sd.to_le_bytes());
            return Some(put(out, &c));
        }
        _ => return None,
    };
    Some(put(out, s.as_bytes()))
}

static BACKEND: UapiBackend = UapiBackend {
    name: "i915",
    version: (1, 6, 0),
    date: "20230929",
    desc: "Intel Graphics (LeandrOS i915 UAPI)",
    ioctl,
    user_args: true,
    gem_close,
    mmap,
    release_open,
    prime_export,
    prime_import,
    bo_release,
    fence_done,
    sysfs_attr,
};

/// The default topology of a KBL GT2 (1 slice, 3 subslices, 24 EUs) for the
/// fake device or a probe that could not read the fuses.
pub(crate) fn kbl_gt2_sseu() -> Sseu { hw::gen9_sseu(0x0080_0000 | 1 << 25, [0; 3], false) }

/// Register the backend: card0/renderD128 answer `i915` from now on.
pub(crate) fn register(d: &super::Device, mode: Mode, sseu: Sseu, cs_hz: u32) {
    let exec = mode == Mode::Hw && super::exec::ready();
    // vcs0: on the soft engine always (it runs MI commands on any ring); on
    // hardware only when its bring-up and self-test passed. Otherwise it
    // stays hidden: HAS_BSD=0, not in ENGINE_INFO, EXEC_BSD -> EINVAL.
    let vcs = mode == Mode::Fake || (exec && super::exec::engine_ok(VCS0));
    EXPOSED.store(0b011 | (vcs as u32) << VCS0, Ordering::Relaxed);
    if let Some(v) = super::opt("i915.uapi_log") { LOG.store(v as u32, Ordering::Relaxed); }
    if let Some(v) = super::opt("i915.ptecheck") { PTECHECK.store(v as u32, Ordering::Relaxed); }
    let (slot, subsys, revision) = if d.fake {
        ((0, 2, 0), (0x8086, 0x2212), 7)
    } else {
        let f = d.func;
        ((f.bus, f.dev, f.func), (f.read16(0x2c), f.read16(0x2e)), f.read8(0x08))
    };
    let facts = Facts {
        device_id: d.platform.device, revision, has_llc: d.platform.has_llc(), sseu, cs_timestamp_hz: cs_hz,
        engines: exposed(), ram_total: 0, ram_free: 0, aperture: d.gmadr_size,
        exec_fence: false, exec_fence_array: true, timeline_fences: true,
    };
    *INFO.lock() = Some(Info { facts, mode, mmio: d.mmio.va, slot, subsys, revision });
    MODE_FAKE.store((mode == Mode::Fake) as u32, Ordering::Relaxed);
    crate::gpu_uapi::register(&BACKEND);
    ilog!("==== G3 UAPI registered: /dev/dri/card0 + renderD128 answer 'i915' 1.6.0 for 8086:{:04x} rev {:02x} at {:02x}:{:02x}.{}; \
           engines rcs0+bcs0{}, {} EUs, CS timestamp {} Hz; execution: {} ====",
          d.platform.device, revision, slot.0, slot.1, slot.2, if vcs { "+vcs0" } else { "" }, sseu.eu_total, cs_hz,
          match (mode, exec) {
              (Mode::Fake, _) => "soft command streamer (i915.fake)",
              (Mode::Hw, true) => "execlists on rcs0+bcs0 (i915.exec=1)",
              (Mode::Hw, false) => "off (EXECBUFFER2 -> ENODEV; i915.exec=1 enables it)",
          });
}

// ── KMS (G4a) helpers: scanout framebuffers hold an export reference ────────

/// What `kms` needs of a GEM object it wraps in a framebuffer.
pub(super) struct KmsBo { pub obj: u32, pub phys: u64, pub size: u64, pub tiling: u32 }

/// `None`: `handle` is not one of this open's i915 handles (the DRM layer's
/// dumb/blob paths take it). Otherwise the object, with an export reference
/// taken (dropped by `kms_bo_put`).
pub(super) fn kms_bo_get(open_id: u32, handle: u32) -> Option<KmsBo> {
    let mut st = STATE.lock();
    let obj = st.obj_of(open_id, handle).ok()?;
    let b = st.bos.get_mut(&obj)?;
    if b.dead { return None; }
    b.exports += 1;
    let (phys, size, tiling) = (b.phys, b.size, b.tiling);
    if super::opt("i915.kms_uncached").unwrap_or(1) != 0 && !b.scanout_uc {
        // Rebind it uncached wherever it is already bound. Every batch
        // starts with a PIPE_CONTROL TLB invalidate; the flip's
        // clflush still writes back whatever an earlier cached write left
        // in the LLC.
        b.scanout_uc = true;
        // Linux marks the object cache-dirty when it goes I915_CACHE_NONE
        // and clflushes it before the next use: whatever is still dirty in
        // the CPU caches/LLC (the zeroing at GEM_CREATE, earlier cached GPU
        // writes) is written back now, once, rather than evicted later on
        // top of what the GPU writes uncached.
        super::kms::flush_for_display(phys, b.size);
        let State { vms, .. } = &mut *st;
        for vm in vms.values_mut() {
            if let Some((start, len)) = vm.map.find(obj) {
                let _ = vm.pt.map_range(&mut KStore, start, phys, len, Cache::Uncached);
            }
        }
    }
    Some(KmsBo { obj, phys, size, tiling })
}

pub(super) fn kms_bo_put(obj: u32) { let _ = bo_release(obj); }

/// Does a flip of `obj` need the CPU clflush? `i915.kms_clflush`: 1 always,
/// 0 never, 3 the old PRIME rule (bound uncached AND shared through PRIME),
/// default (2) only when the object may hold cache-dirty lines the display
/// (which does not snoop the LLC) would miss:
/// * not bound uncached (`i915.kms_uncached=0`): GPU writes land in the LLC;
/// * a CPU mapping exists (`cpu_wb`; mappings are write-back), or a cached
///   CPU write (PWRITE, SET_DOMAIN CPU) happened since the last flip flush.
/// GPU rendering into a `scanout_uc` object needs nothing: Mesa allocates
/// every GBM/KMS-shareable buffer with BO_ALLOC_SHARED (gbm_dri always adds
/// __DRI_IMAGE_USE_SHARE; a handle handed out by EGL/Vulkan export marks it
/// exported too), so iris/anv render it with the PTE MOCS entry -> the
/// uncached PPGTT PTE, and the end-of-batch render-cache flush the fence
/// waits for writes it to memory. What was cached before it went uncached
/// is flushed once in `kms_bo_get`.
pub(super) fn kms_bo_needs_clflush(obj: u32) -> bool {
    let st = STATE.lock();
    let b = match st.bos.get(&obj) { Some(b) => b, None => return true };
    match super::opt("i915.kms_clflush").unwrap_or(2) {
        0 => false,
        1 => true,
        3 => !(b.scanout_uc && b.prime),
        _ => !b.scanout_uc || b.cpu_wb || b.cpu_dirty,
    }
}

/// The flip is about to clflush `obj`: one-shot CPU dirtiness is consumed
/// first (a write racing the flush marks it again for the next flip).
pub(super) fn kms_bo_take_dirty(obj: u32) {
    if let Some(b) = STATE.lock().bos.get_mut(&obj) { b.cpu_dirty = false; }
}

/// Implicit sync for a flip: wait (bounded) for the last request that used
/// the object. False on timeout.
pub(super) fn kms_bo_wait(obj: u32, timeout_ns: u64) -> bool {
    let f = STATE.lock().bos.get(&obj).map(|b| b.last_fence).unwrap_or_default();
    f.wait(timeout_ns).is_ok()
}

pub(super) fn is_fake() -> bool { mode() == Mode::Fake }
