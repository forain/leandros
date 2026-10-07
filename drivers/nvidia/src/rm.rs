//! Typed RM entry points (src/nvidia/arch/nvalloc/unix/include/nv.h). The
//! first argument of every rm_* call is Linux's `nvidia_stack_t *` alt-stack
//! cookie; it is unused on x86_64 and NULL here, as in nvidia-haiku.

use core::ffi::{c_char, c_void};

use crate::glue::NvState;

pub type NvidiaStack = c_void;

extern "C" {
    pub fn rm_init_rm(sp: *mut NvidiaStack) -> bool;
    pub fn rm_shutdown_rm(sp: *mut NvidiaStack);
    pub fn rm_is_supported_device(sp: *mut NvidiaStack, nv: *mut NvState) -> u32;
    pub fn rm_init_private_state(sp: *mut NvidiaStack, nv: *mut NvState) -> bool;
    pub fn rm_free_private_state(sp: *mut NvidiaStack, nv: *mut NvState);
    pub fn rm_set_rm_firmware_requested(sp: *mut NvidiaStack, nv: *mut NvState);
    pub fn rm_init_event_locks(sp: *mut NvidiaStack, nv: *mut NvState) -> bool;
    pub fn rm_destroy_event_locks(sp: *mut NvidiaStack, nv: *mut NvState);
    pub fn rm_init_adapter(sp: *mut NvidiaStack, nv: *mut NvState) -> bool;
    pub fn rm_disable_adapter(sp: *mut NvidiaStack, nv: *mut NvState);
    pub fn rm_shutdown_adapter(sp: *mut NvidiaStack, nv: *mut NvState);
    pub fn rm_isr(sp: *mut NvidiaStack, nv: *mut NvState, need_bottom_half: *mut u32) -> bool;
    pub fn rm_isr_bh(sp: *mut NvidiaStack, nv: *mut NvState);
    pub fn rm_execute_work_item(sp: *mut NvidiaStack, data: *mut c_void);
    pub fn rm_run_nano_timer_callback(sp: *mut NvidiaStack, nv: *mut NvState, event: *mut c_void) -> u32;
    pub fn rm_read_registry_dword(sp: *mut NvidiaStack, nv: *mut NvState, name: *const c_char, data: *mut u32) -> u32;
    pub fn rm_write_registry_dword(sp: *mut NvidiaStack, nv: *mut NvState, name: *const c_char, data: u32) -> u32;
    pub fn rm_get_device_name(device: u16, subsystem_vendor: u16, subsystem_device: u16) -> *const c_char;
}
