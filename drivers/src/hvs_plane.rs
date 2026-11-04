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

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub use hw::{console_restore as restore, flip, init, is_live};

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
            last_frcnt: (st >> 16) & 0x3f, retire: Vec::new(), flips: 0, waits: 0,
        });
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
        s.frames += frcnt_delta(s.last_frcnt, f) as u64;
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
            // A list written mid-frame latches at the next frame start, up to one period away:
            // sleep most of 6 ms in one go, then poll every 2 ms (few wakeups beat tight polling).
            let since = now.wrapping_sub(s.issued_ns);
            sched::kthread::sleep_ns(if since < 6_000_000 { 6_000_000 - since } else { 2_000_000 });
        }
    }

    fn give_up(why: &str) {
        DEAD.store(true, Ordering::Relaxed);
        console_restore();
        serial_debug(&format!("[hvs] plane off, falling back to the copy sink: {}\n", why));
    }

    /// Show the BO window (`obj` pinned by the caller's framebuffer) on channel 0. Returns once the
    /// list is written; the previous flip is waited for first, so calls are paced to the display.
    pub fn flip(obj: u32, phys: u64, w: u32, h: u32, pitch: u32) -> Result<(), DriverError> {
        if DEAD.load(Ordering::Relaxed) { return Err(DriverError::NotFound); }
        let mut g = STATE.lock();
        let s = g.as_mut().ok_or(DriverError::NotFound)?;
        if !bo_matches(&s.fw, w, h, phys, pitch) { return Err(DriverError::InvalidParameter); }
        if !ACTIVE.load(Ordering::Acquire) { s.pending = false; }
        poll(s);
        if s.pending {
            let cur = s.slots[s.cur];
            if !wait_latch(s, cur) {
                give_up(&format!("slot {} never became active (lact={:#x})", cur, rd(CH_DL) & 0xfff));
                return Err(DriverError::Io);
            }
            s.pending = false;
            poll(s);
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
        s.issued_ns = sched::monotonic_ns();
        s.flips += 1;
        if s.flips == 1 {
            // Prove the takeover took before trusting it for good.
            if !wait_latch(s, off) {
                give_up("first list never latched");
                return Err(DriverError::Io);
            }
            s.pending = false;
        }
        if crate::kms_v3d::flip_log_due(s.flips) {
            serial_debug(&format!(
                "[hvs] flip {} slot={} ptr={:#x} pitch={} lptrs={:#x} lact={:#x} frcnt={} frames={} waits={}\n",
                s.flips, off, phys, pitch, rd(CH_LPTRS) & 0xfff, rd(CH_DL) & 0xfff, s.last_frcnt, s.frames, s.waits));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
