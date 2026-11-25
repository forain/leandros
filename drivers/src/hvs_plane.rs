//! `hvs-plane`: scan a v3d BO out of the Raspberry Pi 5 HVS directly (zero copy), rpi5 only.
//!
//! The firmware leaves channel 0 running a one-plane display list (unity, linear, XRGB8888) that
//! reads its framebuffer. We copy that list (8 words + END) into one of two free dlist RAM slots,
//! change only the address and pitch words, and write the slot's start into `DISP0_LPTRS` once per
//! flip; the HVS picks it up at the next frame start (vblank). Nothing else is ever written:
//! channel 1 (a filter table) and the firmware's own list stay untouched, and `restore()` puts the
//! firmware list back (console, VT switch, panic, kexec).
//!
//! Layout from Linux rpi-6.12.y `vc4_regs.h` (SCALER6D_*, the D0 stepping) and `vc4_plane.c`.
//! The pure part (list encoding, decisions) is host-tested in `drivers/v3d-tests`; the register
//! glue is the `hw` module. Contract with the v3d backend (`v3d_scanout`): the BO is idle when
//! `flip` runs; `flip` holds its own reference on the BO until the HVS has certainly left it.

/// `hvs.plane=0` opts out; anything else (or nothing) leaves the HVS sink eligible.
pub fn plane_enabled(cmdline: &str) -> bool {
    !cmdline.split_ascii_whitespace().any(|t| t == "hvs.plane=0")
}

pub const CTL0_END: u32 = 1 << 31;
pub const CTL0_VALID: u32 = 1 << 30;
pub const CTL0_NEXT_SHIFT: u32 = 24;
pub const CTL0_NEXT_MASK: u32 = 0x3f << CTL0_NEXT_SHIFT;
pub const CTL0_ADDR_MODE_MASK: u32 = 7 << 20;
pub const CTL0_UNITY: u32 = 1 << 15;
/// Words of a unity linear plane: CTL0 POS0 CTL2 POS2 POS3 PTR0 PTR1 PTR2.
pub const PLANE_WORDS: usize = 8;
/// Plane plus the END word.
pub const LIST_WORDS: usize = PLANE_WORDS + 1;
pub const POS3_CONTEXT: u32 = 0xc0c0_c0c0;
const PTR0_ADDR_MASK: u32 = 0xff;
const PTR2_PITCH_MASK: u32 = 0x1_ffff;

/// What the firmware's channel 0 list shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FwPlane {
    pub words: [u32; PLANE_WORDS],
    pub w: u32,
    pub h: u32,
    pub phys: u64,
    pub pitch: u32,
}

/// Decode the firmware's first plane. `ctrl0`/`status` are channel 0's CTRL0 and STATUS. `None`
/// unless the channel runs (enabled, mode RUN) with a valid, linear, unity plane.
pub fn parse_fw_plane(ctrl0: u32, status: u32, words: &[u32; PLANE_WORDS]) -> Option<FwPlane> {
    let mode = (status >> 13) & 3;
    if ctrl0 >> 31 == 0 || mode != 2 { return None; }
    let c0 = words[0];
    if c0 & CTL0_VALID == 0 || c0 & CTL0_END != 0 { return None; }
    if c0 & CTL0_ADDR_MODE_MASK != 0 || c0 & CTL0_UNITY == 0 { return None; }
    let w = (words[3] & 0x1fff) + 1;
    let h = ((words[3] >> 16) & 0x1fff) + 1;
    let phys = (((words[5] & PTR0_ADDR_MASK) as u64) << 32) | words[6] as u64;
    let pitch = words[7] & PTR2_PITCH_MASK;
    if phys == 0 || pitch < w * 4 { return None; }
    Some(FwPlane { words: *words, w, h, phys, pitch })
}

/// The display list that shows the BO at `phys` with `pitch`: the firmware's plane words, with
/// NEXT = 8, a fresh context word, the 40-bit address and the pitch replaced, then END.
pub fn build_list(fw: &FwPlane, phys: u64, pitch: u32) -> [u32; LIST_WORDS] {
    let mut l = [0u32; LIST_WORDS];
    l[..PLANE_WORDS].copy_from_slice(&fw.words);
    l[0] = (l[0] & !CTL0_NEXT_MASK) | ((PLANE_WORDS as u32) << CTL0_NEXT_SHIFT);
    l[4] = POS3_CONTEXT;
    l[5] = (l[5] & !PTR0_ADDR_MASK) | ((phys >> 32) as u32 & PTR0_ADDR_MASK);
    l[6] = phys as u32;
    l[7] = (l[7] & !PTR2_PITCH_MASK) | (pitch & PTR2_PITCH_MASK);
    l[8] = CTL0_END;
    l
}

/// May this BO window be scanned out as is? Same size as the firmware plane, XRGB rows, 16-byte
/// aligned linear layout within the 40-bit / 17-bit-pitch reach of the plane words.
pub fn bo_matches(fw: &FwPlane, w: u32, h: u32, phys: u64, pitch: u32) -> bool {
    w == fw.w && h == fw.h && phys != 0 && phys & 0xf == 0 && phys >> 40 == 0
        && pitch >= w * 4 && pitch & 3 == 0 && pitch <= PTR2_PITCH_MASK
}

/// Frames elapsed between two 6-bit FRCNT reads.
pub fn frcnt_delta(prev: u32, now: u32) -> u32 { now.wrapping_sub(prev) & 0x3f }

/// Is a dlist RAM region unused? The firmware fills free space with 0xb0b0b0b0; accept zeros too.
pub fn region_free(words: &[u32]) -> bool { words.iter().all(|&x| x == 0xb0b0_b0b0 || x == 0) }

