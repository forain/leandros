//! Intel Gen9 / Gen9.5 integrated graphics (Skylake .. Comet Lake; the
//! target is the UHD Graphics 620 of an i5-8350U laptop, PCI 8086:5917).
//!
//! Milestones (see artifacts/notes/lane-i915-2026-10-11.md):
//!
//!   G0 `probe`  — always (unless `i915.off=1`): config space, BARs, stolen
//!                 memory, GGTT, OpRegion/VBT, display state the firmware
//!                 (GOP) left, forcewake handshake, fuses/topology, clocks,
//!                 engine registers — logged, nothing reprogrammed except the
//!                 forcewake request it takes and gives back.
//!   G1/G2 `gt`  — only on request (`i915.selftest=` or `i915.start=1`):
//!                 GT reset, private PAT, GGTT + 4-level PPGTT allocations,
//!                 execlists on the blitter engine, in-kernel self-tests with
//!                 a hang detector and engine reset, so a bad batch cannot
//!                 wedge the boot.
//!
//! The hardware-independent half (`hw`) is host-tested in drivers/i915-tests.
//!
//! Kernel command line (Limine `cmdline:`), all optional:
//!   `i915.off=1`             nothing at all (not even the probe)
//!   `i915.probe=0`           skip the G0 probe (the self-test still runs if asked)
//!   `i915.dumpvbt=0`         do not base64-dump the VBT into the klog (default 1)
//!   `i915.ggttscan=0`        skip the GGTT occupancy scan (default 1)
//!   `i915.selftest=LIST`     comma-separated, or `all`: store = execlists on
//!                            the blitter + MI_STORE_DWORD_IMM into the status
//!                            page; blt = XY_COLOR_BLT into a scratch buffer,
//!                            verified by the CPU; fb = colour bars blitted
//!                            into the visible GOP framebuffer (blt and fb
//!                            imply store on bcs0); rcs = the store test on
//!                            the render engine (PIPE_CONTROL breadcrumb)
//!   `i915.start=1`           engine bring-up + the `store` self-test (until
//!                            the DRM UAPI backend exists this is all "start" does)
//!   `i915.selftest_hold_ms=N` how long the fb bars stay before boot goes on (1500)
//!   `i915.reset=0`           do not reset the GT before the bring-up (default 1)
//!   `i915.timeout_ms=N`      per-test budget (default 500)
//!   `i915.fake=1`            (testing, QEMU) run the bring-up and self-tests
//!                            against RAM that stands in for a KBL GT2's
//!                            registers and GGTT: nothing ever completes, so
//!                            every timeout, the hang detector, the state dump
//!                            and the engine-reset path run end to end;
//!                            then registers the G3 UAPI with the soft
//!                            command streamer (`uapi.rs`, `softcs.rs`)
//!   `i915.fake=2`            (testing) the fake device's G3 UAPI only, no
//!                            bring-up (what /bin/i915test runs against)
//!   `i915.uapi=1`            real hardware: register the G3 UAPI node
//!                            (card0/renderD128 answer `i915`); EXECBUFFER2
//!                            answers ENODEV until hardware submission from
//!                            the UAPI is wired. `i915.uapi=0` with
//!                            `i915.fake` keeps the node unregistered
//!   `i915.uapi_log=N`        0 silent, 1 errors (default), 2 every ioctl

use alloc::format;
use alloc::string::String;

pub mod hw;
pub mod uapi_abi;
pub mod softcs;
pub mod vmbind;
pub mod wa;
pub mod gen9_renderstate;
mod probe;
mod gt;
mod uapi;
pub use uapi::{prime_dedup, ptecheck_dmabuf_mmap, ptecheck_fd_to_handle, set_tmpfs_census};
mod exec;
mod kms;

use hw::reg;

// ── kernel interface ─────────────────────────────────────────────────────────

extern "C" {
    fn serial_print_str_raw(s: *const u8, len: usize);
    fn klog_write_only(s: *const u8, len: usize);
    fn arch_monotonic_ns() -> u64;
}

