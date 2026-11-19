//! Module-side kernel-environment seam for the i915 driver: the same paths the built-in build gets
//! from `drivers/src/kenv.rs`, implemented over kmod-kapi. The KMS / uapi tables are stored here and
//! handed to the kernel as static `extern "C"` trampoline tables (mirrors of `drivers/src/kms_class.rs`
//! and `drm_class.rs`).

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use kmod_kapi::ksync::KSpinLock;

#[derive(Debug, Clone, Copy)]
pub enum DriverError { NotFound, Io, Unsupported, InvalidParameter, Access, Busy, NotMaster, Errno(i32) }

fn errno_of(e: DriverError) -> i32 {
    use DriverError::*;
    match e { NotFound => -2, Io => -5, Unsupported => -95, InvalidParameter => -22, Access | NotMaster => -13, Busy => -16, Errno(n) => -n }
}
fn from_errno(r: i32) -> DriverError {
    match -r { 2 => DriverError::NotFound, 5 => DriverError::Io, 95 => DriverError::Unsupported, 22 => DriverError::InvalidParameter,
               13 => DriverError::Access, 16 => DriverError::Busy, n => DriverError::Errno(n) }
}
fn st(r: Result<(), DriverError>) -> i32 { match r { Ok(()) => 0, Err(e) => errno_of(e) } }

/// Set by the first successful table registration: after that the module can no longer unload.
static REGISTERED: AtomicBool = AtomicBool::new(false);
pub fn registered() -> bool { REGISTERED.load(Ordering::SeqCst) }
fn note(r: i32) { if r == 0 { REGISTERED.store(true, Ordering::SeqCst); } }

// ---- logging: the driver emits partial lines; kapi_log adds the newline ----
static LINE: KSpinLock<String> = KSpinLock::new(String::new());
pub fn serial_raw(s: &str) { kmod_kapi::serial_raw(s) }
pub fn emit(s: &str) {
    let mut l = LINE.lock();
    l.push_str(s);
    while let Some(i) = l.find('\n') {
        let line: String = l[..i].into();
        let rest: String = l[i + 1..].into();
        *l = rest;
        kmod_kapi::serial_raw(&line);
    }
}
pub fn emit_logonly(s: &str) { emit(s) }
pub fn now_ns() -> u64 { kmod_kapi::sched::monotonic_ns() }
pub fn delay_us(us: u64) { kmod_kapi::sched::kthread::udelay(us) }