/// Pick two free slots (word offsets) from candidates `base, base+step, ...` (at most `tries`);
/// `read(off)` returns the LIST_WORDS words at `off`.
pub fn pick_slots(base: usize, step: usize, tries: usize, mut read: impl FnMut(usize) -> [u32; LIST_WORDS]) -> Option<[usize; 2]> {
    let mut got = [0usize; 2];
    let mut n = 0;
    for i in 0..tries {
        let off = base + i * step;
        if region_free(&read(off)) {
            got[n] = off;
            n += 1;
            if n == 2 { return Some(got); }
        }
    }
    None
}

/// One frame at 60.000 Hz, the period `next_check_at` assumes.
pub const FRAME_NS: u64 = 16_666_667;

/// When to look at the HVS next: just before the next frame boundary predicted from the last
/// observed latch, then in 250 us steps; 1 ms steps until a boundary has been observed.
pub fn next_check_at(phase_ns: u64, now: u64) -> u64 {
    if phase_ns == 0 { return now + 1_000_000; }
    let k = now.saturating_sub(phase_ns) / FRAME_NS + 1;
    let t = (phase_ns + k * FRAME_NS).saturating_sub(300_000);
    if t <= now + 50_000 { now + 250_000 } else { t }
}

/// The phase to remember when a latch is seen at `now` (the observation time, not the latch).
pub fn latched_phase(_prev_ns: u64, now: u64) -> u64 { now }

/// Upper edges (us) of the event-lateness histogram buckets; the last bucket is open.
pub const LATE_EDGES_US: [u64; 6] = [250, 500, 1000, 2000, 4000, 8000];

/// Histogram bucket of a lateness in ns: <250, <500, <1000, <2000, <4000, <8000, >=8000 us.
pub fn late_bucket(ns: u64) -> usize {
    let us = ns / 1000;
    LATE_EDGES_US.iter().position(|&e| us < e).unwrap_or(LATE_EDGES_US.len())
}

/// CH0 STATUS fields (UNVERIFIED on hardware; candidate layout, firmware sample 0x00ba4124):
/// scan line [12:0], mode [14:13] (2 = run), FRCNT [21:16].
pub fn status_line(status: u32) -> u32 { status & 0x1fff }
pub fn status_mode(status: u32) -> u32 { (status >> 13) & 3 }
pub fn status_frcnt(status: u32) -> u32 { (status >> 16) & 0x3f }

/// Bins of the scan-line histogram: 8 equal bins of 0..vtotal, then bin 8 for line >= vtotal
/// (blanking beyond the frame / implausible reading).
pub const LINE_BINS: usize = 9;
pub fn line_bin(line: u32, vtotal: u32) -> usize {
    if vtotal == 0 || line >= vtotal { return LINE_BINS - 1; }
    (line as u64 * 8 / vtotal as u64) as usize
}

/// Assumed total lines of a frame with `h` active lines (CEA timing: 1080 -> 1125). An
/// assumption, reported as such.
pub fn vtotal_guess(h: u32) -> u32 { (h as u64 * 1125 / 1080) as u32 }

/// Time since the frame start implied by `line` (0 for lines outside the active range `h`
/// give `None`): line * period / vtotal.
pub fn line_late_ns(line: u32, h: u32, vtotal: u32, period_ns: u64) -> Option<u64> {
    if vtotal == 0 || line >= h { return None; }
    Some(line as u64 * period_ns / vtotal as u64)
}

/// Counters behind `/proc/leandros_hvs`. Plain atomics, no locks; every field is a diagnostic.
pub mod stats {
    use core::fmt::Write;
    use core::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
    use super::late_bucket;