/// What the kernel knows about the boot framebuffer.
pub struct BootEnv {
    pub fb_phys: u64,
    pub fb_width: u32,
    pub fb_height: u32,
    pub fb_pitch: u32,
}

pub(crate) fn emit(s: &str) { unsafe { serial_print_str_raw(s.as_ptr(), s.len()); } }
pub(crate) fn emit_logonly(s: &str) { unsafe { klog_write_only(s.as_ptr(), s.len()); } }

/// One console + klog line, prefixed `[i915] `.
pub(crate) fn log(msg: &str) {
    emit("[i915] ");
    emit(msg);
    emit("\n");
}

/// One klog-only line (bulk register dumps): prefixed `[i915] ` too, so a
/// single grep finds everything.
pub(crate) fn dump(msg: &str) {
    emit_logonly("[i915] ");
    emit_logonly(msg);
    emit_logonly("\n");
}

macro_rules! ilog { ($($a:tt)*) => { $crate::i915::log(&alloc::format!($($a)*)) }; }
macro_rules! idump { ($($a:tt)*) => { $crate::i915::dump(&alloc::format!($($a)*)) }; }
pub(crate) use ilog;
pub(crate) use idump;

pub(crate) fn now_ns() -> u64 { unsafe { arch_monotonic_ns() } }

/// Bounded busy-wait; falls back to an iteration cap if the clock is stuck.
pub(crate) fn delay_us(us: u64) {
    let start = now_ns();
    for _ in 0..20_000_000u32 {
        if now_ns().wrapping_sub(start) >= us * 1000 { return; }
        core::hint::spin_loop();
    }
}

pub(crate) fn opt(key: &str) -> Option<u64> { crate::nvidia_probe::cmdline_opt(key) }
pub(crate) fn word(key: &str) -> Option<&'static str> { crate::nvidia_probe::cmdline_word(key) }

pub(crate) fn fmt_size(n: u64) -> String {
    if n >= 1 << 30 && n % (1 << 30) == 0 { format!("{} GiB", n >> 30) }
    else if n >= 1 << 20 && n % (1 << 20) == 0 { format!("{} MiB", n >> 20) }
    else if n >= 1 << 10 && n % (1 << 10) == 0 { format!("{} KiB", n >> 10) }
    else { format!("{} B", n) }
}

// ── MMIO ─────────────────────────────────────────────────────────────────────

/// The register half of BAR0 (GTTMMADR), mapped uncached.
#[derive(Clone, Copy)]
pub(crate) struct Mmio { pub va: usize }

impl Mmio {
    #[inline]
    pub fn r(&self, off: u32) -> u32 {
        debug_assert!((off as usize) < hw::MMIO_SIZE);
        unsafe { core::ptr::read_volatile((self.va + off as usize) as *const u32) }
    }
    #[inline]
    pub fn w(&self, off: u32, v: u32) {
        unsafe { core::ptr::write_volatile((self.va + off as usize) as *mut u32, v) }
    }
    /// Poll until `(r(off) & mask) == want` or `timeout_us` passes. Returns
    /// the elapsed microseconds on success.
    pub fn wait(&self, off: u32, mask: u32, want: u32, timeout_us: u64) -> Result<u64, u32> {
        let t0 = now_ns();
        loop {
            let v = self.r(off);
            if v & mask == want { return Ok(now_ns().wrapping_sub(t0) / 1000); }
            if now_ns().wrapping_sub(t0) >= timeout_us * 1000 { return Err(v); }
            core::hint::spin_loop();
        }
    }
}

/// The GGTT page table (upper half of BAR0), mapped uncached; 8-byte PTEs.
#[derive(Clone, Copy)]
pub(crate) struct Gsm { pub va: usize, pub entries: u64 }

