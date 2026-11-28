//! The `KV3dOps` implementation: submit shims and the table core DRM calls through.
//!
//! One source for `V3D=y` (compiled into the kernel) and `V3D=m` (`v3d.kmod`). Everything it
//! needs from the rest of the kernel comes through `crate::kenv::v3d`; it never names core DRM.
//! `submit.arg` is a kernel pointer to a copy of the fixed ioctl struct (core copied it in).
#![cfg(all(target_arch = "aarch64", feature = "rpi5"))]

use alloc::vec::Vec;
use crate::kenv::v3d as kenv;
use crate::kenv::v3d::DriverError;
use crate::v3d::submit::{self, plan_syncs, plan_syncs_single, ClArgs, CsdArgs, SubmitError, TfuArgs,
                         SUBMIT_CL_FLUSH_CACHE, SUBMIT_CSD_SIZE, SUBMIT_TFU_SIZE};
use crate::v3d::{mmu, queue};
use crate::v3d_class::KV3dOps;

// Linux UAPI ioctl words (duplicates of the core constants; both are compile-time literals).
const IOCTL_SUBMIT_CL: u32 = 0xC0486440;
const IOCTL_SUBMIT_TFU: u32 = 0x40586446;
const IOCTL_SUBMIT_CSD: u32 = 0x40586447;

/// Upper bound on a user handle array (matches the core syncobj limit).
const SYNCOBJ_MAX_HANDLES: u32 = 4096;

fn code(r: Result<usize, DriverError>) -> i64 {
    match r {
        Ok(v) => v as i64,
        Err(DriverError::InvalidParameter) => -22,
        Err(DriverError::NotFound) => -2,
        Err(DriverError::Busy) => -16,
        Err(DriverError::Errno(e)) => -(e as i64),
        Err(_) => -5,
    }
}

fn map_submit_err(e: SubmitError) -> DriverError {
    match e {
        SubmitError::Inval => DriverError::InvalidParameter,
        SubmitError::Fault => DriverError::Errno(14),
    }
}

fn copy_in(p: u64, buf: &mut [u8]) -> bool { kenv::copy_from_user(p as usize, buf) }

/// Nested user handle array (range-checked copy).
fn read_handles(ptr: u64, count: u32) -> Result<Vec<u32>, DriverError> {
    if ptr == 0 || count == 0 || count > SYNCOBJ_MAX_HANDLES { return Err(DriverError::InvalidParameter); }
    let mut v = alloc::vec![0u32; count as usize];
    let bytes = unsafe { core::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, count as usize * 4) };
    if !copy_in(ptr, bytes) { return Err(DriverError::Errno(14)); }
    Ok(v)
}

/// In-sync handles -> pending fence ids (empty / no fence yet = satisfied).
fn deps(hs: &[u32], open_id: u32) -> Result<Vec<u64>, DriverError> {
    let mut v = Vec::with_capacity(hs.len());
    for &h in hs { kenv::syncobj_deps(h, open_id, &mut v)?; }
    Ok(v)
}

fn hold_all(handles: &[u32], open_id: u32) -> Result<Vec<u32>, DriverError> {
    let mut held = Vec::with_capacity(handles.len());
    for &h in handles {
        match kenv::bo_hold(h, open_id) {
            o if o > 0 => held.push(o as u32),
            0 => {}
            _ => {
                for &o in held.iter() { kenv::bo_unref(o); }
                return Err(DriverError::NotFound);
            }
        }
    }
    Ok(held)
}

fn reap() { submit::reap(&mut |obj| kenv::bo_unref(obj)); }

/// Queue the job (waiting up to 2 s for room), then attach the fence everywhere.
fn enqueue(mut job: queue::ClJob, out: &[u32], handles: &[u32], open_id: u32) -> Result<usize, DriverError> {
    let t0 = kenv::sched::monotonic_ns();
    let fence = loop {
        match submit::submit(job, &mut || kenv::alloc_fence_id()) {
            Ok(f) => break f,
            Err(j) => job = j,
        }
        reap();
        let now = kenv::sched::monotonic_ns();
        if now - t0 > 2_000_000_000 || kenv::queue_wait(now + 1_000_000) {
            for &o in job.bos.iter() { kenv::bo_unref(o); }
            return Err(DriverError::Busy);
        }
    };
    for &h in out { kenv::syncobj_install(h, open_id, fence); }
    for &h in handles { kenv::bo_fence(h, open_id, fence); }
    Ok(0)
}

