//! Hardware cursor on pipe A (lane kmscursor): CUR_CTL / CUR_BASE / CUR_POS,
//! as Linux's i9xx_cursor (skl+ flavour) for gen9.
//!
//! Without it the compositor draws the pointer into the primary plane, so
//! every pointer move is a full compose + flip. With it a move is CUR_POS +
//! CUR_BASE (the arm) — two MMIO writes from the atomic commit, no render
//! fence involved.
//!
//! * Image: 64x64 ARGB8888 (MCURSOR_MODE_64_ARGB_AX), copied from the
//!   framebuffer the commit names (a dumb buffer — what GBM gives a
//!   CURSOR|WRITE bo — or an i915 object) into one of two kernel buffers
//!   bound in the GGTT right above the scanout window (4 KiB aligned, which is
//!   what gen9 cursors need), clflushed (the display engine does not snoop
//!   the LLC). The buffers alternate so a new image never overwrites the one
//!   being scanned; the copy happens only when the commit names a different
//!   framebuffer (the DRM layer's `LAST_CURSOR_FB`).
//! * DDB: the cursor needs its own display-buffer blocks (CUR_BUF_CFG). The
//!   GOP leaves plane 1A with part of the 892-block DDB; the cursor takes 32
//!   free blocks (Linux's single-pipe cursor allocation) from the top, never
//!   overlapping a live plane's. No room: no hardware cursor.
//! * Watermarks (CUR_WM 0..7): per level, plane 1A's current one capped at
//!   the cursor's DDB. Plane 1A's method-1 result (latency x bandwidth) is the
//!   cursor's too and its method-2 result (latency lines x blocks per line)
//!   is larger, so the plane's watermark bounds the cursor's from above
//!   (safe against underruns) and keeps every level the plane has.
//! * Visibility: fully off the pipe means disabled (Linux's clipped-plane
//!   rule); partly off the top/left uses the sign bits of CUR_POS.
//! * `console_hide` (lock-free, from `kms::console_restore`) turns it off;
//!   the next move or image shows it again.
//!
//! `i915.hwcursor=0`: no cursor plane is exposed (the compositor composites).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::kenv::sched::ksync::KSpinLock;

use crate::kenv::kms::{KmsCursor, KmsCursorSrc};

use super::hw::{self, reg};
use super::kms;
use super::{ilog, log, opt, Device};

const CURCNTR: u32 = 0x70080;
const CURBASE: u32 = 0x70084;
const CURPOS: u32 = 0x70088;
const CURSURFLIVE: u32 = 0x700AC;
const CUR_WM: u32 = 0x70140;
const CUR_WM_TRANS: u32 = 0x70168;
const CUR_BUF_CFG: u32 = 0x7017C;
/// MCURSOR_MODE_64_ARGB_AX, MCURSOR_PIPE_GAMMA_ENABLE, MCURSOR_PIPE_CSC_ENABLE.
const MODE_64_ARGB: u32 = 0x27;
const CUR_GAMMA: u32 = 1 << 26;
const CUR_CSC: u32 = 1 << 24;
/// PLANE_CTL pipe gamma / pipe CSC enable (skl..kbl).
const PLANE_GAMMA: u32 = 1 << 30;
const PLANE_CSC: u32 = 1 << 23;
const PIPECONF_A: u32 = 0x70008;

const SIZE: u32 = 64;
const BYTES: usize = (SIZE * SIZE * 4) as usize;
/// gen9: 896 blocks, the last 4 reserved.
const DDB_BLOCKS: u32 = 892;
const CUR_DDB: u32 = 32;

struct Cur {
    /// Which buffer CURBASE points at (or will at the next arm).
    active: usize,
    /// An image is loaded.
    loaded: bool,
    x: i32,
    y: i32,
}

static CUR: KSpinLock<Cur> = KSpinLock::new(Cur { active: 0, loaded: false, x: 0, y: 0 });
/// CUR_CTL is enabled in hardware (cleared lock-free by `console_hide`).
static SHOWN: AtomicBool = AtomicBool::new(false);
static ENABLED: AtomicBool = AtomicBool::new(false);
static BUF_PHYS: AtomicU64 = AtomicU64::new(0);
/// CUR_BUF_CFG value (start | end << 16, end inclusive).
static DDB: AtomicU32 = AtomicU32::new(0);
static SETS: AtomicU64 = AtomicU64::new(0);
static MOVES: AtomicU64 = AtomicU64::new(0);
static LOGGED: AtomicU32 = AtomicU32::new(0);