impl Gsm {
    pub fn read(&self, idx: u64) -> u64 {
        if idx >= self.entries { return 0; }
        unsafe { core::ptr::read_volatile((self.va + idx as usize * 8) as *const u64) }
    }
    pub fn write(&self, idx: u64, pte: u64) {
        if idx >= self.entries { return; }
        unsafe { core::ptr::write_volatile((self.va + idx as usize * 8) as *mut u64, pte) }
    }
}

// ── forcewake ────────────────────────────────────────────────────────────────

/// Take every gen9 forcewake domain (GT, render, media) with the kernel bit
/// (`fw_domain_get` + `fw_domain_wait_ack_set`). Returns the per-domain ack
/// latency in µs, or the name of the domain that did not ack.
pub(crate) fn forcewake_get(m: &Mmio) -> Result<[u64; 3], &'static str> {
    let mut lat = [0u64; 3];
    for (i, &(name, req, ack)) in hw::FW_DOMAINS.iter().enumerate() {
        // A stale ack from a previous owner must clear first (50 ms like Linux).
        let _ = m.wait(ack, reg::FORCEWAKE_KERNEL, 0, 50_000);
        m.w(req, hw::masked_en(reg::FORCEWAKE_KERNEL));
        match m.wait(ack, reg::FORCEWAKE_KERNEL, reg::FORCEWAKE_KERNEL, 50_000) {
            Ok(us) => lat[i] = us,
            Err(_) => {
                // Linux's gen9 fallback: pulse the fallback bit and retry once.
                m.w(req, hw::masked_en(reg::FORCEWAKE_KERNEL_FALLBACK));
                let ok = m.wait(ack, reg::FORCEWAKE_KERNEL, reg::FORCEWAKE_KERNEL, 50_000).is_ok();
                m.w(req, hw::masked_dis(reg::FORCEWAKE_KERNEL_FALLBACK));
                if !ok {
                    forcewake_put(m);
                    return Err(name);
                }
                lat[i] = 50_000;
            }
        }
    }
    Ok(lat)
}

pub(crate) fn forcewake_put(m: &Mmio) {
    for &(_, req, _) in hw::FW_DOMAINS.iter() {
        m.w(req, hw::masked_dis(reg::FORCEWAKE_KERNEL));
    }
}

// ── entry point ──────────────────────────────────────────────────────────────

/// What the probe learned; the bring-up consumes it.
pub(crate) struct Device {
    pub func: crate::pci::PciFunction,
    pub platform: hw::Platform,
    pub mmio: Mmio,
    pub gsm: Option<Gsm>,
    pub gmch: hw::Gmch,
    pub stolen_base: u64,
    pub gmadr_base: u64,
    pub gmadr_size: u64,
    /// GGTT offset of the boot framebuffer (= its aperture offset), if the
    /// framebuffer lies in GMADR.
    pub fb_ggtt: Option<u64>,
    /// `i915.fake=1`: registers and GGTT are plain RAM, there is no PCI function.
    pub fake: bool,
}

/// `i915.fake=1`: a Device whose "MMIO" and "GGTT" are zeroed RAM (2 MiB and
/// 8 MiB from the buddy allocator) with the forcewake acks preset, so the
/// bring-up runs to its first wait and then through the hang path.
fn fake_device() -> Option<Device> {
    let mmio = mm::buddy::alloc(9)?;
    let gsm = mm::buddy::alloc(11)?;
    let mva = mm::phys_to_virt(mmio);
    let gva = mm::phys_to_virt(gsm);
    unsafe {
        core::ptr::write_bytes(mva as *mut u8, 0, 2 << 20);
        core::ptr::write_bytes(gva as *mut u8, 0, 8 << 20);
    }
    let m = Mmio { va: mva };
    for &(_, _, ack) in hw::FW_DOMAINS.iter() { m.w(ack, reg::FORCEWAKE_KERNEL); }
    let gmch = hw::decode_ggc(0x02C1);
    Some(Device {
        func: crate::pci::PciFunction::new(0xFF, 0x1F, 7),
        platform: hw::identify(0x5917)?,
        mmio: m,
        gsm: Some(Gsm { va: gva, entries: gmch.ggtt_entries }),
        gmch, stolen_base: 0, gmadr_base: 0, gmadr_size: 256 << 20, fb_ggtt: None, fake: true,
    })
}