    pub static LIVE: AtomicU32 = AtomicU32::new(0);
    pub static FLIPS: AtomicU64 = AtomicU64::new(0);
    pub static WAITS: AtomicU64 = AtomicU64::new(0);
    pub static TICKS: AtomicU64 = AtomicU64::new(0);
    pub static BUSY_REFUSED: AtomicU64 = AtomicU64::new(0);
    pub static EVENTS: AtomicU64 = AtomicU64::new(0);
    pub static MISSED: AtomicU64 = AtomicU64::new(0);
    /// Upper-bound lateness histogram (only events with a pending observation before them).
    pub static LATE: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
    /// Events with no pending observation before them (no bound exists).
    pub static LATE_UNBOUNDED: AtomicU64 = AtomicU64::new(0);
    /// Scan-line histogram at latch observation (UNVERIFIED field), and implied lateness.
    pub static LINE: [AtomicU64; super::LINE_BINS] = [const { AtomicU64::new(0) }; super::LINE_BINS];
    pub static LINE_LATE: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
    pub static LINE_SKIPPED: AtomicU64 = AtomicU64::new(0);
    /// Last 8 raw STATUS values at latch with their ktime (ns), newest at RING_N-1 mod 8.
    pub static RING_N: AtomicU64 = AtomicU64::new(0);
    pub static RING_ST: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];
    pub static RING_NS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
    /// FRCNT frames and the time they took, summed over unambiguous poll pairs.
    pub static PERIOD_FRAMES: AtomicU64 = AtomicU64::new(0);
    pub static PERIOD_NS: AtomicU64 = AtomicU64::new(0);
    pub static LATCH_STATUS: AtomicU32 = AtomicU32::new(0);
    pub static FENCE_NS: AtomicU64 = AtomicU64::new(0);
    pub static FENCE_MAX_NS: AtomicU64 = AtomicU64::new(0);
    pub static FENCE_N: AtomicU64 = AtomicU64::new(0);
    pub static TAKEOVER_NS: AtomicU64 = AtomicU64::new(0);
    pub static KSYNC: AtomicU64 = AtomicU64::new(0);

    /// Account one flip event. `bound_ns` is `Some(now - last observation that saw it pending)`,
    /// an upper bound of its lateness, or `None` when no pending observation preceded it.
    pub fn record_event(bound_ns: Option<u64>) {
        EVENTS.fetch_add(1, Relaxed);
        match bound_ns {
            Some(b) => {
                LATE[late_bucket(b)].fetch_add(1, Relaxed);
                if b > super::FRAME_NS { MISSED.fetch_add(1, Relaxed); }
            }
            None => { LATE_UNBOUNDED.fetch_add(1, Relaxed); }
        }
    }

    /// Record the raw STATUS seen at a latch at ktime `now`, decode its scan line against an
    /// active height `h` and `vtotal`, and bin it. `period_ns` converts the line to time.
    pub fn record_status(status: u32, now: u64, h: u32, vtotal: u32, period_ns: u64) {
        LATCH_STATUS.store(status, Relaxed);
        let i = RING_N.fetch_add(1, Relaxed) as usize & 7;
        RING_ST[i].store(status, Relaxed);
        RING_NS[i].store(now, Relaxed);
        let line = super::status_line(status);
        LINE[super::line_bin(line, vtotal)].fetch_add(1, Relaxed);
        match super::line_late_ns(line, h, vtotal, period_ns) {
            Some(ns) => { LINE_LATE[late_bucket(ns)].fetch_add(1, Relaxed); }
            None => { LINE_SKIPPED.fetch_add(1, Relaxed); }
        }
    }

    pub fn record_fence_wait(ns: u64) {
        FENCE_N.fetch_add(1, Relaxed);
        FENCE_NS.fetch_add(ns, Relaxed);
        FENCE_MAX_NS.fetch_max(ns, Relaxed);
    }

    pub fn record_period(frames: u64, ns: u64) {
        PERIOD_FRAMES.fetch_add(frames, Relaxed);
        PERIOD_NS.fetch_add(ns, Relaxed);
    }

    /// Measured frame period in ns (0 until a sample exists). Reported only; nothing uses it.
    pub fn period_ns() -> u64 {
        let f = PERIOD_FRAMES.load(Relaxed);
        if f == 0 { 0 } else { PERIOD_NS.load(Relaxed) / f }
    }

    /// Zero every counter (`LIVE` and `TAKEOVER_NS` describe state, not a count, and stay).
    pub fn reset() {
        for c in [&FLIPS, &WAITS, &TICKS, &BUSY_REFUSED, &EVENTS, &MISSED, &PERIOD_FRAMES, &PERIOD_NS,
                  &FENCE_NS, &FENCE_MAX_NS, &FENCE_N, &KSYNC] { c.store(0, Relaxed); }
        for c in LATE.iter().chain(LINE_LATE.iter()) { c.store(0, Relaxed); }
        for c in &LINE { c.store(0, Relaxed); }
        for c in [&LATE_UNBOUNDED, &LINE_SKIPPED, &RING_N] { c.store(0, Relaxed); }
        LATCH_STATUS.store(0, Relaxed);
    }

    pub fn text(o: &mut dyn Write) {
        let g = |c: &AtomicU64| c.load(Relaxed);
        let _ = writeln!(o, "live {}", LIVE.load(Relaxed));
        let _ = writeln!(o, "flips {}", g(&FLIPS));
        let _ = writeln!(o, "waits {}", g(&WAITS));
        let _ = writeln!(o, "ticks {}", g(&TICKS));
        let _ = writeln!(o, "busy_refused {}", g(&BUSY_REFUSED));
        let _ = writeln!(o, "events {}", g(&EVENTS));
        let _ = writeln!(o, "missed {}", g(&MISSED));
        let _ = writeln!(o, "late_bound_us (UPPER BOUND, only events with a pending observation before them) <250 <500 <1000 <2000 <4000 <8000 >=8000");
        let _ = writeln!(o, "late_bound {} {} {} {} {} {} {}", g(&LATE[0]), g(&LATE[1]), g(&LATE[2]), g(&LATE[3]),
                         g(&LATE[4]), g(&LATE[5]), g(&LATE[6]));
        let _ = writeln!(o, "late_unbounded {}", g(&LATE_UNBOUNDED));
        let _ = writeln!(o, "scan_line UNVERIFIED (STATUS[12:0]; bins of 0..vtotal/8, last = line>=vtotal or unknown vtotal)");
        let _ = writeln!(o, "line_bins {} {} {} {} {} {} {} {} {}", g(&LINE[0]), g(&LINE[1]), g(&LINE[2]), g(&LINE[3]),
                         g(&LINE[4]), g(&LINE[5]), g(&LINE[6]), g(&LINE[7]), g(&LINE[8]));
        let _ = writeln!(o, "late_line_us UNVERIFIED (line*period/vtotal) <250 <500 <1000 <2000 <4000 <8000 >=8000");
        let _ = writeln!(o, "late_line {} {} {} {} {} {} {}", g(&LINE_LATE[0]), g(&LINE_LATE[1]), g(&LINE_LATE[2]),
                         g(&LINE_LATE[3]), g(&LINE_LATE[4]), g(&LINE_LATE[5]), g(&LINE_LATE[6]));
        let _ = writeln!(o, "line_outside_active {}", g(&LINE_SKIPPED));
        let n = RING_N.load(Relaxed);
        for k in 0..n.min(8) {
            let i = ((n - 1 - k) & 7) as usize;
            let _ = writeln!(o, "status_ring ktime_ns {} raw {:#010x}", RING_NS[i].load(Relaxed), RING_ST[i].load(Relaxed));
        }
        let _ = writeln!(o, "frame_period_ns {} frames {}", period_ns(), g(&PERIOD_FRAMES));
        let st = LATCH_STATUS.load(Relaxed);
        let _ = writeln!(o, "latch_status {:#010x} frcnt {} mode {}", st, (st >> 16) & 0x3f, (st >> 13) & 3);
        let _ = writeln!(o, "fence_wait_ns {} max {} count {}", g(&FENCE_NS), g(&FENCE_MAX_NS), g(&FENCE_N));
        let _ = writeln!(o, "takeover_ms {}.{:03}", g(&TAKEOVER_NS) / 1_000_000, g(&TAKEOVER_NS) / 1000 % 1000);
        let _ = writeln!(o, "ksync_violations {}", g(&KSYNC));
    }
}

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub use hw::{busy, console_restore as restore, event, flip, init, is_live, ksync_now, queue, wait_idle};

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
mod hw {
    use super::*;
    use crate::drm_device_interface as dri;
    use crate::pci::serial_debug;
    use crate::DriverError;
    use alloc::format;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
    use leandros_lib::mmio::{Mmio, MmioRd};
    use mm::kva::{ioremap, CacheMode};
    use spin::Mutex;
    use core::sync::atomic::AtomicPtr;

