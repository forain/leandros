//! RM deferred work: os_queue_work_item / os_flush_work_queue.
//!
//! Linux runs each item through `rm_execute_work_item(sp, data)` on a
//! kthread queue: the per-GPU `nv->queue` or the global `nv_kthread_q` when
//! RM passes NULL. Here both are one private `sched::workqueue::WorkQueue`
//! ("nvrm-q", created by the bring-up thread; the system queue until then).
//! `nv->queue` is set to a non-NULL cookie so RM treats the device queue as
//! present.

use alloc::sync::Arc;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU64, Ordering};

use sched::workqueue::WorkQueue;

use crate::{nvlog, NV_ERR_ILLEGAL_ACTION, NV_ERR_NOT_READY, NV_ERR_NO_MEMORY, NV_OK};

static mut QUEUE: Option<Arc<WorkQueue>> = None;
static ITEMS_RUN: AtomicU64 = AtomicU64::new(0);

/// Create the RM work queue (task context, once).
pub(crate) fn init() -> bool {
    unsafe {
        if (*core::ptr::addr_of!(QUEUE)).is_some() {
            return true;
        }
        match WorkQueue::new("nvrm-q") {
            Some(q) => {
                QUEUE = Some(q);
                true
            }
            None => false,
        }
    }
}

fn queue() -> Option<&'static WorkQueue> {
    unsafe {
        match &*core::ptr::addr_of!(QUEUE) {
            Some(q) => Some(&**q),
            None => sched::workqueue::system(),
        }
    }
}

pub(crate) fn items_run() -> u64 {
    ITEMS_RUN.load(Ordering::Relaxed)
}

/// Opaque cookie stored in nv->queue.
pub(crate) fn device_queue_cookie() -> *mut c_void {
    core::ptr::addr_of!(QUEUE) as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn os_queue_work_item(_queue: *mut c_void, data: *mut c_void) -> u32 {
    cb!("os_queue_work_item");
    let q = match queue() {
        Some(q) => q,
        None => return NV_ERR_NOT_READY,
    };
    let d = data as usize;
    if q.queue_fn(move || {
        let n = ITEMS_RUN.fetch_add(1, Ordering::Relaxed);
        if n < 16 {
            nvlog!("[nvrm] +{}ms work item {:#x} running (#{})\n", crate::ms(), d, n + 1);
        }
        crate::rm::rm_execute_work_item(core::ptr::null_mut(), d as *mut c_void);
    }) {
        NV_OK
    } else {
        NV_ERR_NO_MEMORY
    }
}

#[no_mangle]
pub unsafe extern "C" fn os_flush_work_queue(_queue: *mut c_void, _is_unload: u8) -> u32 {
    cb!("os_flush_work_queue");
    if !sched::irqctx::can_sleep() {
        nvlog!("[nvrm] os_flush_work_queue from atomic/IRQ context refused\n");
        return NV_ERR_ILLEGAL_ACTION;
    }
    if let Some(q) = queue() {
        q.flush();
    }
    NV_OK
}

#[no_mangle]
pub unsafe extern "C" fn os_is_queue_flush_ongoing(_queue: *mut c_void) -> u8 {
    cb!("os_is_queue_flush_ongoing");
    0
}

/// UVM is not built; nvidia-haiku logs and ignores this too.
#[no_mangle]
pub unsafe extern "C" fn nv_schedule_uvm_isr(_nv: *mut c_void) {
    cb!("nv_schedule_uvm_isr");
}
