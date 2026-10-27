//! V3D SUBMIT_CL front end (openbugs #98 phase C): ioctl argument parsing, the MULTI_SYNC
//! extension, and (under `aarch64 + rpi5`) the kernel glue around `queue::ClQueue`: the queue lock,
//! interrupt/tick servicing (IRQ or polled mode), the overflow chunk pool, BO reference release
//! and the boot self-test (a render-only RCL that clears a 64x64 RGBA8 frame, checked by CRC).
//!
//! The parsing half is pure and host-tested (`drivers/v3d-tests`).
use alloc::vec::Vec;

pub const SUBMIT_CL_FLUSH_CACHE: u32 = 0x01;
pub const SUBMIT_EXTENSION: u32 = 0x02;
pub const EXT_ID_MULTI_SYNC: u32 = 1;
/// `drm_v3d_queue` values used as MULTI_SYNC `wait_stage`.
pub const QUEUE_BIN: u32 = 0;
pub const QUEUE_RENDER: u32 = 1;
pub const QUEUE_CPU: u32 = 5;
pub const MAX_SYNCS: u32 = 256;
pub const MAX_EXTENSIONS: usize = 8;
pub const SUBMIT_CL_SIZE: usize = 72;
pub const MULTI_SYNC_SIZE: usize = 48;
pub const SEM_SIZE: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitError {
    /// EINVAL
    Inval,
    /// EFAULT (unreadable user memory)
    Fault,
}

fn u32_at(b: &[u8], o: usize) -> u32 { u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) }
fn u64_at(b: &[u8], o: usize) -> u64 { u32_at(b, o) as u64 | (u32_at(b, o + 4) as u64) << 32 }

/// `struct drm_v3d_submit_cl` (72 bytes).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClArgs {
    pub bcl_start: u32,
    pub bcl_end: u32,
    pub rcl_start: u32,
    pub rcl_end: u32,
    pub in_sync_bcl: u32,
    pub in_sync_rcl: u32,
    pub out_sync: u32,
    pub qma: u32,
    pub qms: u32,
    pub qts: u32,
    pub bo_handles: u64,
    pub bo_handle_count: u32,
    pub flags: u32,
    pub perfmon_id: u32,
    pub pad: u32,
    pub extensions: u64,
}

impl ClArgs {
    pub fn parse(b: &[u8; SUBMIT_CL_SIZE]) -> Self {
        ClArgs {
            bcl_start: u32_at(b, 0), bcl_end: u32_at(b, 4), rcl_start: u32_at(b, 8), rcl_end: u32_at(b, 12),
            in_sync_bcl: u32_at(b, 16), in_sync_rcl: u32_at(b, 20), out_sync: u32_at(b, 24),
            qma: u32_at(b, 28), qms: u32_at(b, 32), qts: u32_at(b, 36),
            bo_handles: u64_at(b, 40), bo_handle_count: u32_at(b, 48), flags: u32_at(b, 52),
            perfmon_id: u32_at(b, 56), pad: u32_at(b, 60), extensions: u64_at(b, 64),
        }
    }

    /// The checks that do not need any handle lookup.
    pub fn validate(&self) -> Result<(), SubmitError> {
        if self.pad != 0 { return Err(SubmitError::Inval); }
        if self.flags & !(SUBMIT_CL_FLUSH_CACHE | SUBMIT_EXTENSION) != 0 { return Err(SubmitError::Inval); }
        if (self.flags & SUBMIT_EXTENSION != 0) != (self.extensions != 0) { return Err(SubmitError::Inval); }
        if self.bcl_end < self.bcl_start || self.rcl_end < self.rcl_start { return Err(SubmitError::Inval); }
        if self.rcl_start == self.rcl_end { return Err(SubmitError::Inval); }
        Ok(())
    }
}

/// Which syncobjs gate which part of the job, and which ones receive its fence.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncPlan {
    pub bin_in: Vec<u32>,
    pub render_in: Vec<u32>,
    pub out: Vec<u32>,
    pub multisync: bool,
}