fn buf_phys(i: usize) -> u64 { BUF_PHYS.load(Ordering::Acquire) + (i * BYTES) as u64 }
fn buf_ggtt(i: usize) -> u32 { (kms::CURSOR_GGTT + (i * BYTES) as u64) as u32 }

/// CUR_POS: sign-magnitude per axis (bit 15 / bit 31 = negative).
fn pos(x: i32, y: i32) -> u32 {
    let mut v = 0;
    if x < 0 { v |= 1 << 15; }
    v |= x.unsigned_abs() & 0x7FF;
    if y < 0 { v |= 1 << 31; }
    v |= (y.unsigned_abs() & 0x7FF) << 16;
    v
}

fn visible(x: i32, y: i32) -> bool {
    let (mw, mh, _) = kms::mode();
    x < mw as i32 && y < mh as i32 && x > -(SIZE as i32) && y > -(SIZE as i32)
}

/// Watermarks and DDB, then enable — or disable — and arm (CURBASE last).
/// Under `CUR`.
fn program(c: &Cur) {
    let m = kms::mmio();
    if m.va == 0 { return; }
    let show = c.loaded && visible(c.x, c.y);
    if show {
        if !SHOWN.load(Ordering::Acquire) {
            write_wm(&m);
        }
        let pctl = m.r(kms::preg(reg::PLANE_CTL_1_A));
        let mut ctl = MODE_64_ARGB;
        if pctl & PLANE_GAMMA != 0 { ctl |= CUR_GAMMA; }
        if pctl & PLANE_CSC != 0 { ctl |= CUR_CSC; }
        m.w(CURPOS, pos(c.x, c.y));
        m.w(CURCNTR, ctl);
        m.w(CURBASE, buf_ggtt(c.active));
        SHOWN.store(true, Ordering::Release);
    } else if SHOWN.swap(false, Ordering::AcqRel) {
        m.w(CURCNTR, 0);
        m.w(CURBASE, buf_ggtt(c.active));
    }
}

/// CUR_BUF_CFG and CUR_WM 0..7 (+ TRANS off), from plane 1A's current
/// watermarks capped at the cursor's DDB.
fn write_wm(m: &super::Mmio) {
    let ddb = DDB.load(Ordering::Relaxed);
    let blocks_ddb = ((ddb >> 16) & 0x7FF).saturating_sub(ddb & 0x7FF) + 1;
    m.w(CUR_BUF_CFG, ddb);
    let mut off = false;
    let mut wm = [0u32; 8];
    let own = PLAN_WM_SET.load(Ordering::Acquire);
    for (i, w) in wm.iter_mut().enumerate() {
        if own { *w = PLAN_WM[i].load(Ordering::Relaxed); continue; }
        let v = m.r(kms::preg(kms::PLANE_WM_1_A) + 4 * i as u32);
        if off || v >> 31 == 0 { off = true; continue; }
        let blocks = (v & 0x3FF).clamp(1, blocks_ddb - 1);
        let lines = ((v >> 14) & 0x1F).max((blocks + 1) / 2).min(31);
        *w = 1 << 31 | lines << 14 | blocks;
    }
    for (i, &w) in wm.iter().enumerate() { m.w(CUR_WM + 4 * i as u32, w); }
    m.w(CUR_WM_TRANS, 0);
    if LOGGED.fetch_add(1, Ordering::Relaxed) < 2 {
        // No logging under the lock: keep the values for the caller.
        WM_LOG.store(1, Ordering::Release);
        for (i, &w) in wm.iter().enumerate() { WM_SEEN[i].store(w, Ordering::Relaxed); }
    }
}

static WM_LOG: AtomicU32 = AtomicU32::new(0);
static WM_SEEN: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];

fn log_wm_once() {
    if WM_LOG.swap(0, Ordering::AcqRel) == 0 { return; }
    let w: alloc::vec::Vec<u32> = WM_SEEN.iter().map(|a| a.load(Ordering::Relaxed)).collect();
    let m = kms::mmio();
    ilog!("kms: cursor: enabled: CUR_BUF_CFG 0x{:08x} CUR_WM {:08x?} (plane 1A WM {:08x?}) CURCNTR 0x{:08x} CURPOS 0x{:08x} CURBASE 0x{:08x} SURFLIVE 0x{:08x}",
          DDB.load(Ordering::Relaxed), w,
          (0..8).map(|i| m.r(kms::preg(kms::PLANE_WM_1_A) + 4 * i)).collect::<alloc::vec::Vec<u32>>(),
          m.r(CURCNTR), m.r(CURPOS), m.r(CURBASE), m.r(CURSURFLIVE));
}