    const HVS_PHYS: u64 = 0x10_7c58_0000;
    const HVS_LEN: usize = 0x8000;
    const DLIST_OFF: usize = 0x4000;
    const DLIST_WORDS: usize = 4096;
    const VERSION: usize = 0x00;
    const CH0: usize = 0x100;
    const CH_CTRL0: usize = CH0;
    const CH_LPTRS: usize = CH0 + 0x10;
    const CH_STATUS: usize = CH0 + 0x18;
    const CH_DL: usize = CH0 + 0x1c;
    /// Free dlist RAM starts far above the firmware lists (ch1 at 0, ch0 around 672).
    const SLOT_BASE: usize = 2048;
    const SLOT_STEP: usize = 32;
    const LATCH_TIMEOUT_NS: u64 = 100_000_000;

    static PENDING: AtomicBool = AtomicBool::new(false);
    static TIMER: AtomicPtr<sched::hrtimer::HrTimer> = AtomicPtr::new(core::ptr::null_mut());
    static VA: AtomicUsize = AtomicUsize::new(0);
    static FW_LPTRS: AtomicU32 = AtomicU32::new(0);
    /// Our list is (or may be) on screen; false after `restore`.
    static ACTIVE: AtomicBool = AtomicBool::new(false);
    /// A failed takeover or a bad latch: the sink stays off, the backend uses FwCopy.
    static DEAD: AtomicBool = AtomicBool::new(false);

    struct State {
        fw: FwPlane,
        slots: [usize; 2],
        /// Slot written last (index into `slots`).
        cur: usize,
        /// `cur` was written and not yet seen as the active list.
        pending: bool,
        /// The BO the `cur` list reads (we hold a reference).
        cur_obj: u32,
        issued_ns: u64,
        frames: u64,
        last_frcnt: u32,
        /// (BO, frame count at retirement): released two frames after its list was replaced.
        retire: Vec<(u32, u64)>,
        flips: u64,
        waits: u64,
        /// Event cookie of the flip in flight (0: none).
        ev: u32,
        /// No-pixel commits waiting for a vblank.
        extra: Vec<u32>,
        /// When the last latch was seen (0: never).
        phase_ns: u64,
        /// Last time the flip in flight was known to be pending (issue or a check that saw it so).
        last_pending_ns: u64,
        /// A tick has seen the flip in flight still pending (a bound for its lateness exists).
        seen_pending: bool,
        /// Time of the previous `poll` (0: none).
        last_poll_ns: u64,
    }

    static STATE: Mutex<Option<State>> = Mutex::new(None);

    #[inline]
    fn rd(off: usize) -> u32 { unsafe { ((VA.load(Ordering::Relaxed) + off) as *const u32).mmio_read() } }
    #[inline]
    fn wr(off: usize, v: u32) { unsafe { <u32 as Mmio>::mmio_write((VA.load(Ordering::Relaxed) + off) as *mut u32, v) } }
    fn dl_read<const N: usize>(word: usize) -> [u32; N] { core::array::from_fn(|i| rd(DLIST_OFF + (word + i) * 4)) }

    pub fn is_live() -> bool { VA.load(Ordering::Relaxed) != 0 && !DEAD.load(Ordering::Relaxed) }

    /// Validate that channel 0 runs the firmware's one-plane list matching the firmware
    /// framebuffer, find two free slots, remember the firmware LPTRS. Writes nothing.
    pub fn init(cmdline: &str, serror_check: fn() -> Option<u64>) -> Option<(u32, u32)> {
        if !plane_enabled(cmdline) {
            serial_debug("[hvs] plane disabled (hvs.plane=0)\n");
            return None;
        }
        let (fb_base, fb_w, fb_h, fb_pitch) = crate::framebuffer::get_hardware_fb_info()?;
        let va = ioremap(HVS_PHYS, HVS_LEN, CacheMode::Uncached)?;
        VA.store(va, Ordering::Relaxed);
        let fail = |why: &str| -> Option<(u32, u32)> {
            serial_debug(&format!("[hvs] plane not used: {}\n", why));
            VA.store(0, Ordering::Relaxed);
            None
        };
        let version = rd(VERSION);
        if let Some(syn) = serror_check() { return fail(&format!("serror 0x{:x} on VERSION", syn)); }
        if version & 0xff != 0x54 { return fail(&format!("version 0x{:x} is not the D0 layout", version)); }
        let (c0, lp, st, dl) = (rd(CH_CTRL0), rd(CH_LPTRS), rd(CH_STATUS), rd(CH_DL));
        let start = (lp & 0xfff) as usize;
        if start + PLANE_WORDS > DLIST_WORDS { return fail("firmware list out of range"); }
        let words: [u32; PLANE_WORDS] = dl_read(start);
        let Some(fw) = parse_fw_plane(c0, st, &words) else {
            return fail(&format!("ch0 not a running one-plane list (ctrl0={:08x} status={:08x} w0={:08x})", c0, st, words[0]));
        };
        if fw.phys != fb_base || fw.w != fb_w || fw.h != fb_h || fw.pitch != fb_pitch {
            return fail(&format!("firmware plane {}x{} ptr={:#x} pitch={} differs from the framebuffer {}x{} {:#x} pitch={}",
                fw.w, fw.h, fw.phys, fw.pitch, fb_w, fb_h, fb_base, fb_pitch));
        }
        let Some(slots) = pick_slots(SLOT_BASE, SLOT_STEP, 60, |o| dl_read::<LIST_WORDS>(o)) else { return fail("no free dlist slots"); };
        if let Some(syn) = serror_check() { return fail(&format!("serror 0x{:x} on dlist read", syn)); }
        FW_LPTRS.store(lp, Ordering::Relaxed);
        *STATE.lock() = Some(State {
            fw, slots, cur: 1, pending: false, cur_obj: 0, issued_ns: 0, frames: 0,
            last_frcnt: (st >> 16) & 0x3f, retire: Vec::new(), flips: 0, waits: 0, ev: 0, extra: Vec::new(), phase_ns: 0,
            last_pending_ns: 0, seen_pending: false, last_poll_ns: 0,
        });
        let tm = alloc::boxed::Box::into_raw(sched::hrtimer::HrTimer::new(tick, 0, sched::hrtimer::TimerMode::Worker));
        TIMER.store(tm, Ordering::Release);
        stats::LIVE.store(1, Ordering::Relaxed);
        crate::kenv::dma_quiesce_register("hvs-plane", quiesce);
        serial_debug(&format!(
            "[hvs] plane ready {}x{} pitch={} fw ptr={:#x} lptrs={:#x} lact={:#x} slots={},{}\n",
            fw.w, fw.h, fw.pitch, fw.phys, lp, dl & 0xfff, slots[0], slots[1]));
        Some((fw.w, fw.h))
    }