/// Walks the extension chain and builds the sync plan. `read(user_addr, buf)` copies user memory
/// (false = fault). Without MULTI_SYNC the legacy `in_sync_bcl/rcl` and `out_sync` fields apply;
/// with it they are ignored (Linux `v3d_submit.c`). In-syncs attach to the stage named by
/// `wait_stage`; out-syncs all receive the (single) job fence. Binary syncobjs only.
pub fn plan_syncs(a: &ClArgs, read: &mut dyn FnMut(u64, &mut [u8]) -> bool) -> Result<SyncPlan, SubmitError> {
    let mut plan = SyncPlan::default();
    let mut next = if a.flags & SUBMIT_EXTENSION != 0 { a.extensions } else { 0 };
    let mut n = 0;
    while next != 0 {
        n += 1;
        if n > MAX_EXTENSIONS { return Err(SubmitError::Inval); }
        let mut hdr = [0u8; 16];
        if !read(next, &mut hdr) { return Err(SubmitError::Fault); }
        let id = u32_at(&hdr, 8);
        let flags = u32_at(&hdr, 12);
        if id != EXT_ID_MULTI_SYNC || flags != 0 || plan.multisync { return Err(SubmitError::Inval); }
        let mut ms = [0u8; MULTI_SYNC_SIZE];
        if !read(next, &mut ms) { return Err(SubmitError::Fault); }
        let (in_ptr, out_ptr) = (u64_at(&ms, 16), u64_at(&ms, 24));
        let (in_n, out_n, stage, pad) = (u32_at(&ms, 32), u32_at(&ms, 36), u32_at(&ms, 40), u32_at(&ms, 44));
        if pad != 0 || in_n > MAX_SYNCS || out_n > MAX_SYNCS || stage > QUEUE_CPU { return Err(SubmitError::Inval); }
        let ins = read_sems(in_ptr, in_n, read)?;
        plan.out = read_sems(out_ptr, out_n, read)?;
        match stage {
            QUEUE_BIN => plan.bin_in = ins,
            QUEUE_RENDER => plan.render_in = ins,
            // A stage this submission has no job for: Linux attaches nothing; gate the whole
            // job instead, which can only be stricter.
            _ => plan.bin_in = ins,
        }
        plan.multisync = true;
        next = u64_at(&hdr, 0);
    }
    if !plan.multisync {
        if a.in_sync_bcl != 0 { plan.bin_in.push(a.in_sync_bcl); }
        if a.in_sync_rcl != 0 { plan.render_in.push(a.in_sync_rcl); }
        if a.out_sync != 0 { plan.out.push(a.out_sync); }
    }
    Ok(plan)
}

fn read_sems(ptr: u64, count: u32, read: &mut dyn FnMut(u64, &mut [u8]) -> bool) -> Result<Vec<u32>, SubmitError> {
    let mut v = Vec::with_capacity(count as usize);
    for i in 0..count as u64 {
        let mut s = [0u8; SEM_SIZE];
        if !read(ptr + i * SEM_SIZE as u64, &mut s) { return Err(SubmitError::Fault); }
        let (handle, flags, point) = (u32_at(&s, 0), u32_at(&s, 4), u64_at(&s, 8));
        // No timeline syncobjs on this path; mbz must be zero.
        if handle == 0 || flags != 0 || point != 0 || u64_at(&s, 16) != 0 || u64_at(&s, 24) != 0 {
            return Err(SubmitError::Inval);
        }
        v.push(handle);
    }
    Ok(v)
}

/// CRC-32 (IEEE, reflected), for the self-test image.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 { c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 }; }
    }
    !c
}

/// The image the self-test RCL must produce: every pixel = `CLEAR_COLOR` (little endian).
pub fn selftest_expected_crc() -> u32 {
    use super::selftest_cl::*;
    let mut c = !0u32;
    let px = CLEAR_COLOR.to_le_bytes();
    for _ in 0..(W * H) {
        for &b in px.iter() {
            c ^= b as u32;
            for _ in 0..8 { c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 }; }
        }
    }
    !c
}

