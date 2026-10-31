//! kapi exports: DRM class (W2-7a). A GPU module registers a `drivers::drm_class::KDrmOps` table and
//! reaches the kernel's fake-offset window, fence wait and syncobj tables through these.
use super::*;

fn errno_of(e: drivers::DriverError) -> i32 {
    use drivers::DriverError::*;
    match e { NotFound => -2, Io => -5, Unsupported => -95, InvalidParameter => -22, Access | NotMaster => -13, Busy => -16, Errno(n) => -n }
}

/// Register the DRM uapi table. 0 ok, -22 bad table, -17 a backend is already installed.
#[no_mangle]
pub extern "C" fn kapi_drm_register(owner: KmodId, ops: *const drivers::drm_class::KDrmOps, ctx: usize) -> i32 {
    let r = drivers::drm_class::register(ops, ctx);
    if r == 0 { crate::kmod::registry::note_registration(owner, RegKind::DRM, ctx as u64); }
    r
}

/// A fake mmap offset for the object whose real token is `real`; 0 = none.
#[no_mangle]
pub extern "C" fn kapi_drm_offset_alloc(real: u64, size: u64) -> u64 { drivers::drm_offset::offset_for(real, size).unwrap_or(0) }

#[no_mangle]
pub extern "C" fn kapi_drm_offset_release(real: u64) { drivers::drm_offset::release(real) }

/// Wait until every fence id is signalled or `deadline_ns` (monotonic) passes. 0 ok, -errno.
#[no_mangle]
pub extern "C" fn kapi_fence_wait(ids: *const u64, n: usize, deadline_ns: u64) -> i32 {
    let s: &[u64] = if n == 0 || ids.is_null() { &[] } else { unsafe { core::slice::from_raw_parts(ids, n) } };
    match drivers::drm_device_interface::wait_fences(s, deadline_ns) { Ok(()) => 0, Err(e) => errno_of(e) }
}

#[no_mangle]
pub extern "C" fn kapi_syncobj_install(handle: u32, open_id: u32, point: u64, fence: u64) -> i32 {
    match drivers::drm_device_interface::syncobj_install(handle, open_id, point, fence) { Ok(()) => 0, Err(e) => errno_of(e) }
}

#[no_mangle]
pub extern "C" fn kapi_syncobj_exists(handle: u32, open_id: u32) -> bool { drivers::drm_device_interface::syncobj_exists(handle, open_id) }

/// Fence ids behind `handle`/`point` copied into `out` (up to `cap`). Returns the total count, or -errno.
#[no_mangle]
pub extern "C" fn kapi_syncobj_in_fences(handle: u32, open_id: u32, point: u64, out: *mut u64, cap: usize) -> i64 {
    match drivers::drm_device_interface::syncobj_in_fences(handle, open_id, point) {
        Ok(v) => {
            if !out.is_null() { for (i, f) in v.iter().take(cap).enumerate() { unsafe { *out.add(i) = *f; } } }
            v.len() as i64
        }
        Err(e) => errno_of(e) as i64,
    }
}

/// A GPU interrupt signalled a fence: wake syncobj waiters.
#[no_mangle]
pub extern "C" fn kapi_fence_irq_hint() { drivers::drm_device_interface::gpu_fence_irq_hint() }

/// A page flip / vblank event for `cookie` completed.
#[no_mangle]
pub extern "C" fn kapi_kms_event_complete(cookie: u32, seq: u32, ts_ns: u64) { drivers::drm_device_interface::ext_event_complete(cookie, seq, ts_ns) }

/// Register one KMS table (`drivers::kms_class`): kind 0 backend, 1 flip, 2 cursor, 3 outputs, 4 pipe2, 5 pipe-B cursor.
/// 0 ok, -22 bad kind/table, -17 already registered. Permanent: no unregister exists.
#[no_mangle]
pub extern "C" fn kapi_kms_register(owner: KmodId, kind: u32, ops: *const u8, ctx: usize) -> i32 {
    let r = drivers::kms_class::register_raw(kind, ops, ctx);
    if r == 0 { crate::kmod::registry::note_registration(owner, RegKind::KMS, ((kind as u64) << 32) | (ctx as u64 & 0xFFFF_FFFF)); }
    r
}

/// Connector 1 reports eDP (the backend scans out an internal panel).
#[no_mangle]
pub extern "C" fn kapi_kms_set_internal_panel(v: bool) { drivers::gpu_kms::set_internal_panel(v) }

/// HPD uevent for DRM card `minor`.
#[no_mangle]
pub extern "C" fn kapi_drm_hotplug(minor: u8) { evdev_server::hp_drm_change(minor) }

/// `[presents, present_errors, vblank_time_ns?]` as `drm_device_interface::ext_present_stats` returns them.
#[no_mangle]
pub extern "C" fn kapi_ext_present_stats(out: *mut u64) {
    if out.is_null() { return; }
    let (a, b, c) = drivers::drm_device_interface::ext_present_stats();
    unsafe { *out = a; *out.add(1) = b; *out.add(2) = c; }
}

/// The VFS tmpfs-pool census registered by servers/drm: 9 u64 into `out`. False = none.
#[no_mangle]
pub extern "C" fn kapi_tmpfs_census(out: *mut u64) -> bool {
    match (out.is_null(), drivers::drm_class::tmpfs_census()) {
        (false, Some(v)) => { for (i, x) in v.iter().enumerate() { unsafe { *out.add(i) = *x; } } true }
        _ => false,
    }
}