    /// Put the firmware list back. Lock-free (panic path, VT switch, kexec stop hook).
    pub fn console_restore() {
        if !ACTIVE.swap(false, Ordering::AcqRel) { return; }
        wr(CH_LPTRS, FW_LPTRS.load(Ordering::Relaxed));
    }

    /// kexec stop hook: firmware list back on screen, then (once it has surely latched, plus a
    /// margin of frames) refill our slots with the firmware's free-space pattern so the next
    /// kernel finds them free. Spins: this runs late in the stop path.
    fn quiesce() -> bool {
        let was = ACTIVE.load(Ordering::Acquire);
        console_restore();
        if !was { return true; }
        let slots = match STATE.try_lock() { Some(g) => g.as_ref().map(|s| s.slots), None => None };
        let Some(slots) = slots else { return true };
        let fw_lact = FW_LPTRS.load(Ordering::Relaxed) & 0xfff;
        let t0 = sched::monotonic_ns();
        while (rd(CH_DL) & 0xfff) != fw_lact && sched::monotonic_ns().wrapping_sub(t0) < 50_000_000 {
            core::hint::spin_loop();
        }
        let t1 = sched::monotonic_ns();
        while sched::monotonic_ns().wrapping_sub(t1) < 20_000_000 { core::hint::spin_loop(); }
        if (rd(CH_DL) & 0xfff) == fw_lact {
            for off in slots { for i in 0..LIST_WORDS { wr(DLIST_OFF + (off + i) * 4, 0xb0b0_b0b0); } }
        }
        true
    }

    fn poll(s: &mut State) {
        let f = (rd(CH_STATUS) >> 16) & 0x3f;
        let d = frcnt_delta(s.last_frcnt, f) as u64;
        s.frames += d;
        let t = sched::monotonic_ns();
        // Only pairs shorter than the 6-bit wrap are unambiguous.
        if s.last_poll_ns != 0 && d != 0 && t.wrapping_sub(s.last_poll_ns) < 32 * FRAME_NS {
            stats::record_period(d, t.wrapping_sub(s.last_poll_ns));
        }
        s.last_poll_ns = t;
        s.last_frcnt = f;
        let keep = core::mem::take(&mut s.retire);
        for (obj, at) in keep {
            if s.frames >= at + 2 { dri::v3d_scanout_unref(obj); } else { s.retire.push((obj, at)); }
        }
    }

    /// Wait until the list in `slot` is the one the HVS runs.
    fn wait_latch(s: &mut State, slot: usize) -> bool {
        let t0 = sched::monotonic_ns();
        loop {
            if (rd(CH_DL) & 0xfff) as usize == slot { return true; }
            let now = sched::monotonic_ns();
            if now.wrapping_sub(t0) > LATCH_TIMEOUT_NS { return false; }
            s.waits += 1;
            stats::WAITS.fetch_add(1, Ordering::Relaxed);
            // A list written mid-frame latches at the next frame start, up to one period away:
            // sleep most of 6 ms in one go, then poll every 2 ms (few wakeups beat tight polling).
            let since = now.wrapping_sub(s.issued_ns);
            let v0 = ksync_now();
            sched::kthread::sleep_ns(if since < 6_000_000 { 6_000_000 - since } else { 2_000_000 });
            stats::KSYNC.fetch_add(ksync_now().wrapping_sub(v0), Ordering::Relaxed);
        }
    }

    /// Global count of voluntary switches/sleeps made with a spin lock held (`/proc/lockstat`'s
    /// `spin_held` line). A delta across a sleep on this path is attributed to it (other CPUs'
    /// violations can leak in, so it is an upper bound).
    pub fn ksync_now() -> u64 { sched::spinheld::switch_violations() as u64 + sched::irqctx::sleep_violations() as u64 }

    fn give_up(why: &str) {
        DEAD.store(true, Ordering::Relaxed);
        console_restore();
        serial_debug(&format!("[hvs] plane off, falling back to the copy sink: {}\n", why));
    }

    fn complete(ev: u32, frames: u64, now: u64) {
        if ev != 0 { dri::ext_event_complete(ev, frames as u32, now); }
    }

    /// The `cur` list is the one on screen: finish the flip in flight (its event and any no-pixel
    /// events waiting for a vblank).
    fn latched(s: &mut State, now: u64) {
        s.pending = false;
        s.phase_ns = latched_phase(s.phase_ns, now);
        let st = rd(CH_STATUS);
        let vtotal = vtotal_guess(s.fw.h);
        let period = match stats::period_ns() { 0 => FRAME_NS, p => p };
        stats::record_status(st, now, s.fw.h, vtotal, period);
        if s.ev != 0 {
            stats::record_event(if s.seen_pending { Some(now.wrapping_sub(s.last_pending_ns)) } else { None });
        }
        s.seen_pending = false;
        poll(s);
        let ev = core::mem::take(&mut s.ev);
        complete(ev, s.frames, now);
        for e in core::mem::take(&mut s.extra) { complete(e, s.frames, now); }
        PENDING.store(false, Ordering::Release);
    }