// ── KmsCursor hooks ─────────────────────────────────────────────────────────

fn set(src: &KmsCursorSrc, x: i32, y: i32) -> bool {
    let (phys, pitch, w, h) = if src.token != 0 {
        match kms::fb_info(src.token) {
            Some(f) => (f.phys + f.offset as u64, f.pitch, f.width, f.height),
            None => return false,
        }
    } else {
        (src.phys, src.pitch, src.width, src.height)
    };
    if phys == 0 || pitch < 4 { return false; }
    let next = 1 - CUR.lock().active;
    // Copy outside the lock: the other buffer is not being scanned (the
    // previous switch latched before the commit that brings this one).
    let dst = crate::kenv::mm::phys_to_virt(buf_phys(next) as usize) as *mut u8;
    let s = crate::kenv::mm::phys_to_virt(phys as usize) as *const u8;
    let cols = w.min(SIZE) as usize * 4;
    let rows = h.min(SIZE) as usize;
    unsafe {
        for r in 0..SIZE as usize {
            let d = dst.add(r * SIZE as usize * 4);
            if r < rows {
                core::ptr::copy_nonoverlapping(s.add(r * pitch as usize), d, cols);
                core::ptr::write_bytes(d.add(cols), 0, SIZE as usize * 4 - cols);
            } else {
                core::ptr::write_bytes(d, 0, SIZE as usize * 4);
            }
        }
    }
    kms::flush_for_display(buf_phys(next), BYTES as u64);
    {
        let mut c = CUR.lock();
        c.active = next;
        c.loaded = true;
        c.x = x;
        c.y = y;
        program(&c);
    }
    SETS.fetch_add(1, Ordering::Relaxed);
    log_wm_once();
    true
}

fn move_to(x: i32, y: i32) {
    {
        let mut c = CUR.lock();
        c.x = x;
        c.y = y;
        if !c.loaded { return; }
        let m = kms::mmio();
        if SHOWN.load(Ordering::Acquire) && visible(x, y) && m.va != 0 {
            // The common case: position, then the arm.
            m.w(CURPOS, pos(x, y));
            m.w(CURBASE, buf_ggtt(c.active));
        } else {
            program(&c);
        }
    }
    MOVES.fetch_add(1, Ordering::Relaxed);
    log_wm_once();
}

fn hide() {
    let mut c = CUR.lock();
    c.loaded = false;
    program(&c);
}

static CURSOR: KmsCursor = KmsCursor { size: SIZE, set, move_to, hide };

/// The console owns the scanout: cursor off. Lock-free (panic path).
pub(super) fn console_hide() {
    if !ENABLED.load(Ordering::Relaxed) { return; }
    if !SHOWN.swap(false, Ordering::AcqRel) { return; }
    let m = kms::mmio();
    if m.va == 0 { return; }
    m.w(CURCNTR, 0);
    m.w(CURBASE, m.r(CURBASE));
}

/// `(images loaded, moves)` for summary lines.
pub(super) fn stats() -> (u64, u64) { (SETS.load(Ordering::Relaxed), MOVES.load(Ordering::Relaxed)) }

// ── init ────────────────────────────────────────────────────────────────────

/// DDB entry [start, end] (inclusive) of a BUF_CFG value, if any.
fn ddb_range(v: u32) -> Option<(u32, u32)> {
    let (s, e) = (v & 0x7FF, (v >> 16) & 0x7FF);
    if v == 0 || e < s { None } else { Some((s, e)) }
}

/// The whole-pipe plan's CUR_BUF_CFG (`kms::ddb_setup`), 0 = none.
static PLAN_DDB: AtomicU32 = AtomicU32::new(0);
/// skl_compute_wm for the cursor itself against its DDB (`kms::native_setup`).
static PLAN_WM: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];
static PLAN_WM_SET: AtomicBool = AtomicBool::new(false);

pub(super) fn set_ddb(v: u32) { PLAN_DDB.store(v, Ordering::Relaxed); }

/// Blocks of the planned cursor allocation, if there is a plan.
pub(super) fn ddb_blocks() -> Option<u32> {
    let v = PLAN_DDB.load(Ordering::Relaxed);
    ddb_range(v).map(|(s, e)| e - s + 1)
}

pub(super) fn set_wm(w: &[u32; 9]) {
    for i in 0..8 { PLAN_WM[i].store(w[i], Ordering::Relaxed); }
    PLAN_WM_SET.store(true, Ordering::Release);
}

