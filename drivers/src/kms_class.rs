//! KMS class tables for loadable GPU drivers (kmod kapi, K2a).
//!
//! Mirrors `drm_class`: a module hands over `extern "C"` tables for the five KMS halves of
//! `gpu_kms` (backend, flip, cursor, outputs, second pipe) and `register_raw` installs static Rust
//! adapters that forward to them, so `drm_device_interface` stays unchanged. Slices / Options are
//! flattened (rect pointer + count, modifier has + value, info as an out-param bool).
//! Return conventions: `fb_create` gives `>= 0` token, `KMS_NOT_MINE`, or `-errno`; every `i32`
//! status is 0 ok / `-errno`. One table per kind, ever (no unregister).

use alloc::boxed::Box;
use alloc::string::String;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use crate::gpu_kms::{self, KmsBackend, KmsConnInfo, KmsCursor, KmsCursorSrc, KmsFbReq, KmsFlip, KmsOutputs, KmsPipe2, KmsTiming};
use crate::DriverError;

pub const KMS_NOT_MINE: i64 = i64::MIN;

pub const KIND_BACKEND: u32 = 0;
pub const KIND_FLIP: u32 = 1;
pub const KIND_CURSOR: u32 = 2;
pub const KIND_OUTPUTS: u32 = 3;
pub const KIND_PIPE2: u32 = 4;
/// Second pipe's own cursor table (`KmsPipe2::cursor`).
pub const KIND_CURSOR2: u32 = 5;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct KKmsFbReq {
    pub handle: u32, pub width: u32, pub height: u32, pub pitch: u32, pub offset: u32, pub fourcc: u32,
    pub has_modifier: bool, pub modifier: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct KKmsCursorSrc { pub token: u32, pub phys: u64, pub pitch: u32, pub width: u32, pub height: u32 }

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct KKmsTiming { pub clock_khz: u32, pub h: [u16; 4], pub v: [u16; 4], pub flags: u32 }

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct KKmsConnInfo {
    pub conn_type: u32, pub type_id: u32, pub enc_type: u32, pub connected: bool,
    pub mm_w: u32, pub mm_h: u32, pub modes: [KKmsTiming; 4], pub nmodes: usize,
    pub edid: [[u8; 16]; 8], pub edid_len: usize,
}

fn to_c_timing(t: &KmsTiming) -> KKmsTiming { KKmsTiming { clock_khz: t.clock_khz, h: t.h, v: t.v, flags: t.flags } }
fn from_c_timing(t: &KKmsTiming) -> KmsTiming { KmsTiming { clock_khz: t.clock_khz, h: t.h, v: t.v, flags: t.flags } }
fn from_c_info(i: &KKmsConnInfo) -> KmsConnInfo {
    let mut m = [KmsTiming::default(); 4];
    for k in 0..4 { m[k] = from_c_timing(&i.modes[k]); }
    KmsConnInfo { conn_type: i.conn_type, type_id: i.type_id, enc_type: i.enc_type, connected: i.connected,
                  mm_w: i.mm_w, mm_h: i.mm_h, modes: m, nmodes: i.nmodes.min(4), edid: i.edid, edid_len: i.edid_len.min(128) }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KKmsBackendOps {
    /// `size_of::<KKmsBackendOps>()`.
    pub size: u32,
    pub name: *const u8, pub name_len: usize,
    /// Fills `out[0..3]` = width, height, refresh Hz.
    pub mode: extern "C" fn(ctx: usize, out: *mut u32),
    pub modifiers: *const u64, pub n_modifiers: usize,
    pub fb_create: extern "C" fn(ctx: usize, req: *const KKmsFbReq, open_id: u32) -> i64,
    pub fb_destroy: extern "C" fn(ctx: usize, token: u32),
    /// `rects` = `n_rects` x [x1, y1, x2, y2], only read when `has_rects`.
    pub present: extern "C" fn(ctx: usize, token: u32, src_x: u32, src_y: u32, has_rects: bool, rects: *const [i32; 4], n_rects: usize) -> i32,
    pub console_restore: Option<extern "C" fn(ctx: usize)>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KKmsFlipOps {
    pub size: u32,
    pub queue: extern "C" fn(ctx: usize, token: u32, src_x: u32, src_y: u32, ev: u32) -> i32,
    pub event: extern "C" fn(ctx: usize, ev: u32),
    pub busy: extern "C" fn(ctx: usize) -> bool,
    pub wait_idle: extern "C" fn(ctx: usize, timeout_ns: u64) -> bool,
    // Added after the first release of this table: read only when `size`
    // covers them (an older module's table ends at `wait_idle`).
    pub queue_async: Option<extern "C" fn(ctx: usize, token: u32, src_x: u32, src_y: u32, ev: u32) -> i32>,
    /// `out[0]` = counter, `out[1]` = monotonic ns of the latest vblank.
    pub frame: Option<extern "C" fn(ctx: usize, out: *mut u64)>,
    /// Sleep until the counter reaches `target`; false on timeout, else `out` as `frame`.
    pub wait_frame: Option<extern "C" fn(ctx: usize, target: u32, timeout_ns: u64, out: *mut u64) -> bool>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KKmsCursorOps {
    pub size: u32,
    pub cursor_size: u32,
    pub set: extern "C" fn(ctx: usize, src: *const KKmsCursorSrc, x: i32, y: i32) -> bool,
    pub move_to: extern "C" fn(ctx: usize, x: i32, y: i32),
    pub hide: extern "C" fn(ctx: usize),
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KKmsOutputsOps {
    pub size: u32,
    pub count: extern "C" fn(ctx: usize) -> usize,
    pub info: extern "C" fn(ctx: usize, idx: usize, out: *mut KKmsConnInfo) -> bool,
    pub cached: extern "C" fn(ctx: usize, idx: usize, out: *mut KKmsConnInfo) -> bool,
    pub crtc_mask: extern "C" fn(ctx: usize, idx: usize) -> u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KKmsPipe2Ops {
    pub size: u32,
    pub name: *const u8, pub name_len: usize,
    pub modeset: extern "C" fn(ctx: usize, conn_idx: usize, t: *const KKmsTiming) -> i32,
    pub disable: extern "C" fn(ctx: usize),
    pub queue: extern "C" fn(ctx: usize, token: u32, src_x: u32, src_y: u32, ev: u32) -> i32,
    pub event: extern "C" fn(ctx: usize, ev: u32),
    pub busy: extern "C" fn(ctx: usize) -> bool,
    pub wait_idle: extern "C" fn(ctx: usize, timeout_ns: u64) -> bool,
    /// Fills `out[0..3]`; false = pipe off.
    pub mode: extern "C" fn(ctx: usize, out: *mut u32) -> bool,
    pub dumb_ok: bool,
    /// Register the cursor half with `KIND_CURSOR2` BEFORE this table to expose pipe B's cursor plane.
    pub has_cursor: bool,
}

unsafe impl Send for KKmsBackendOps {} unsafe impl Sync for KKmsBackendOps {}
unsafe impl Send for KKmsPipe2Ops {} unsafe impl Sync for KKmsPipe2Ops {}

macro_rules! slot { ($t:ident, $c:ident, $ty:ty) => {
    static $t: AtomicPtr<$ty> = AtomicPtr::new(core::ptr::null_mut());
    static $c: AtomicUsize = AtomicUsize::new(0);
}; }
slot!(BE_T, BE_C, KKmsBackendOps);
slot!(FL_T, FL_C, KKmsFlipOps);
slot!(OU_T, OU_C, KKmsOutputsOps);
slot!(P2_T, P2_C, KKmsPipe2Ops);
// Cursor: slot 0 = pipe A, 1 = pipe B.
static CU_T: [AtomicPtr<KKmsCursorOps>; 2] = [AtomicPtr::new(core::ptr::null_mut()), AtomicPtr::new(core::ptr::null_mut())];
static CU_C: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];

macro_rules! tb { ($t:ident, $c:ident) => { (unsafe { &*$t.load(Ordering::Acquire) }, $c.load(Ordering::Acquire)) }; }
fn errno(r: i32) -> Result<(), DriverError> { if r >= 0 { Ok(()) } else { Err(DriverError::Errno(-r)) } }

// backend
fn b_mode() -> (u32, u32, u32) { let (t, c) = tb!(BE_T, BE_C); let mut o = [0u32; 3]; (t.mode)(c, o.as_mut_ptr()); (o[0], o[1], o[2]) }
fn b_fb_create(r: &KmsFbReq, open_id: u32) -> Option<Result<u32, DriverError>> {
    let (t, c) = tb!(BE_T, BE_C);
    let q = KKmsFbReq { handle: r.handle, width: r.width, height: r.height, pitch: r.pitch, offset: r.offset, fourcc: r.fourcc,
                        has_modifier: r.modifier.is_some(), modifier: r.modifier.unwrap_or(0) };
    match (t.fb_create)(c, &q, open_id) {
        KMS_NOT_MINE => None,
        v if v >= 0 => Some(Ok(v as u32)),
        v => Some(Err(DriverError::Errno((-v).min(i32::MAX as i64) as i32))),
    }
}
fn b_fb_destroy(tok: u32) { let (t, c) = tb!(BE_T, BE_C); (t.fb_destroy)(c, tok) }
fn b_present(tok: u32, sx: u32, sy: u32, rects: Option<&[(i32, i32, i32, i32)]>) -> Result<(), DriverError> {
    let (t, c) = tb!(BE_T, BE_C);
    let flat: alloc::vec::Vec<[i32; 4]> = rects.map_or(alloc::vec::Vec::new(), |r| r.iter().map(|&(a, b, c2, d)| [a, b, c2, d]).collect());
    errno((t.present)(c, tok, sx, sy, rects.is_some(), flat.as_ptr(), flat.len()))
}
fn b_console_restore() { let (t, c) = tb!(BE_T, BE_C); if let Some(f) = t.console_restore { f(c) } }

// flip
fn f_queue(tok: u32, sx: u32, sy: u32, ev: u32) -> Result<(), DriverError> { let (t, c) = tb!(FL_T, FL_C); errno((t.queue)(c, tok, sx, sy, ev)) }
fn f_event(ev: u32) { let (t, c) = tb!(FL_T, FL_C); (t.event)(c, ev) }
fn f_busy() -> bool { let (t, c) = tb!(FL_T, FL_C); (t.busy)(c) }
fn f_wait(ns: u64) -> bool { let (t, c) = tb!(FL_T, FL_C); (t.wait_idle)(c, ns) }
const FL_V1: usize = core::mem::offset_of!(KKmsFlipOps, queue_async);
fn f_queue_async(tok: u32, sx: u32, sy: u32, ev: u32) -> Result<(), DriverError> {
    let (t, c) = tb!(FL_T, FL_C);
    match t.queue_async { Some(f) if t.size as usize > FL_V1 => errno(f(c, tok, sx, sy, ev)), _ => Err(DriverError::Errno(22)) }
}
fn f_frame() -> (u32, u64) {
    let (t, c) = tb!(FL_T, FL_C);
    let mut o = [0u64; 2];
    if let Some(f) = t.frame { f(c, o.as_mut_ptr()); }
    (o[0] as u32, o[1])
}
fn f_wait_frame(target: u32, ns: u64) -> Option<(u32, u64)> {
    let (t, c) = tb!(FL_T, FL_C);
    let mut o = [0u64; 2];
    match t.wait_frame { Some(f) if f(c, target, ns, o.as_mut_ptr()) => Some((o[0] as u32, o[1])), _ => None }
}

// cursor (const generic = slot)
fn cu_set<const N: usize>(s: &KmsCursorSrc, x: i32, y: i32) -> bool {
    let (t, c) = (unsafe { &*CU_T[N].load(Ordering::Acquire) }, CU_C[N].load(Ordering::Acquire));
    let q = KKmsCursorSrc { token: s.token, phys: s.phys, pitch: s.pitch, width: s.width, height: s.height };
    (t.set)(c, &q, x, y)
}
fn cu_move<const N: usize>(x: i32, y: i32) { (unsafe { &*CU_T[N].load(Ordering::Acquire) }.move_to)(CU_C[N].load(Ordering::Acquire), x, y) }
fn cu_hide<const N: usize>() { (unsafe { &*CU_T[N].load(Ordering::Acquire) }.hide)(CU_C[N].load(Ordering::Acquire)) }

// outputs
fn o_count() -> usize { let (t, c) = tb!(OU_T, OU_C); (t.count)(c) }
fn o_info(i: usize) -> Option<KmsConnInfo> { let (t, c) = tb!(OU_T, OU_C); let mut o = KKmsConnInfo::default(); if (t.info)(c, i, &mut o) { Some(from_c_info(&o)) } else { None } }
fn o_cached(i: usize) -> Option<KmsConnInfo> { let (t, c) = tb!(OU_T, OU_C); let mut o = KKmsConnInfo::default(); if (t.cached)(c, i, &mut o) { Some(from_c_info(&o)) } else { None } }
fn o_mask(i: usize) -> u32 { let (t, c) = tb!(OU_T, OU_C); (t.crtc_mask)(c, i) }

// pipe2
fn p_modeset(i: usize, t2: &KmsTiming) -> Result<(), DriverError> { let (t, c) = tb!(P2_T, P2_C); errno((t.modeset)(c, i, &to_c_timing(t2))) }
fn p_disable() { let (t, c) = tb!(P2_T, P2_C); (t.disable)(c) }
fn p_queue(tok: u32, sx: u32, sy: u32, ev: u32) -> Result<(), DriverError> { let (t, c) = tb!(P2_T, P2_C); errno((t.queue)(c, tok, sx, sy, ev)) }
fn p_event(ev: u32) { let (t, c) = tb!(P2_T, P2_C); (t.event)(c, ev) }
fn p_busy() -> bool { let (t, c) = tb!(P2_T, P2_C); (t.busy)(c) }
fn p_wait(ns: u64) -> bool { let (t, c) = tb!(P2_T, P2_C); (t.wait_idle)(c, ns) }
fn p_mode() -> Option<(u32, u32, u32)> { let (t, c) = tb!(P2_T, P2_C); let mut o = [0u32; 3]; if (t.mode)(c, o.as_mut_ptr()) { Some((o[0], o[1], o[2])) } else { None } }

static BE: spin::Once<KmsBackend> = spin::Once::new();
static FL: spin::Once<KmsFlip> = spin::Once::new();
static CU: [spin::Once<KmsCursor>; 2] = [spin::Once::new(), spin::Once::new()];
static OU: spin::Once<KmsOutputs> = spin::Once::new();
static P2: spin::Once<KmsPipe2> = spin::Once::new();

fn leak_str(p: *const u8, n: usize) -> &'static str {
    if p.is_null() { return ""; }
    Box::leak(String::from_utf8_lossy(unsafe { core::slice::from_raw_parts(p, n) }).into_owned().into_boxed_str())
}

fn claim<T>(slot: &AtomicPtr<T>, p: *const T) -> bool {
    slot.compare_exchange(core::ptr::null_mut(), p as *mut T, Ordering::AcqRel, Ordering::Acquire).is_ok()
}
fn size_ok<T>(p: *const u8, size: u32) -> bool { !p.is_null() && (size as usize) >= core::mem::size_of::<T>() }

/// Install a module's KMS table of `kind`. 0 ok, -22 bad kind/table, -17 that kind is already
/// registered (a built-in backend counts for `KIND_BACKEND`).
pub fn register_raw(kind: u32, ops: *const u8, ctx: usize) -> i32 {
    if ops.is_null() { return -22; }
    macro_rules! get { ($ty:ty) => {{ let o = unsafe { &*(ops as *const $ty) }; if !size_ok::<$ty>(ops, o.size) { return -22; } o }}; }
    match kind {
        KIND_BACKEND => {
            let o = get!(KKmsBackendOps);
            if o.name.is_null() || o.name_len == 0 { return -22; }
            if gpu_kms::active() { return -17; }
            if !claim(&BE_T, o as *const _) { return -17; }
            BE_C.store(ctx, Ordering::Release);
            let b = BE.call_once(|| KmsBackend {
                name: leak_str(o.name, o.name_len), mode: b_mode,
                modifiers: if o.modifiers.is_null() { &[] } else { unsafe { core::slice::from_raw_parts(o.modifiers, o.n_modifiers) } },
                fb_create: b_fb_create, fb_destroy: b_fb_destroy, present: b_present,
                console_restore: if o.console_restore.is_some() { Some(b_console_restore) } else { None },
            });
            gpu_kms::register(b);
        }
        KIND_FLIP => {
            let o = get!(KKmsFlipOps);
            if !claim(&FL_T, o as *const _) { return -17; }
            FL_C.store(ctx, Ordering::Release);
            gpu_kms::register_flip(FL.call_once(|| {
                let v = o.size as usize > FL_V1;
                KmsFlip { queue: f_queue, event: f_event, busy: f_busy, wait_idle: f_wait,
                    queue_async: if v && o.queue_async.is_some() { Some(f_queue_async) } else { None },
                    frame: if v && o.frame.is_some() { Some(f_frame) } else { None },
                    wait_frame: if v && o.wait_frame.is_some() { Some(f_wait_frame) } else { None } }
            }));
        }
        KIND_CURSOR | KIND_CURSOR2 => {
            let n = (kind == KIND_CURSOR2) as usize;
            let o = get!(KKmsCursorOps);
            if !claim(&CU_T[n], o as *const _) { return -17; }
            CU_C[n].store(ctx, Ordering::Release);
            let cur = CU[n].call_once(|| if n == 0 {
                KmsCursor { size: o.cursor_size, set: cu_set::<0>, move_to: cu_move::<0>, hide: cu_hide::<0> }
            } else {
                KmsCursor { size: o.cursor_size, set: cu_set::<1>, move_to: cu_move::<1>, hide: cu_hide::<1> }
            });
            if n == 0 { gpu_kms::register_cursor(cur); }
        }
        KIND_OUTPUTS => {
            let o = get!(KKmsOutputsOps);
            if !claim(&OU_T, o as *const _) { return -17; }
            OU_C.store(ctx, Ordering::Release);
            gpu_kms::register_outputs(OU.call_once(|| KmsOutputs { count: o_count, info: o_info, cached: o_cached, crtc_mask: o_mask }));
        }
        KIND_PIPE2 => {
            let o = get!(KKmsPipe2Ops);
            if o.name.is_null() || o.name_len == 0 { return -22; }
            if !claim(&P2_T, o as *const _) { return -17; }
            P2_C.store(ctx, Ordering::Release);
            let cursor = if o.has_cursor && CU[1].get().is_some() { CU[1].get() } else { None };
            gpu_kms::register_pipe2(P2.call_once(|| KmsPipe2 {
                name: leak_str(o.name, o.name_len), modeset: p_modeset, disable: p_disable, queue: p_queue, event: p_event,
                busy: p_busy, wait_idle: p_wait, cursor, mode: p_mode, dumb_ok: o.dumb_ok,
            }));
        }
        _ => return -22,
    }
    0
}

/// Whether a module table of `kind` is installed.
pub fn registered(kind: u32) -> bool {
    match kind {
        KIND_BACKEND => !BE_T.load(Ordering::Acquire).is_null(),
        KIND_FLIP => !FL_T.load(Ordering::Acquire).is_null(),
        KIND_CURSOR => !CU_T[0].load(Ordering::Acquire).is_null(),
        KIND_CURSOR2 => !CU_T[1].load(Ordering::Acquire).is_null(),
        KIND_OUTPUTS => !OU_T.load(Ordering::Acquire).is_null(),
        KIND_PIPE2 => !P2_T.load(Ordering::Acquire).is_null(),
        _ => false,
    }
}