    fn next_check(s: &State, now: u64) -> u64 { next_check_at(s.phase_ns, now) }

    fn arm(at: u64) {
        let p = TIMER.load(Ordering::Acquire);
        if p.is_null() { return; }
        let t = unsafe { &*p };
        t.cancel();
        t.start_at(at, 0);
    }

    extern "C" fn tick(_ctx: usize) {
        stats::TICKS.fetch_add(1, Ordering::Relaxed);
        if DEAD.load(Ordering::Relaxed) { return; }
        let Some(mut g) = STATE.try_lock() else { arm(sched::monotonic_ns() + 500_000); return };
        let Some(s) = g.as_mut() else { return };
        let now = sched::monotonic_ns();
        if !ACTIVE.load(Ordering::Acquire) {
            // The firmware list is back (VT switch, restore): nothing is in flight any more.
            if s.pending || PENDING.load(Ordering::Relaxed) { latched(s, now); }
            return;
        }
        poll(s);
        if s.pending {
            if (rd(CH_DL) & 0xfff) as usize == s.slots[s.cur] {
                latched(s, now);
            } else if now.wrapping_sub(s.issued_ns) > LATCH_TIMEOUT_NS {
                let cur = s.slots[s.cur];
                give_up(&format!("slot {} never became active (lact={:#x})", cur, rd(CH_DL) & 0xfff));
                latched(s, now);
                return;
            } else {
                s.last_pending_ns = now;
                s.seen_pending = true;
                arm(next_check(s, now));
                return;
            }
        } else if !s.extra.is_empty() {
            latched(s, now);
        }
        if !s.retire.is_empty() { arm(now + 3 * FRAME_NS); }
    }

    /// A flip or a no-pixel event is queued and has not completed (lock-free).
    pub fn busy() -> bool { PENDING.load(Ordering::Acquire) }

    /// Sleep until not `busy` or `timeout_ns` passed.
    pub fn wait_idle(timeout_ns: u64) -> bool {
        let t0 = sched::monotonic_ns();
        while busy() {
            if sched::monotonic_ns().wrapping_sub(t0) > timeout_ns { return false; }
            let v0 = ksync_now();
            sched::kthread::sleep_ns(500_000);
            stats::KSYNC.fetch_add(ksync_now().wrapping_sub(v0), Ordering::Relaxed);
        }
        true
    }

    /// Complete `ev` at the next vblank without changing the picture.
    pub fn event(ev: u32) {
        let mut g = STATE.lock();
        let Some(s) = g.as_mut() else { complete(ev, 0, sched::monotonic_ns()); return };
        s.extra.push(ev);
        PENDING.store(true, Ordering::Release);
        let now = sched::monotonic_ns();
        arm(next_check(s, now));
    }

    /// Show the BO window (`obj` pinned by the caller's framebuffer) on channel 0, blocking form
    /// (SETCRTC): the picture is queued and the call returns; no event.
    pub fn flip(obj: u32, phys: u64, w: u32, h: u32, pitch: u32) -> Result<(), DriverError> {
        queue(obj, phys, w, h, pitch, 0)
    }