/// Builds the self-test BO contents for a BO at GPU address `va` into `bo` (BO_SIZE bytes, zeroed).
/// Returns the (rcl_start, rcl_end) pair.
pub fn selftest_fill(bo: &mut [u8], va: u32) -> (u32, u32) {
    use super::selftest_cl::*;
    bo[RCL_OFF..RCL_OFF + RCL.len()].copy_from_slice(&RCL);
    bo[GTL_OFF..GTL_OFF + GTL.len()].copy_from_slice(&GTL);
    for &p in PATCHES.iter() {
        let w = u32_at(bo, p).wrapping_add(va);
        bo[p..p + 4].copy_from_slice(&w.to_le_bytes());
    }
    (va + RCL_OFF as u32, va + (RCL_OFF + RCL.len()) as u32)
}

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub use hw::*;

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
mod hw {
    use super::super::queue::{ClJob, ClQueue, QueueEnv, Stats, OVERFLOW_BYTES};
    use super::super::regs::V3dRegs;
    use super::super::{irq, mmu, MmioRegs};
    use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
    use spin::Mutex;

    static QUEUE: Mutex<ClQueue> = Mutex::new(ClQueue::new());
    static PENDING: AtomicU32 = AtomicU32::new(0);
    static ACTIVE: AtomicBool = AtomicBool::new(false);
    static REGS: AtomicPtr<MmioRegs> = AtomicPtr::new(core::ptr::null_mut());
    static VER: AtomicU32 = AtomicU32::new(0);
    static CHUNKS: AtomicU32 = AtomicU32::new(0);
    /// Chunks mapped at bring-up; more are added (task context) while the pool runs low.
    const POOL_START: u32 = 4;
    const POOL_MAX: u32 = 32;
    const CHUNK_ORDER: usize = 6; // 256 KiB

    struct HwEnv;
    impl QueueEnv for HwEnv {
        fn now_ns(&self) -> u64 { sched::monotonic_ns() }
        fn fence_done(&self, f: u64) -> bool { crate::drm_device_interface::v3d_fence_signalled(f) }
        fn signal(&self, f: u64) { crate::drm_device_interface::syncobj_retire_fence(f) }
        fn reinit(&self, regs: &dyn V3dRegs) -> bool {
            let ok = mmu::reenable(regs);
            irq::enable(regs, VER.load(Ordering::Relaxed));
            ok
        }
    }

