//! `KV3dOps`: the repr(C) table shared by core (`v3d_class`) and the `v3d` module, which `#[path]`-includes this file.

#[repr(C)]
pub struct KV3dOps {
    /// `size_of::<KV3dOps>()` as the registrant built it; core refuses a smaller table.
    pub size: usize,
    /// Real hardware queue is up (probe + MMU ok).
    pub active: extern "C" fn() -> bool,
    /// Back a GPU VA with PTEs.
    pub map_bo: extern "C" fn(va: u64, phys: u64, bytes: u64) -> i32,
    /// Drop the PTEs before the VA span and pages are reused.
    pub unmap_bo: extern "C" fn(va: u64, bytes: u64),
    /// `DRM_V3D_PARAM`: true and `*out` set when the driver answers.
    pub param: extern "C" fn(param: u32, out: *mut u64) -> bool,
    /// SUBMIT_CL / SUBMIT_TFU / SUBMIT_CSD; `nr` is the ioctl command word.
    pub submit: extern "C" fn(nr: u32, arg: usize, open_id: u32) -> i64,
    /// Release BO references of jobs whose fences signalled.
    pub reap: extern "C" fn(),
    /// Tick-context poll: IRQ fallback, hang timer, dependency re-checks.
    pub tick: extern "C" fn(),
    /// Task-context bounded wait for `fence`; false on timeout.
    pub wait_bo: extern "C" fn(fence: u64, timeout_ns: u64) -> bool,
}