/// The planned allocation, else 32 DDB blocks no live plane uses: the GOP's
/// cursor allocation if it has one, else the highest free run.
fn find_ddb(m: &super::Mmio) -> Result<u32, alloc::string::String> {
    let plan = PLAN_DDB.load(Ordering::Relaxed);
    if plan != 0 { return Ok(plan); }
    let mut used: alloc::vec::Vec<(u32, u32, &'static str)> = alloc::vec::Vec::new();
    for pipe in 0..3u32 {
        let on = m.r(PIPECONF_A + 0x1000 * pipe) >> 31 != 0;
        if !on && pipe != 0 { continue; }
        for pl in 0..3u32 {
            if pipe == 0 && pl == 0 || m.r(reg::plane(reg::PLANE_CTL_1_A, pipe, pl)) >> 31 != 0 {
                if let Some((s, e)) = ddb_range(m.r(reg::plane(kms::PLANE_BUF_CFG_1_A, pipe, pl))) { used.push((s, e, "plane")); }
            }
        }
        if pipe != 0 {
            if let Some((s, e)) = ddb_range(m.r(CUR_BUF_CFG + 0x1000 * pipe)) { used.push((s, e, "cursor")); }
        }
    }
    let free = |s: u32, e: u32| e < DDB_BLOCKS && used.iter().all(|&(us, ue, _)| e < us || s > ue);
    if let Some((s, e)) = ddb_range(m.r(CUR_BUF_CFG)) {
        if e - s + 1 >= 8 && free(s, e) { return Ok(s | e << 16); }
    }
    let mut start = DDB_BLOCKS - CUR_DDB;
    loop {
        if free(start, start + CUR_DDB - 1) { return Ok(start | (start + CUR_DDB - 1) << 16); }
        if start == 0 { break; }
        start -= 1;
    }
    Err(alloc::format!("{:?}", used))
}

/// Set the cursor up (unless `i915.hwcursor=0`) and register the cursor
/// half. Called from `kms::init` before the backend registers.
pub(super) fn init(d: &Device) -> bool {
    if opt("i915.hwcursor") == Some(0) {
        log("kms: i915.hwcursor=0: no cursor plane (the compositor composites the pointer)");
        return false;
    }
    if d.gmch.ggtt_size < kms::CURSOR_GGTT + 2 * BYTES as u64 {
        log("kms: cursor: GGTT too small; no hardware cursor");
        return false;
    }
    let m = &d.mmio;
    let ddb = match find_ddb(m) {
        Ok(v) => v,
        Err(used) => {
            ilog!("kms: cursor: no {} free DDB blocks (in use: {}); no hardware cursor", CUR_DDB, used);
            return false;
        }
    };
    // Two 16 KiB images, contiguous (order 3 = 32 KiB), zeroed, flushed.
    let Some(phys) = crate::kenv::mm::buddy::alloc(3) else { log("kms: cursor: out of memory; no hardware cursor"); return false };
    unsafe { core::ptr::write_bytes(crate::kenv::mm::phys_to_virt(phys) as *mut u8, 0, 2 * BYTES); }
    kms::flush_for_display(phys as u64, 2 * BYTES as u64);
    let base = kms::CURSOR_GGTT / 4096;
    for i in 0..(2 * BYTES as u64 / 4096) { kms::gsm_write(base + i, hw::ggtt_pte(phys as u64 + i * 4096)); }
    m.w(reg::GFX_FLSH_CNTL_GEN6, 1);
    BUF_PHYS.store(phys as u64, Ordering::Release);
    DDB.store(ddb, Ordering::Relaxed);
    let was = m.r(CURCNTR);
    if was & 0x27 != 0 && !d.fake {
        // The firmware left a cursor on: off until the compositor sets one.
        m.w(CURCNTR, 0);
        m.w(CURBASE, buf_ggtt(0));
    }
    ENABLED.store(true, Ordering::Release);
    crate::kenv::kms::register_cursor(&CURSOR);
    ilog!("kms: cursor: hardware cursor on pipe A: {}x{} ARGB8888, images at GGTT 0x{:08x}/0x{:08x}, DDB blocks {}..={} (CUR_BUF_CFG was 0x{:08x}, CURCNTR was 0x{:08x}; plane 1A BUF_CFG 0x{:08x}); i915.hwcursor=0 turns it off",
          SIZE, SIZE, buf_ggtt(0), buf_ggtt(1), ddb & 0x7FF, (ddb >> 16) & 0x7FF, m.r(CUR_BUF_CFG), was,
          m.r(kms::preg(kms::PLANE_BUF_CFG_1_A)));
    true
}