    fn regs() -> Option<&'static MmioRegs> {
        let p = REGS.load(Ordering::Acquire);
        if p.is_null() { None } else { Some(unsafe { &*p }) }
    }

    /// The real V3D CL queue is up (probe + MMU succeeded): ioctls take the hardware path.
    pub fn active() -> bool { ACTIVE.load(Ordering::Acquire) }

    pub fn stats() -> Stats { QUEUE.lock().stats }

    /// Advances the queue with whatever interrupt bits are pending. Callable from any context:
    /// it only try-locks, and whoever holds the lock re-checks `PENDING` before leaving.
    pub fn service() {
        let r = match regs() { Some(r) => r, None => return };
        loop {
            let mut q = match QUEUE.try_lock() { Some(q) => q, None => return };
            let bits = PENDING.swap(0, Ordering::AcqRel);
            q.service(r, &HwEnv, bits);
            drop(q);
            if PENDING.load(Ordering::Acquire) == 0 { return; }
        }
    }

    fn core_hook(bits: u32) {
        PENDING.fetch_or(bits, Ordering::AcqRel);
        service();
    }

    /// Timer tick: the polled interrupt fallback, the hang timer and dependency re-checks.
    pub fn tick() {
        if !active() { return; }
        if irq::polled() { irq::poll(); }
        let r = match regs() { Some(r) => r, None => return };
        if let Some(mut q) = QUEUE.try_lock() {
            if !q.idle() { q.tick(r, &HwEnv); }
        }
        service();
    }

    fn add_chunk() -> bool {
        if CHUNKS.load(Ordering::Relaxed) >= POOL_MAX { return false; }
        let pa = match mm::buddy::alloc(CHUNK_ORDER) { Some(p) => p, None => return false };
        let va = match crate::drm_device_interface::v3d_kernel_va_alloc(OVERFLOW_BYTES as u64) {
            Some(v) => v,
            None => { mm::buddy::free(pa, CHUNK_ORDER); return false; }
        };
        if mmu::map_bo(va, pa as u64, OVERFLOW_BYTES as u64).is_err() {
            crate::drm_device_interface::v3d_kernel_va_free(va);
            mm::buddy::free(pa, CHUNK_ORDER);
            return false;
        }
        // Chunks are permanent: they move between the pool and jobs, never back to the allocator.
        if QUEUE.lock().add_chunk(va as u32) { CHUNKS.fetch_add(1, Ordering::Relaxed); }
        service();
        true
    }

    /// Task context: queues a job; `alloc_fence` runs under the queue lock so fence order equals
    /// queue order (the global watermark retirement depends on it).
    /// A full queue hands the job back (its BO references are still the caller's).
    pub fn submit(mut job: ClJob, alloc_fence: &mut dyn FnMut() -> u64) -> Result<u64, ClJob> {
        if QUEUE.lock().free_chunks() < 2 { let _ = add_chunk(); }
        let fence = {
            let mut q = QUEUE.lock();
            if !q.has_space() { return Err(job); }
            job.fence = alloc_fence();
            let f = job.fence;
            let _ = q.push(job); // cannot fail: space checked under the same lock
            f
        };
        service();
        Ok(fence)
    }

    /// Task context: drops completed jobs, handing each BO reference to `release`.
    pub fn reap(release: &mut dyn FnMut(u32)) {
        loop {
            let job = match QUEUE.lock().take_retired() { Some(j) => j, None => return };
            for &o in job.bos.iter() { release(o); }
        }
    }

    /// GET_PARAM answers from the probed hardware; `None` for params the caller keeps answering.
    pub fn param(p: u32) -> Option<u64> {
        let rep = super::super::REPORT.lock();
        let rep = rep.as_ref()?;
        Some(match p {
            0 => regs().map(|r| r.hub_r(super::super::regs::HUB_UIFCFG)).unwrap_or(0) as u64,
            1 => rep.hub_ident1 as u64,
            2 => rep.hub_ident2 as u64,
            3 => rep.hub_ident3 as u64,
            4 => rep.core_ident0 as u64,
            5 => rep.core_ident1 as u64,
            6 => rep.core_ident2 as u64,
            // TFU and CSD are phase D (pv3d-tfu-csd): not on the real device yet.
            7 | 8 => 0,
            9 => 1,  // CACHE_FLUSH
            10 => 0, // PERFMON
            11 => 1, // MULTISYNC_EXT
            12 => 0, // CPU_QUEUE
            13 => 0, // MAX_PERF_COUNTERS
            14 => 0, // SUPER_PAGES
            15 => QUEUE.lock().stats.resets,
            16 => 0,
            _ => return None,
        })
    }

    /// After the MMU came up (phase B): bring the queue up, then the boot self-test.
    pub fn after_mmu(r: &'static MmioRegs, ver: u32, cmdline: &str) {
        REGS.store(r as *const MmioRegs as *mut MmioRegs, Ordering::Release);
        VER.store(ver, Ordering::Relaxed);
        QUEUE.lock().reserve();
        irq::set_core_hook(core_hook);
        ACTIVE.store(true, Ordering::Release);
        for _ in 0..POOL_START { if !add_chunk() { break; } }
        crate::pci::serial_debug(&alloc::format!(
            "[v3d] cl queue up pool={}x256K mode={}\n",
            CHUNKS.load(Ordering::Relaxed), if irq::polled() { "poll" } else { "irq" }));
        if cmdline.split_ascii_whitespace().any(|t| t == "v3d.selftest=0") {
            crate::pci::serial_debug("[v3d] selftest skipped (v3d.selftest=0)\n");
            return;
        }
        selftest(r);
    }

    fn selftest(r: &'static MmioRegs) {
        use super::super::selftest_cl::*;
        let t0 = sched::monotonic_ns();
        let order = 3; // 32 KiB >= BO_SIZE
        let fail = |why: &str| crate::pci::serial_debug(&alloc::format!("[v3d] selftest clear fail {}\n", why));
        let pa = match mm::buddy::alloc(order) { Some(p) => p, None => return fail("nomem") };
        let k = mm::phys_to_virt(pa) as *mut u8;
        let va = match crate::drm_device_interface::v3d_kernel_va_alloc(BO_SIZE as u64) {
            Some(v) => v,
            None => { mm::buddy::free(pa, order); return fail("nova"); }
        };
        let bo = unsafe { core::slice::from_raw_parts_mut(k, BO_SIZE) };
        bo.fill(0);
        let (rs, re) = super::selftest_fill(bo, va as u32);
        // map_bo cleans + invalidates the cacheable alias, so the GPU sees the CL bytes.
        if mmu::map_bo(va, pa as u64, BO_SIZE as u64).is_err() {
            crate::drm_device_interface::v3d_kernel_va_free(va);
            mm::buddy::free(pa, order);
            return fail("map");
        }
        let before = QUEUE.lock().stats;
        let job = ClJob { rcl_start: rs, rcl_end: re, ..Default::default() };
        let fence = match submit(job, &mut || crate::drm_device_interface::v3d_alloc_fence_id()) {
            Ok(f) => f,
            Err(_) => return fail("queue-full"),
        };
        let deadline = t0 + 200_000_000;
        let mut done = false;
        while sched::monotonic_ns() < deadline {
            irq::poll(); // harmless in IRQ mode; the only driver when IRQs are off this early
            service();
            if crate::drm_device_interface::v3d_fence_signalled(fence) { done = true; break; }
            core::hint::spin_loop();
        }
        if !done {
            // Kill it now rather than wait for the 500 ms hang timer.
            QUEUE.lock().reset(r, &HwEnv);
        }
        let after = QUEUE.lock().stats;
        reap(&mut |_| {});
        let frdone = after.render_done - before.render_done;
        let t = (sched::monotonic_ns() - t0) / 1000;
        if !done || after.resets != before.resets {
            // The GPU may still hold the BO: leak it.
            return crate::pci::serial_debug(&alloc::format!(
                "[v3d] selftest clear fail timeout frdone={} ct1ca=0x{:08x} ct1cs=0x{:08x} int=0x{:08x} t={}us\n",
                frdone, r.core_r(super::super::regs::CLE_CT1CA), r.core_r(super::super::regs::CLE_CT1CS),
                r.core_r(super::super::regs::CTL_INT_STS), t));
        }
        mmu::dcache_clean_inval(k as usize + IMG_OFF, (STRIDE * H) as usize);
        let img = &bo[IMG_OFF..IMG_OFF + (STRIDE * H) as usize];
        let crc = super::crc32(img);
        let want = super::selftest_expected_crc();
        let bad = img.chunks_exact(4).filter(|p| u32::from_le_bytes([p[0], p[1], p[2], p[3]]) != CLEAR_COLOR).count();
        let first = u32::from_le_bytes([img[0], img[1], img[2], img[3]]);
        mmu::unmap_bo(va, BO_SIZE as u64);
        crate::drm_device_interface::v3d_kernel_va_free(va);
        mm::buddy::free(pa, order);
        if crc == want {
            crate::pci::serial_debug(&alloc::format!("[v3d] selftest clear crc=0x{:08x} ok frdone={} t={}us\n", crc, frdone, t));
        } else {
            crate::pci::serial_debug(&alloc::format!(
                "[v3d] selftest clear fail mismatch crc=0x{:08x} want=0x{:08x} bad={}/4096 px0=0x{:08x} frdone={} t={}us\n",
                crc, want, bad, first, frdone, t));
        }
    }
}