/// Run the probe and, when asked, the engine bring-up. Called once from
/// kernel init (task context, before userspace), after `nvidia_probe::set_cmdline`.
pub fn run(env: &BootEnv) {
    if opt("i915.off") == Some(1) { return; }
    if let Some(fk @ (1 | 2)) = opt("i915.fake") {
        let d = match fake_device() { Some(d) => d, None => { log("i915.fake: out of memory"); return; } };
        if fk == 1 {
            log("i915.fake=1: bring-up against RAM standing in for a KBL GT2 (testing only; expect a reported hang)");
            gt::bringup_and_selftest(&d, env, gt::Tests { store: true, blt: true, fb: true, rcs: true });
            if opt("i915.exec") == Some(1) {
                // Exercises the G3b bring-up against RAM: the golden-context
                // request cannot complete, so execution stays off.
                let ok = exec::init(&d, &uapi::kbl_gt2_sseu(), uapi::retire);
                ilog!("i915.fake=1 i915.exec=1: exec bring-up returned {} (expected false on RAM)", ok);
            }
        }
        if opt("i915.uapi") != Some(0) {
            uapi::register(&d, uapi::Mode::Fake, uapi::kbl_gt2_sseu(), 12_000_000);
            kms::init(&d, env);
        }
        return;
    }
    let dev = match probe::run(env) {
        Some(d) => d,
        None => return,
    };
    let mut t = match word("i915.selftest") {
        Some(list) => match parse_tests(list) {
            Ok(t) => t,
            Err(bad) => { ilog!("unknown i915.selftest item '{}' (want store,blt,fb,rcs or all); engines left alone", bad); return; }
        },
        None => gt::Tests::default(),
    };
    // G3: the i915 UAPI node, only on request. G3b: with `i915.exec=1` the
    // engines are brought up for user batches first (after the G2
    // self-tests, which own the engines while they run); without it, or if
    // that bring-up fails, EXECBUFFER2 answers ENODEV.
    if opt("i915.uapi") == Some(1) {
        if dev.platform.family.is_lp() || !dev.platform.has_llc() {
            log("i915.uapi=1: Gen9 LP (no LLC) parts are not supported; UAPI not registered");
        } else {
            let (s, hz) = probe::gt_facts(&dev);
            if opt("i915.exec") == Some(1) {
                if opt("i915.start") == Some(1) { t.store = true; }
                if t.store || t.blt || t.fb || t.rcs { gt::bringup_and_selftest(&dev, env, t); }
                if !exec::init(&dev, &s, uapi::retire) { log("G3b exec bring-up failed; EXECBUFFER2 stays ENODEV"); }
            }
            uapi::register(&dev, uapi::Mode::Hw, s, hz);
            kms::init(&dev, env);
            if opt("i915.exec") == Some(1) { return; }
        }
    }
    if opt("i915.start") == Some(1) { t.store = true; }
    if !(t.store || t.blt || t.fb || t.rcs) {
        log("engines untouched (default; i915.selftest=store,blt,fb,rcs or i915.start=1 to bring them up)");
        return;
    }
    gt::bringup_and_selftest(&dev, env, t);
}

/// `i915.selftest=` value: comma-separated store, blt, fb, rcs, or all.
fn parse_tests(list: &str) -> Result<gt::Tests, &str> {
    let mut t = gt::Tests::default();
    for item in list.split(',') {
        match item {
            "store" => t.store = true,
            "blt" => t.blt = true,
            "fb" => t.fb = true,
            "rcs" => t.rcs = true,
            "all" => { t.store = true; t.blt = true; t.fb = true; t.rcs = true; }
            "" => {}
            other => return Err(other),
        }
    }
    Ok(t)
}