// ---- command line ----
pub fn cmdline_known() -> bool { true }
pub fn cmdline_opt(key: &str) -> Option<u64> {
    let v = cmdline_word(key)?;
    if let Some(h) = v.strip_prefix("0x") { u64::from_str_radix(h, 16).ok() } else { v.parse().ok() }
}
pub fn cmdline_word(key: &str) -> Option<&'static str> {
    kmod_kapi::cmdline().split_ascii_whitespace().find_map(|t| t.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

pub mod irq { pub use kmod_kapi::irq::*; }

pub mod pci {
    use alloc::vec::Vec;
    use kmod_kapi::pci as k;
    pub use k::BarInfo;
    pub const CMD_MEMORY: u16 = 1 << 1;
    pub const CMD_BUS_MASTER: u16 = 1 << 2;

    pub struct BarSizing { pub bars: Vec<BarInfo>, pub rom_orig: u32, pub rom_size: u64 }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PciFunction { f: k::PciFunction, pub bus: u8, pub dev: u8, pub func: u8 }
    impl core::ops::Deref for PciFunction { type Target = k::PciFunction; fn deref(&self) -> &k::PciFunction { &self.f } }
    impl From<PciFunction> for u32 { fn from(f: PciFunction) -> u32 { f.f.bdf() } }
    impl PciFunction {
        pub const fn new(bus: u8, dev: u8, func: u8) -> Self { Self { f: k::PciFunction::new(bus, dev, func), bus, dev, func } }
        pub fn vendor_id(&self) -> u16 { self.f.read16(0x00) }
        pub fn device_id(&self) -> u16 { self.f.read16(0x02) }
        pub fn command(&self) -> u16 { self.f.read16(0x04) }
        pub fn set_command(&self, v: u16) { self.f.write16(0x04, v) }
        pub fn enable_memory(&self) { let c = self.command(); if c & CMD_MEMORY == 0 { self.set_command(c | CMD_MEMORY); } }
        pub fn set_bus_master(&self, on: bool) {
            let c = self.command();
            self.set_command(if on { c | CMD_BUS_MASTER } else { c & !CMD_BUS_MASTER });
        }
        pub fn capabilities(&self) -> Vec<(u8, u16)> {
            let mut out = Vec::new();
            if self.f.read16(0x06) & (1 << 4) == 0 { return out; }
            let mut p = (self.f.read8(0x34) & !3) as u16;
            let mut guard = 0;
            while p >= 0x40 && guard < 48 { guard += 1; out.push((self.f.read8(p), p)); p = (self.f.read8(p + 1) & !3) as u16; }
            out
        }
        /// Diagnostics only (the built-in logs them); not walked from a module.
        pub fn ext_capabilities(&self) -> Vec<(u16, u8, u16)> { Vec::new() }
        pub fn size_bars(&self) -> BarSizing {
            let mut bars = Vec::new();
            let mut i = 0;
            while i < 6 {
                if let Some(mut b) = self.f.bar(i) {
                    if !b.io { b.size = self.f.bar_size(i); }
                    let wide = b.is64;
                    if b.io || b.base != 0 || b.size != 0 { bars.push(b); }
                    if wide { i += 1; }
                }
                i += 1;
            }
            BarSizing { bars, rom_orig: 0, rom_size: 0 }
        }
    }

    #[derive(Debug, Clone, Copy)]
    pub struct PciDevice(k::PciDevice);
    impl core::ops::Deref for PciDevice { type Target = k::PciDevice; fn deref(&self) -> &k::PciDevice { &self.0 } }
    impl PciDevice { pub fn function(&self) -> PciFunction { PciFunction::new(self.0.bus, self.0.dev, self.0.func) } }
    pub fn scan() -> Vec<PciDevice> { k::scan().into_iter().map(PciDevice).collect() }
}

pub mod mm {
    pub use kmod_kapi::mm::phys_to_virt;
    pub mod kva {
        pub use kmod_kapi::mm::kva::*;
        pub fn iounmap(va: usize) { kmod_kapi::sys_common::iounmap(va, 0) }
    }
    pub mod buddy {
        pub use kmod_kapi::mm::buddy::*;
        pub fn largest_free_order() -> Option<usize> { None }
    }
    pub use kmod_kapi::mm::pageref;
}

pub mod sched {
    pub use kmod_kapi::sched::monotonic_ns;
    pub fn current_pid() -> u32 { kmod_kapi::sys_common::current_pid() }
    pub mod kthread {
        pub use kmod_kapi::sched::kthread::{msleep, spawn};
        pub fn sleep_ns(ns: u64) { kmod_kapi::sys_common::sleep_until(super::monotonic_ns().saturating_add(ns)) }
        /// The module is permanent: its threads never stop.
        pub fn should_stop() -> bool { false }
    }
    pub mod ksync {
        pub use kmod_kapi::ksync::{KSemaphore, KSpinLock};
        pub struct KMutex<T>(kmod_kapi::ksync::KMutex<T>);
        impl<T> KMutex<T> {
            pub const fn new(v: T) -> Self { Self(kmod_kapi::ksync::KMutex::new("i915", v)) }
            pub fn lock(&self) -> kmod_kapi::ksync::KMutexGuard<'_, T> { self.0.lock() }
            pub fn try_lock(&self) -> Option<kmod_kapi::ksync::KMutexGuard<'_, T>> { self.0.try_lock() }
        }
    }
    pub mod hrtimer {
        use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
        #[derive(Clone, Copy)]
        pub enum TimerMode { Irq, Task }
        pub struct HrTimer { h: AtomicUsize, cb: extern "C" fn(usize), ctx: usize, irq: bool, armed: AtomicBool, expires: AtomicU64 }
        extern "C" fn fire(p: usize) {
            let t = unsafe { &*(p as *const HrTimer) };
            t.armed.store(false, Ordering::Release);
            (t.cb)(t.ctx);
        }
        impl HrTimer {
            pub fn new(cb: extern "C" fn(usize), ctx: usize, mode: TimerMode) -> alloc::boxed::Box<HrTimer> {
                let b = alloc::boxed::Box::new(HrTimer { h: AtomicUsize::new(0), cb, ctx, irq: matches!(mode, TimerMode::Irq),
                                                          armed: AtomicBool::new(false), expires: AtomicU64::new(0) });
                let p = &*b as *const HrTimer as usize;
                let h = unsafe { kmod_abi::sys::kapi_hrtimer_new(Some(fire), p, b.irq) };
                b.h.store(h, Ordering::Release);
                b
            }
            pub fn is_armed(&self) -> bool { self.armed.load(Ordering::Acquire) }
            pub fn expires(&self) -> u64 { self.expires.load(Ordering::Acquire) }
            pub fn start_at(&self, at: u64, _period: u64) -> bool {
                self.expires.store(at, Ordering::Release);
                self.armed.store(true, Ordering::Release);
                unsafe { kmod_abi::sys::kapi_hrtimer_arm(self.h.load(Ordering::Acquire), at.saturating_sub(super::monotonic_ns())) };
                true
            }
        }
    }
    pub mod uaccess {
        pub unsafe fn copy_from_user(dst: *mut u8, src: usize, len: usize) -> usize {
            if len == 0 { return 0; }
            match kmod_kapi::sys_common::copy_from_user(core::slice::from_raw_parts_mut(dst, len), src) { Ok(()) => 0, Err(_) => len }
        }
        pub unsafe fn copy_to_user(dst: usize, src: *const u8, len: usize) -> usize {
            if len == 0 { return 0; }
            match kmod_kapi::sys_common::copy_to_user(dst, core::slice::from_raw_parts(src, len)) { Ok(()) => 0, Err(_) => len }
        }
        pub fn read_user<T: Copy>(src: usize) -> Option<T> {
            let mut v = core::mem::MaybeUninit::<T>::uninit();
            let n = core::mem::size_of::<T>();
            if unsafe { copy_from_user(v.as_mut_ptr() as *mut u8, src, n) } != 0 { return None; }
            Some(unsafe { v.assume_init() })
        }
        pub fn write_user<T: Copy>(dst: usize, v: T) -> bool {
            unsafe { copy_to_user(dst, &v as *const T as *const u8, core::mem::size_of::<T>()) == 0 }
        }
    }
}

/// DRM device-interface seam over the `kapi_*` exports.
pub mod drm {
    use super::*;
    pub fn ext_event_complete(cookie: u32, seq: u32, ts_ns: u64) { kmod_kapi::drm::kms_event_complete(cookie, seq, ts_ns) }
    pub fn ext_event_stats() -> (u64, u32) { let (a, b) = kmod_kapi::sys_common::ext_event_stats(); (a, b as u32) }
    pub fn ext_present_stats() -> (u64, u64, u64) { let s = kmod_kapi::drm::ext_present_stats(); (s[0], s[1], s[2]) }
    pub fn gpu_fence_irq_hint() { kmod_kapi::drm::fence_irq_hint() }
    pub fn proc_prime(s: &mut String) -> bool {
        let mut b = [0u8; 512];
        match kmod_kapi::sys_common::proc_prime(&mut b) {
            Ok(n) if n > 0 => { s.push_str(&String::from_utf8_lossy(&b[..n.min(512)])); true }
            _ => false,
        }
    }
    pub fn syncobj_exists(h: u32, open_id: u32) -> bool { kmod_kapi::drm::syncobj_exists(h, open_id) }
    pub fn syncobj_install(h: u32, open_id: u32, point: u64, fence: u64) -> Result<(), DriverError> {
        match kmod_kapi::drm::syncobj_install(h, open_id, point, fence) { 0 => Ok(()), e => Err(from_errno(e)) }
    }
    pub fn syncobj_in_fences(h: u32, open_id: u32, point: u64) -> Result<Vec<u64>, DriverError> {
        let mut v = alloc::vec![0u64; 32];
        let mut n = kmod_kapi::drm::syncobj_in_fences(h, open_id, point, &mut v);
        if n < 0 { return Err(from_errno(n as i32)); }
        if n as usize > v.len() {
            v = alloc::vec![0u64; n as usize];
            n = kmod_kapi::drm::syncobj_in_fences(h, open_id, point, &mut v);
            if n < 0 { return Err(from_errno(n as i32)); }
        }
        v.truncate(n as usize);
        Ok(v)
    }
    pub fn wait_fences(ids: &[u64], deadline_ns: u64) -> Result<(), DriverError> {
        match kmod_kapi::drm::fence_wait(ids, deadline_ns) { 0 => Ok(()), e => Err(from_errno(e)) }
    }
    pub fn offset_for(real: u64, size: u64) -> Option<u64> { kmod_kapi::drm::offset_alloc(real, size) }
    pub fn offset_release(real: u64) { kmod_kapi::drm::offset_release(real) }
}

pub fn hotplug(minor: u8) { kmod_kapi::drm::hotplug(minor) }

// ======================= KMS / uapi: POD types + trampoline tables =======================

#[path = "../../../drivers/src/gpu_kms.rs"]
#[allow(dead_code)]
mod gpu_kms;
#[path = "../../../drivers/src/gpu_uapi.rs"]
#[allow(dead_code)]
mod gpu_uapi;

const KMS_NOT_MINE: i64 = i64::MIN;
const KDRM_NOT_MINE: i64 = i64::MIN;

fn leak<T>(v: T) -> &'static T { alloc::boxed::Box::leak(alloc::boxed::Box::new(v)) }

pub mod kms {
    use super::*;
    pub use super::gpu_kms::{KmsBackend, KmsConnInfo, KmsCursor, KmsCursorSrc, KmsFbReq, KmsFlip, KmsOutputs, KmsPipe2, KmsTiming,
                             EXT_CONN_BASE, EXT_ENC_BASE, EXT_MAX};
    use kmod_kapi::drm as kd;

    #[repr(C)] #[derive(Clone, Copy, Default)]
    struct KFbReq { handle: u32, width: u32, height: u32, pitch: u32, offset: u32, fourcc: u32, has_modifier: bool, modifier: u64 }
    #[repr(C)] #[derive(Clone, Copy, Default)]
    struct KCursorSrc { token: u32, phys: u64, pitch: u32, width: u32, height: u32 }
    #[repr(C)] #[derive(Clone, Copy, Default)]
    struct KTiming { clock_khz: u32, h: [u16; 4], v: [u16; 4], flags: u32 }
    #[repr(C)] #[derive(Clone, Copy, Default)]
    struct KConnInfo { conn_type: u32, type_id: u32, enc_type: u32, connected: bool, mm_w: u32, mm_h: u32, modes: [KTiming; 4], nmodes: usize, edid: [[u8; 16]; 8], edid_len: usize }

    #[repr(C)] #[derive(Clone, Copy)]
    struct KBackendOps {
        size: u32, name: *const u8, name_len: usize,
        mode: extern "C" fn(usize, *mut u32),
        modifiers: *const u64, n_modifiers: usize,
        fb_create: extern "C" fn(usize, *const KFbReq, u32) -> i64,
        fb_destroy: extern "C" fn(usize, u32),
        present: extern "C" fn(usize, u32, u32, u32, bool, *const [i32; 4], usize) -> i32,
        console_restore: Option<extern "C" fn(usize)>,
    }
    #[repr(C)] #[derive(Clone, Copy)]
    struct KFlipOps {
        size: u32,
        queue: extern "C" fn(usize, u32, u32, u32, u32) -> i32,
        event: extern "C" fn(usize, u32),
        busy: extern "C" fn(usize) -> bool,
        wait_idle: extern "C" fn(usize, u64) -> bool,
        queue_async: Option<extern "C" fn(usize, u32, u32, u32, u32) -> i32>,
        frame: Option<extern "C" fn(usize, *mut u64)>,
        wait_frame: Option<extern "C" fn(usize, u32, u64, *mut u64) -> bool>,
    }
    #[repr(C)] #[derive(Clone, Copy)]
    struct KCursorOps {
        size: u32, cursor_size: u32,
        set: extern "C" fn(usize, *const KCursorSrc, i32, i32) -> bool,
        move_to: extern "C" fn(usize, i32, i32),
        hide: extern "C" fn(usize),
    }
    #[repr(C)] #[derive(Clone, Copy)]
    struct KOutputsOps {
        size: u32,
        count: extern "C" fn(usize) -> usize,
        info: extern "C" fn(usize, usize, *mut KConnInfo) -> bool,
        cached: extern "C" fn(usize, usize, *mut KConnInfo) -> bool,
        crtc_mask: extern "C" fn(usize, usize) -> u32,
    }
    #[repr(C)] #[derive(Clone, Copy)]
    struct KPipe2Ops {
        size: u32, name: *const u8, name_len: usize,
        modeset: extern "C" fn(usize, usize, *const KTiming) -> i32,
        disable: extern "C" fn(usize),
        queue: extern "C" fn(usize, u32, u32, u32, u32) -> i32,
        event: extern "C" fn(usize, u32),
        busy: extern "C" fn(usize) -> bool,
        wait_idle: extern "C" fn(usize, u64) -> bool,
        mode: extern "C" fn(usize, *mut u32) -> bool,
        dumb_ok: bool, has_cursor: bool,
    }

    static BE: AtomicPtr<KmsBackend> = AtomicPtr::new(core::ptr::null_mut());
    static FL: AtomicPtr<KmsFlip> = AtomicPtr::new(core::ptr::null_mut());
    static CU: [AtomicPtr<KmsCursor>; 2] = [AtomicPtr::new(core::ptr::null_mut()), AtomicPtr::new(core::ptr::null_mut())];
    static OU: AtomicPtr<KmsOutputs> = AtomicPtr::new(core::ptr::null_mut());
    static P2: AtomicPtr<KmsPipe2> = AtomicPtr::new(core::ptr::null_mut());
    macro_rules! g { ($s:expr) => { unsafe { &*$s.load(Ordering::Acquire) } }; }

    fn tim(t: &KTiming) -> KmsTiming { KmsTiming { clock_khz: t.clock_khz, h: t.h, v: t.v, flags: t.flags } }
    fn ctim(t: &KmsTiming) -> KTiming { KTiming { clock_khz: t.clock_khz, h: t.h, v: t.v, flags: t.flags } }
    fn cinfo(i: &KmsConnInfo) -> KConnInfo {
        let mut m = [KTiming::default(); 4];
        for k in 0..4 { m[k] = ctim(&i.modes[k]); }
        KConnInfo { conn_type: i.conn_type, type_id: i.type_id, enc_type: i.enc_type, connected: i.connected,
                    mm_w: i.mm_w, mm_h: i.mm_h, modes: m, nmodes: i.nmodes.min(4), edid: i.edid, edid_len: i.edid_len }
    }
    #[allow(dead_code)] fn _unused(t: &KTiming) -> KmsTiming { tim(t) }

    extern "C" fn b_mode(_: usize, o: *mut u32) { let (w, h, r) = (g!(BE).mode)(); unsafe { *o = w; *o.add(1) = h; *o.add(2) = r; } }
    extern "C" fn b_fb_create(_: usize, r: *const KFbReq, open_id: u32) -> i64 {
        let r = unsafe { &*r };
        let q = KmsFbReq { handle: r.handle, width: r.width, height: r.height, pitch: r.pitch, offset: r.offset, fourcc: r.fourcc,
                           modifier: if r.has_modifier { Some(r.modifier) } else { None } };
        match (g!(BE).fb_create)(&q, open_id) { None => KMS_NOT_MINE, Some(Ok(t)) => t as i64, Some(Err(e)) => errno_of(e) as i64 }
    }
    extern "C" fn b_fb_destroy(_: usize, t: u32) { (g!(BE).fb_destroy)(t) }
    extern "C" fn b_present(_: usize, t: u32, sx: u32, sy: u32, has: bool, rects: *const [i32; 4], n: usize) -> i32 {
        let v: Vec<(i32, i32, i32, i32)> = if has && n > 0 { unsafe { core::slice::from_raw_parts(rects, n) }.iter().map(|r| (r[0], r[1], r[2], r[3])).collect() } else { Vec::new() };
        st((g!(BE).present)(t, sx, sy, if has { Some(&v[..]) } else { None }))
    }
    extern "C" fn b_restore(_: usize) { if let Some(f) = g!(BE).console_restore { f() } }

    extern "C" fn f_queue(_: usize, t: u32, sx: u32, sy: u32, ev: u32) -> i32 { st((g!(FL).queue)(t, sx, sy, ev)) }
    extern "C" fn f_event(_: usize, ev: u32) { (g!(FL).event)(ev) }
    extern "C" fn f_busy(_: usize) -> bool { (g!(FL).busy)() }
    extern "C" fn f_wait(_: usize, ns: u64) -> bool { (g!(FL).wait_idle)(ns) }
    extern "C" fn f_queue_async(_: usize, t: u32, sx: u32, sy: u32, ev: u32) -> i32 {
        match g!(FL).queue_async { Some(f) => st(f(t, sx, sy, ev)), None => -22 }
    }
    extern "C" fn f_frame(_: usize, out: *mut u64) {
        if let Some(f) = g!(FL).frame { let (c, t) = f(); unsafe { *out = c as u64; *out.add(1) = t; } }
    }
    extern "C" fn f_wait_frame(_: usize, target: u32, ns: u64, out: *mut u64) -> bool {
        match g!(FL).wait_frame {
            Some(f) => match f(target, ns) { Some((c, t)) => { unsafe { *out = c as u64; *out.add(1) = t; } true } None => false },
            None => false,
        }
    }

    extern "C" fn cu_set<const N: usize>(_: usize, s: *const KCursorSrc, x: i32, y: i32) -> bool {
        let s = unsafe { &*s };
        (g!(CU[N]).set)(&KmsCursorSrc { token: s.token, phys: s.phys, pitch: s.pitch, width: s.width, height: s.height }, x, y)
    }
    extern "C" fn cu_move<const N: usize>(_: usize, x: i32, y: i32) { (g!(CU[N]).move_to)(x, y) }
    extern "C" fn cu_hide<const N: usize>(_: usize) { (g!(CU[N]).hide)() }

    extern "C" fn o_count(_: usize) -> usize { (g!(OU).count)() }
    extern "C" fn o_info(_: usize, i: usize, out: *mut KConnInfo) -> bool {
        match (g!(OU).info)(i) { Some(v) => { unsafe { *out = cinfo(&v); } true } None => false }
    }
    extern "C" fn o_cached(_: usize, i: usize, out: *mut KConnInfo) -> bool {
        match (g!(OU).cached)(i) { Some(v) => { unsafe { *out = cinfo(&v); } true } None => false }
    }
    extern "C" fn o_mask(_: usize, i: usize) -> u32 { (g!(OU).crtc_mask)(i) }

    extern "C" fn p_modeset(_: usize, i: usize, t: *const KTiming) -> i32 { st((g!(P2).modeset)(i, &tim(unsafe { &*t }))) }
    extern "C" fn p_disable(_: usize) { (g!(P2).disable)() }
    extern "C" fn p_queue(_: usize, t: u32, sx: u32, sy: u32, ev: u32) -> i32 { st((g!(P2).queue)(t, sx, sy, ev)) }
    extern "C" fn p_event(_: usize, ev: u32) { (g!(P2).event)(ev) }
    extern "C" fn p_busy(_: usize) -> bool { (g!(P2).busy)() }
    extern "C" fn p_wait(_: usize, ns: u64) -> bool { (g!(P2).wait_idle)(ns) }
    extern "C" fn p_mode(_: usize, o: *mut u32) -> bool {
        match (g!(P2).mode)() { Some((w, h, r)) => { unsafe { *o = w; *o.add(1) = h; *o.add(2) = r; } true } None => false }
    }

    fn reg<T>(kind: u32, t: &'static T) { note(kd::kms_register(kind, t as *const T as *const u8, 0)); }

    pub fn register(b: &'static KmsBackend) {
        BE.store(b as *const _ as *mut _, Ordering::Release);
        reg(kd::KMS_BACKEND, leak(KBackendOps {
            size: core::mem::size_of::<KBackendOps>() as u32, name: b.name.as_ptr(), name_len: b.name.len(), mode: b_mode,
            modifiers: b.modifiers.as_ptr(), n_modifiers: b.modifiers.len(), fb_create: b_fb_create, fb_destroy: b_fb_destroy,
            present: b_present, console_restore: if b.console_restore.is_some() { Some(b_restore) } else { None },
        }));
    }
    pub fn register_flip(f: &'static KmsFlip) {
        FL.store(f as *const _ as *mut _, Ordering::Release);
        reg(kd::KMS_FLIP, leak(KFlipOps { size: core::mem::size_of::<KFlipOps>() as u32, queue: f_queue, event: f_event, busy: f_busy, wait_idle: f_wait,
            queue_async: if g!(FL).queue_async.is_some() { Some(f_queue_async) } else { None },
            frame: if g!(FL).frame.is_some() { Some(f_frame) } else { None },
            wait_frame: if g!(FL).wait_frame.is_some() { Some(f_wait_frame) } else { None } }));
    }
    fn reg_cursor<const N: usize>(c: &'static KmsCursor, kind: u32) {
        CU[N].store(c as *const _ as *mut _, Ordering::Release);
        reg(kind, leak(KCursorOps { size: core::mem::size_of::<KCursorOps>() as u32, cursor_size: c.size,
                                    set: cu_set::<N>, move_to: cu_move::<N>, hide: cu_hide::<N> }));
    }
    pub fn register_cursor(c: &'static KmsCursor) { reg_cursor::<0>(c, kd::KMS_CURSOR) }
    pub fn register_outputs(o: &'static KmsOutputs) {
        OU.store(o as *const _ as *mut _, Ordering::Release);
        reg(kd::KMS_OUTPUTS, leak(KOutputsOps { size: core::mem::size_of::<KOutputsOps>() as u32, count: o_count, info: o_info, cached: o_cached, crtc_mask: o_mask }));
    }
    pub fn register_pipe2(p: &'static KmsPipe2) {
        P2.store(p as *const _ as *mut _, Ordering::Release);
        if let Some(c) = p.cursor { reg_cursor::<1>(c, kd::KMS_CURSOR2); }
        reg(kd::KMS_PIPE2, leak(KPipe2Ops {
            size: core::mem::size_of::<KPipe2Ops>() as u32, name: p.name.as_ptr(), name_len: p.name.len(), modeset: p_modeset, disable: p_disable,
            queue: p_queue, event: p_event, busy: p_busy, wait_idle: p_wait, mode: p_mode, dumb_ok: p.dumb_ok, has_cursor: p.cursor.is_some(),
        }));
    }
    pub fn set_internal_panel(v: bool) { kd::kms_set_internal_panel(v) }
}

pub mod uapi {
    use super::*;
    pub use super::gpu_uapi::{UapiBackend, UapiPrime, EXT_FENCE};

    #[repr(C)] #[derive(Clone, Copy, Default)]
    struct KPrime { phys: usize, order: usize, pages: usize, len: usize, obj: u32 }
    #[repr(C)] #[derive(Clone, Copy)]
    struct KDrmOps {
        size: u32, ver_major: i32, ver_minor: i32, ver_patch: i32, user_args: bool,
        name: *const u8, name_len: usize, date: *const u8, date_len: usize, desc: *const u8, desc_len: usize,
        ioctl: extern "C" fn(usize, u32, usize, u32) -> i64,
        gem_close: extern "C" fn(usize, u32, u32) -> i64,
        mmap: extern "C" fn(usize, u64, u32, *mut bool) -> u64,
        release_open: extern "C" fn(usize, u32),
        prime_export: extern "C" fn(usize, u32, u32, *mut KPrime) -> bool,
        prime_import: extern "C" fn(usize, u32, u32) -> u32,
        bo_release: extern "C" fn(usize, u32) -> bool,
        fence_done: extern "C" fn(usize, u64) -> bool,
        sysfs_attr: extern "C" fn(usize, *const u8, usize, *mut u8, usize) -> i64,
        mmap_pin: Option<extern "C" fn(usize, usize, usize, u32, *mut usize, usize) -> i64>,
        proc_text: Option<extern "C" fn(usize, *mut u8, usize) -> i64>,
        prime_dedup: Option<extern "C" fn(usize, u32) -> bool>,
        ptecheck_mmap: Option<extern "C" fn(usize, u32, u64, *const usize, usize)>,
        ptecheck_fd_to_handle: Option<extern "C" fn(usize, i32, u32, u32, u32)>,
    }

    static BK: AtomicPtr<UapiBackend> = AtomicPtr::new(core::ptr::null_mut());
    static PIN: AtomicUsize = AtomicUsize::new(0);
    macro_rules! b { () => { unsafe { &*BK.load(Ordering::Acquire) } }; }
    fn res(r: Option<Result<usize, DriverError>>) -> i64 {
        match r { None => KDRM_NOT_MINE, Some(Ok(v)) => v as i64, Some(Err(e)) => errno_of(e) as i64 }
    }

    extern "C" fn k_ioctl(_: usize, cmd: u32, arg: usize, open_id: u32) -> i64 { res((b!().ioctl)(cmd, arg, open_id)) }
    extern "C" fn k_gem_close(_: usize, h: u32, open_id: u32) -> i64 { res((b!().gem_close)(h, open_id)) }
    extern "C" fn k_mmap(_: usize, token: u64, open_id: u32, unc: *mut bool) -> u64 {
        match (b!().mmap)(token, open_id) { Some((p, u)) => { unsafe { *unc = u; } p } None => 0 }
    }
    extern "C" fn k_release_open(_: usize, open_id: u32) { (b!().release_open)(open_id) }
    extern "C" fn k_prime_export(_: usize, h: u32, open_id: u32, out: *mut KPrime) -> bool {
        match (b!().prime_export)(h, open_id) {
            Some(p) => { unsafe { *out = KPrime { phys: p.phys, order: p.order, pages: p.pages, len: p.len, obj: p.obj }; } true }
            None => false,
        }
    }
    extern "C" fn k_prime_import(_: usize, obj: u32, open_id: u32) -> u32 { (b!().prime_import)(obj, open_id).unwrap_or(0) }
    extern "C" fn k_bo_release(_: usize, obj: u32) -> bool { (b!().bo_release)(obj) }
    extern "C" fn k_fence_done(_: usize, f: u64) -> bool { (b!().fence_done)(f) }
    extern "C" fn k_sysfs(_: usize, a: *const u8, an: usize, out: *mut u8, cap: usize) -> i64 {
        let attr = unsafe { core::slice::from_raw_parts(a, an) };
        let o = unsafe { core::slice::from_raw_parts_mut(out, cap) };
        match (b!().sysfs_attr)(attr, o) { Some(n) => n as i64, None => -1 }
    }
    extern "C" fn k_mmap_pin(_: usize, token: usize, len: usize, open_id: u32, out: *mut usize, cap: usize) -> i64 {
        let f = PIN.load(Ordering::Acquire);
        if f == 0 { return KDRM_NOT_MINE; }
        let f: fn(usize, usize, u32) -> Option<Vec<usize>> = unsafe { core::mem::transmute(f) };
        match f(token, len, open_id) {
            None => KDRM_NOT_MINE,
            Some(v) => { let n = v.len().min(cap); unsafe { core::ptr::copy_nonoverlapping(v.as_ptr(), out, n); } n as i64 }
        }
    }
    extern "C" fn k_proc_text(_: usize, out: *mut u8, cap: usize) -> i64 {
        match crate::i915::proc_text() {
            None => -1,
            Some(s) => { let n = s.len().min(cap); unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), out, n); } s.len() as i64 }
        }
    }
    extern "C" fn k_prime_dedup(_: usize, obj: u32) -> bool { crate::i915::prime_dedup(obj) }
    extern "C" fn k_pte_mmap(_: usize, obj: u32, off: u64, frames: *const usize, n: usize) {
        crate::i915::ptecheck_dmabuf_mmap(obj, off, unsafe { core::slice::from_raw_parts(frames, n) })
    }
    extern "C" fn k_pte_fd(_: usize, fd: i32, open_id: u32, obj: u32, handle: u32) {
        crate::i915::ptecheck_fd_to_handle(fd, open_id, if obj == u32::MAX { None } else { Some(obj) }, handle)
    }

    pub fn register(bk: &'static UapiBackend) {
        BK.store(bk as *const _ as *mut _, Ordering::Release);
        let t = leak(KDrmOps {
            size: core::mem::size_of::<KDrmOps>() as u32, ver_major: bk.version.0, ver_minor: bk.version.1, ver_patch: bk.version.2,
            user_args: bk.user_args, name: bk.name.as_ptr(), name_len: bk.name.len(), date: bk.date.as_ptr(), date_len: bk.date.len(),
            desc: bk.desc.as_ptr(), desc_len: bk.desc.len(),
            ioctl: k_ioctl, gem_close: k_gem_close, mmap: k_mmap, release_open: k_release_open, prime_export: k_prime_export,
            prime_import: k_prime_import, bo_release: k_bo_release, fence_done: k_fence_done, sysfs_attr: k_sysfs,
            mmap_pin: Some(k_mmap_pin), proc_text: Some(k_proc_text), prime_dedup: Some(k_prime_dedup),
            ptecheck_mmap: Some(k_pte_mmap), ptecheck_fd_to_handle: Some(k_pte_fd),
        });
        note(kmod_kapi::drm::register(t as *const KDrmOps as *const u8, 0));
    }
    /// The driver's sys_mmap page-pin hook; resolved at call time by `k_mmap_pin`.
    pub fn set_mmap_pin(f: fn(usize, usize, u32) -> Option<Vec<usize>>) { PIN.store(f as usize, Ordering::Release); }
}