    /// Queue the BO window on channel 0 and return at once. `ev` (nonzero) is completed by `tick`
    /// when the list has latched. Only one flip is in flight: the caller checks `busy` first
    /// (EBUSY / wait_idle); a flip that still finds one pending waits for its latch.
    pub fn queue(obj: u32, phys: u64, w: u32, h: u32, pitch: u32, ev: u32) -> Result<(), DriverError> {
        if DEAD.load(Ordering::Relaxed) { return Err(DriverError::NotFound); }
        let mut g = STATE.lock();
        let s = g.as_mut().ok_or(DriverError::NotFound)?;
        if !bo_matches(&s.fw, w, h, phys, pitch) { return Err(DriverError::InvalidParameter); }
        if !ACTIVE.load(Ordering::Acquire) && s.pending { let n = sched::monotonic_ns(); latched(s, n); }
        poll(s);
        if s.pending {
            let cur = s.slots[s.cur];
            if !wait_latch(s, cur) {
                give_up(&format!("slot {} never became active (lact={:#x})", cur, rd(CH_DL) & 0xfff));
                let n = sched::monotonic_ns();
                latched(s, n);
                return Err(DriverError::Io);
            }
            let n = sched::monotonic_ns();
            latched(s, n);
        }
        if !dri::v3d_scanout_ref_obj(obj) { return Err(DriverError::NotFound); }
        let next = s.cur ^ 1;
        let off = s.slots[next];
        let list = build_list(&s.fw, phys, pitch);
        for (i, wd) in list.iter().enumerate() { wr(DLIST_OFF + (off + i) * 4, *wd); }
        unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)); }
        wr(CH_LPTRS, off as u32);
        ACTIVE.store(true, Ordering::Release);
        if s.cur_obj != 0 { let at = s.frames; let o = s.cur_obj; s.retire.push((o, at)); }
        s.cur = next;
        s.cur_obj = obj;
        s.pending = true;
        s.ev = ev;
        PENDING.store(true, Ordering::Release);
        s.issued_ns = sched::monotonic_ns();
        s.last_pending_ns = s.issued_ns;
        s.seen_pending = false;
        s.flips += 1;
        stats::FLIPS.fetch_add(1, Ordering::Relaxed);
        if s.flips == 1 {
            // Prove the takeover took before trusting it for good.
            if !wait_latch(s, off) {
                give_up("first list never latched");
                let n = sched::monotonic_ns();
                latched(s, n);
                return Err(DriverError::Io);
            }
            let n = sched::monotonic_ns();
            stats::TAKEOVER_NS.store(n.wrapping_sub(s.issued_ns), Ordering::Relaxed);
            latched(s, n);
        } else {
            let n = s.issued_ns;
            arm(next_check(s, n));
        }
        if crate::kms_v3d::flip_log_due(s.flips) {
            serial_debug(&format!(
                "[hvs] flip {} slot={} ptr={:#x} pitch={} lptrs={:#x} lact={:#x} frcnt={} frames={} waits={} ticks={}\n",
                s.flips, off, phys, pitch, rd(CH_LPTRS) & 0xfff, rd(CH_DL) & 0xfff, s.last_frcnt, s.frames, s.waits,
                stats::TICKS.load(Ordering::Relaxed)));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::{string::String, vec::Vec};

    const FW: [u32; PLANE_WORDS] = [0x6000_c007, 0, 0x4000_fff0, 0x0437_077f, 0x8000_01f1, 0, 0x3f40_0000, 0x1e00];

    fn fw() -> FwPlane { parse_fw_plane(0x877f_0437, 0x00ba_4124, &FW).unwrap() }

    #[test]
    fn opt_out() {
        assert!(plane_enabled("quiet v3d.probe=1"));
        assert!(!plane_enabled("quiet hvs.plane=0"));
        assert!(plane_enabled("hvs.plane=1"));
    }

    #[test]
    fn parses_the_measured_firmware_list() {
        let p = fw();
        assert_eq!((p.w, p.h, p.pitch, p.phys), (1920, 1080, 7680, 0x3f40_0000));
    }

    #[test]
    fn rejects_stopped_or_odd_channels() {
        assert!(parse_fw_plane(0x077f_0437, 0x00ba_4124, &FW).is_none()); // disabled
        assert!(parse_fw_plane(0x877f_0437, 0x00ba_0124, &FW).is_none()); // mode 0
        let mut w = FW; w[0] &= !CTL0_VALID;
        assert!(parse_fw_plane(0x877f_0437, 0x00ba_4124, &w).is_none());
        let mut w = FW; w[0] |= 4 << 20; // UIF
        assert!(parse_fw_plane(0x877f_0437, 0x00ba_4124, &w).is_none());
        let mut w = FW; w[0] &= !CTL0_UNITY;
        assert!(parse_fw_plane(0x877f_0437, 0x00ba_4124, &w).is_none());
    }

    #[test]
    fn list_changes_only_next_context_pointer_pitch_end() {
        let l = build_list(&fw(), 0x2_1234_5000, 7680);
        assert_eq!(l[0], 0x0800_c007 | CTL0_VALID);
        assert_eq!((l[1], l[2], l[3]), (FW[1], FW[2], FW[3]));
        assert_eq!(l[4], POS3_CONTEXT);
        assert_eq!((l[5], l[6], l[7], l[8]), (2, 0x1234_5000, 7680, CTL0_END));
    }

    #[test]
    fn bo_acceptance() {
        let p = fw();
        assert!(bo_matches(&p, 1920, 1080, 0x1_0000_0000, 7680));
        assert!(!bo_matches(&p, 1280, 720, 0x1_0000_0000, 5120));
        assert!(!bo_matches(&p, 1920, 1080, 0x1_0000_0008, 7680)); // unaligned
        assert!(!bo_matches(&p, 1920, 1080, 1 << 40, 7680)); // beyond 40 bits
        assert!(!bo_matches(&p, 1920, 1080, 0x1000, 7000)); // pitch < row
        assert!(!bo_matches(&p, 1920, 1080, 0x1000, 1 << 17));
    }

    #[test]
    fn frcnt_wraps_at_six_bits() {
        assert_eq!(frcnt_delta(0x3e, 0x01), 3);
        assert_eq!(frcnt_delta(5, 5), 0);
    }

    #[test]
    fn slots_skip_used_regions() {
        let used = [0x6000_c007u32; LIST_WORDS];
        let free = [0xb0b0_b0b0u32; LIST_WORDS];
        let got = pick_slots(2048, 32, 8, |o| if o == 2048 { used } else { free });
        assert_eq!(got, Some([2080, 2112]));
        assert_eq!(pick_slots(2048, 32, 8, |_| used), None);
    }

    // ---- flip event timing model ------------------------------------------------------------
    // The HVS latches a pending list at frame boundaries `B0 + k*P` (true period `P`). The driver
    // looks at it at the times `next_check_at` names; an observation can be delayed by `delay(n)`
    // (a busy worker). Lateness = observation time - the true latch time.

    const B0: u64 = 1_000_000_000;

    struct Sim { p: u64, phase: u64, late: Vec<u64> }

    impl Sim {
        fn new(p: u64) -> Sim { Sim { p, phase: 0, late: Vec::new() } }
        /// First boundary strictly after `t`.
        fn boundary_after(&self, t: u64) -> u64 { B0 + ((t - B0) / self.p + 1) * self.p }
        /// A flip issued at `issued`; `delay(n)` delays the n-th look (0-based). Returns the
        /// time the driver saw it latched; records its lateness and the new phase.
        fn flip(&mut self, issued: u64, delay: &dyn Fn(u32) -> u64) -> u64 {
            let latch = self.boundary_after(issued);
            let mut at = next_check_at(self.phase, issued);
            let mut n = 0u32;
            loop {
                let seen = at + delay(n);
                if seen >= latch {
                    self.late.push(seen - latch);
                    self.phase = latched_phase(self.phase, seen);
                    return seen;
                }
                at = next_check_at(self.phase, seen);
                n += 1;
                assert!(n < 100_000, "never saw the latch");
            }
        }
    }

    fn no_delay(_: u32) -> u64 { 0 }

    /// Steady state: a flip every frame, 60.000 Hz, prompt worker. Lateness stays small.
    #[test]
    fn steady_60hz_is_prompt() {
        let mut s = Sim::new(FRAME_NS);
        s.phase = B0; // a boundary was observed exactly
        let mut t = B0 + 2_000_000;
        for _ in 0..200 {
            let seen = s.flip(t, &no_delay);
            t = seen + 1_000_000;
        }
        let worst = *s.late.iter().max().unwrap();
        assert!(worst < 500_000, "worst lateness {} ns", worst);
    }

    #[test]
    fn frcnt_wrap_full_cycle() {
        // 0x3f -> 0 is one frame; a full 64-frame cycle is indistinguishable from none.
        assert_eq!(frcnt_delta(0x3f, 0), 1);
        assert_eq!(frcnt_delta(0x20, 0x1f), 0x3f);
        assert_eq!(frcnt_delta(7, 7 + 64 & 0x3f), 0);
    }

    #[test]
    fn frame_constant_is_60hz() {
        let hz = 1e9 / FRAME_NS as f64;
        assert!((hz - 60.0).abs() < 1e-5, "{}", hz);
    }

    #[test]
    fn lateness_buckets() {
        assert_eq!(late_bucket(0), 0);
        assert_eq!(late_bucket(249_999), 0);
        assert_eq!(late_bucket(250_000), 1);
        assert_eq!(late_bucket(7_999_999), 5);
        assert_eq!(late_bucket(8_000_000), 6);
        assert_eq!(late_bucket(u64::MAX), 6);
    }

    #[test]
    fn status_decode_firmware_sample() {
        let st = 0x00ba_4124;
        assert_eq!(status_line(st), 0x124);
        assert_eq!(status_mode(st), 2);
        assert_eq!(status_frcnt(st), 0x3a);
        assert_eq!(vtotal_guess(1080), 1125);
    }

    #[test]
    fn line_binning() {
        assert_eq!(line_bin(0, 1125), 0);
        assert_eq!(line_bin(140, 1125), 0);
        assert_eq!(line_bin(141, 1125), 1);
        assert_eq!(line_bin(1124, 1125), 7);
        assert_eq!(line_bin(1125, 1125), 8);
        assert_eq!(line_bin(5000, 1125), 8);
        assert_eq!(line_bin(3, 0), 8);
        assert_eq!(line_late_ns(540, 1080, 1125, 16_666_667), Some(8_000_000));
        assert_eq!(line_late_ns(1080, 1080, 1125, 16_666_667), None); // blanking
        assert_eq!(line_late_ns(0, 1080, 1125, 16_666_667), Some(0));
    }

    /// The stats are process-wide atomics: the tests that use them take this lock.
    static STATS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn status_recording_bins_and_ring() {
        let _g = STATS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        stats::reset();
        for i in 0..10u32 { stats::record_status(0x00ba_4000 | (i * 100), 1000 + i as u64, 1080, 1125, 16_666_667); }
        let mut t = String::new();
        stats::text(&mut t);
        assert!(t.contains("line_outside_active 0\n"), "{}", t); // lines 0..900 all active
        assert_eq!(t.matches("status_ring").count(), 8);
        assert!(t.contains("status_ring ktime_ns 1009 raw 0x00ba4384"), "{}", t);
        stats::record_status(0x00ba_4000 | 1100, 5, 1080, 1125, 16_666_667); // blanking
        let mut t = String::new();
        stats::text(&mut t);
        assert!(t.contains("line_outside_active 1\n"), "{}", t);
        stats::reset();
    }

    #[test]
    fn stats_text_and_reset() {
        let _g = STATS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        stats::reset();
        stats::record_event(Some(300_000));
        stats::record_event(Some(20_000_000));
        stats::record_event(None);
        stats::record_fence_wait(5_000);
        stats::record_fence_wait(9_000);
        let mut t = String::new();
        stats::text(&mut t);
        assert!(t.contains("events 3\n") && t.contains("missed 1\n"), "{}", t);
        assert!(t.contains("late_bound 0 1 0 0 0 0 1\n") && t.contains("late_unbounded 1\n"), "{}", t);
        assert!(t.contains("UPPER BOUND") && t.contains("UNVERIFIED"), "{}", t);
        assert!(t.contains("fence_wait_ns 14000 max 9000 count 2\n"), "{}", t);
        stats::reset();
        let mut t = String::new();
        stats::text(&mut t);
        assert!(t.contains("events 0\n") && t.contains("fence_wait_ns 0 max 0 count 0\n"), "{}", t);
    }

    /// (a) The first event is observed 8 ms late (worker delayed). The phase must not inherit that
    /// error: the next event must be less than 0.5 ms late. EXPECTED RED on the current logic
    /// (phase := observation time makes every later prediction 8 ms late).
    #[test]
    #[ignore = "red until hvsphase"]
    fn late_observation_does_not_poison_next_event() {
        let mut s = Sim::new(FRAME_NS);
        s.phase = B0;
        let seen = s.flip(B0 + 2_000_000, &no_delay);
        let _ = seen;
        // The observation of this flip is delayed by 8 ms ...
        let first = s.flip(B0 + 4 * FRAME_NS + 2_000_000, &|_| 8_000_000);
        assert!(*s.late.last().unwrap() >= 7_000_000);
        // ... and the next flip, with a prompt worker, must still be seen on time.
        s.flip(first + 1_000_000, &no_delay);
        let l = *s.late.last().unwrap();
        assert!(l < 500_000, "next event {} ns late", l);
    }

    /// (b) The panel's true period is 16.683 ms, not 16.666667: after an idle gap the predicted
    /// boundary drifts. 20, 60 and 300 idle frames: the next event must be <0.5 ms late.
    /// EXPECTED RED on the current logic.
    #[test]
    #[ignore = "red until hvsphase"]
    fn real_period_after_idle_is_prompt() {
        for idle in [20u64, 60, 300] {
            let p = 16_683_000;
            let mut s = Sim::new(p);
            s.phase = B0; // phase exactly right when the last event was seen
            let t = B0 + idle * p + 2_000_000;
            s.flip(t, &no_delay);
            let l = *s.late.last().unwrap();
            assert!(l < 500_000, "idle {} frames: {} ns late", idle, l);
        }
    }
}
