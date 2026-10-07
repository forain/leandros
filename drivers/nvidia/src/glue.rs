//! Rust view of ports/openrm/glue/lnv_glue.h. nv_state_t stays opaque: the
//! C glue (compiled against NVIDIA's headers) owns its layout.

use core::ffi::c_void;

/// Opaque `nv_state_t`.
#[repr(C)]
pub struct NvState {
    _opaque: [u8; 0],
}

/// nv_state_t.bars[] slots (nv.h NV_GPU_BAR_INDEX_*).
pub const NV_GPU_BAR_INDEX_REGS: u32 = 0;
pub const NV_GPU_BAR_INDEX_FB: u32 = 1;
pub const NV_GPU_BAR_INDEX_IMEM: u32 = 2;

extern "C" {
    pub fn lnv_state_size() -> u64;
    #[allow(clippy::too_many_arguments)]
    pub fn lnv_state_init(
        nv: *mut NvState,
        os_state: *mut c_void,
        pci_handle: *mut c_void,
        domain: u32,
        bus: u8,
        slot: u8,
        function: u8,
        vendor_id: u16,
        device_id: u16,
        subsystem_vendor: u16,
        subsystem_id: u16,
        interrupt_line: u32,
    );
    pub fn lnv_state_set_bar(nv: *mut NvState, nv_bar_index: u32, pci_bar: u32, cpu_address: u64, size: u64) -> u8;
    pub fn lnv_state_os_state(nv: *mut NvState) -> *mut c_void;
    pub fn lnv_state_pci_handle(nv: *mut NvState) -> *mut c_void;
}