fn submit_cl(arg: usize, open_id: u32) -> Result<usize, DriverError> {
    reap();
    if arg == 0 { return Err(DriverError::InvalidParameter); }
    let raw = unsafe { core::ptr::read_unaligned(arg as *const [u8; submit::SUBMIT_CL_SIZE]) };
    let a = ClArgs::parse(&raw);
    a.validate().map_err(map_submit_err)?;
    if a.perfmon_id != 0 { return Err(DriverError::NotFound); }
    let plan = plan_syncs(&a, &mut |p, buf| copy_in(p, buf)).map_err(map_submit_err)?;
    let bin_deps = deps(&plan.bin_in, open_id)?;
    let render_deps = deps(&plan.render_in, open_id)?;
    for &h in plan.out.iter() {
        if !kenv::syncobj_exists(h, open_id) { return Err(DriverError::NotFound); }
    }
    let handles = if a.bo_handle_count == 0 { Vec::new() } else { read_handles(a.bo_handles, a.bo_handle_count)? };
    let held = hold_all(&handles, open_id)?;
    let job = queue::ClJob {
        bcl_start: a.bcl_start, bcl_end: a.bcl_end, rcl_start: a.rcl_start, rcl_end: a.rcl_end,
        qma: a.qma, qms: a.qms, qts: a.qts, bin_deps, render_deps,
        flush_cache: a.flags & SUBMIT_CL_FLUSH_CACHE != 0, bos: held, ..Default::default()
    };
    enqueue(job, &plan.out, &handles, open_id)
}

/// Common tail of SUBMIT_TFU / SUBMIT_CSD.
fn enqueue_single(mut job: queue::ClJob, plan: &submit::SyncPlan, handles: &[u32], open_id: u32) -> Result<usize, DriverError> {
    job.bin_deps = deps(&plan.bin_in, open_id)?;
    for &h in plan.out.iter() {
        if !kenv::syncobj_exists(h, open_id) { return Err(DriverError::NotFound); }
    }
    job.bos = hold_all(handles, open_id)?;
    enqueue(job, &plan.out, handles, open_id)
}

fn submit_tfu(arg: usize, open_id: u32) -> Result<usize, DriverError> {
    reap();
    if arg == 0 { return Err(DriverError::InvalidParameter); }
    let raw = unsafe { core::ptr::read_unaligned(arg as *const [u8; SUBMIT_TFU_SIZE]) };
    let a = TfuArgs::parse(&raw);
    a.validate().map_err(map_submit_err)?;
    let plan = plan_syncs_single(a.flags, a.extensions, a.in_sync, a.out_sync, &mut |p, buf| copy_in(p, buf))
        .map_err(map_submit_err)?;
    let job = queue::ClJob { kind: queue::JobKind::Tfu(a.regs), ..Default::default() };
    enqueue_single(job, &plan, &a.handles(), open_id)
}

fn submit_csd(arg: usize, open_id: u32) -> Result<usize, DriverError> {
    reap();
    if arg == 0 { return Err(DriverError::InvalidParameter); }
    let raw = unsafe { core::ptr::read_unaligned(arg as *const [u8; SUBMIT_CSD_SIZE]) };
    let a = CsdArgs::parse(&raw);
    a.validate().map_err(map_submit_err)?;
    if a.perfmon_id != 0 { return Err(DriverError::NotFound); }
    let plan = plan_syncs_single(a.flags, a.extensions, a.in_sync, a.out_sync, &mut |p, buf| copy_in(p, buf))
        .map_err(map_submit_err)?;
    let handles = if a.bo_handle_count == 0 { Vec::new() } else { read_handles(a.bo_handles, a.bo_handle_count)? };
    let job = queue::ClJob { kind: queue::JobKind::Csd(a.cfg), ..Default::default() };
    enqueue_single(job, &plan, &handles, open_id)
}

extern "C" fn active() -> bool { submit::active() }
extern "C" fn map_bo(va: u64, phys: u64, bytes: u64) -> i32 {
    if !mmu::active() { return 1; }
    if mmu::map_bo(va, phys, bytes).is_err() { -5 } else { 0 }
}
extern "C" fn unmap_bo(va: u64, bytes: u64) { mmu::unmap_bo(va, bytes) }
extern "C" fn param(p: u32, out: *mut u64) -> bool {
    match submit::param(p) { Some(v) => { unsafe { out.write_unaligned(v) }; true } None => false }
}
extern "C" fn submit_(nr: u32, arg: usize, open_id: u32) -> i64 {
    code(match nr {
        IOCTL_SUBMIT_CL => submit_cl(arg, open_id),
        IOCTL_SUBMIT_TFU => submit_tfu(arg, open_id),
        IOCTL_SUBMIT_CSD => submit_csd(arg, open_id),
        _ => Err(DriverError::Unsupported),
    })
}
extern "C" fn reap_() { reap() }
extern "C" fn tick() { submit::tick() }
extern "C" fn wait_bo(fence: u64, timeout_ns: u64) -> bool {
    let deadline = kenv::sched::monotonic_ns().saturating_add(timeout_ns);
    loop {
        reap();
        if kenv::fence_signalled(fence) { return true; }
        let now = kenv::sched::monotonic_ns();
        if now >= deadline { return false; }
        // A timed park, not a yield loop: the fence signals from the IRQ/reap path, and a
        // 250 us granule keeps the added latency negligible without spinning a CPU.
        crate::kenv::sleep_until(core::cmp::min(now + 250_000, deadline));
    }
}

pub static TABLE: KV3dOps = KV3dOps {
    size: core::mem::size_of::<KV3dOps>(),
    active, map_bo, unmap_bo, param, submit: submit_, reap: reap_, tick, wait_bo,
};
