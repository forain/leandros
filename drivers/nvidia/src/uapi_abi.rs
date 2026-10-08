//! The nouveau DRM UAPI as Mesa 25.3.6's NVK uses it (include/drm-uapi/
//! nouveau_drm.h and src/nouveau/drm/nvif/*.h), field for field. Sizes and
//! offsets were taken from those headers with a compiled probe and are pinned
//! by the `const` asserts below.

#![allow(dead_code)]

use core::mem::size_of;

/// `nr` of each driver-private ioctl (DRM_COMMAND_BASE + index).
pub const NR_GETPARAM: u32 = 0x40;
pub const NR_SETPARAM: u32 = 0x41;
pub const NR_CHANNEL_ALLOC: u32 = 0x42;
pub const NR_CHANNEL_FREE: u32 = 0x43;
pub const NR_NVIF: u32 = 0x47;
pub const NR_VM_INIT: u32 = 0x50;
pub const NR_VM_BIND: u32 = 0x51;
pub const NR_EXEC: u32 = 0x52;
pub const NR_GEM_NEW: u32 = 0x80;
pub const NR_GEM_PUSHBUF: u32 = 0x81;
pub const NR_GEM_CPU_PREP: u32 = 0x82;
pub const NR_GEM_CPU_FINI: u32 = 0x83;
pub const NR_GEM_INFO: u32 = 0x84;

pub const GETPARAM_PCI_VENDOR: u64 = 3;
pub const GETPARAM_PCI_DEVICE: u64 = 4;
pub const GETPARAM_BUS_TYPE: u64 = 5;
pub const GETPARAM_FB_SIZE: u64 = 8;
pub const GETPARAM_AGP_SIZE: u64 = 9;
pub const GETPARAM_CHIPSET_ID: u64 = 11;
pub const GETPARAM_VM_VRAM_BASE: u64 = 12;
pub const GETPARAM_GRAPH_UNITS: u64 = 13;
pub const GETPARAM_PTIMER_TIME: u64 = 14;
pub const GETPARAM_HAS_BO_USAGE: u64 = 15;
pub const GETPARAM_HAS_PAGEFLIP: u64 = 16;
pub const GETPARAM_EXEC_PUSH_MAX: u64 = 17;
pub const GETPARAM_VRAM_BAR_SIZE: u64 = 18;
pub const GETPARAM_VRAM_USED: u64 = 19;
pub const GETPARAM_HAS_VMA_TILEMODE: u64 = 20;

pub const GEM_DOMAIN_CPU: u32 = 1 << 0;
pub const GEM_DOMAIN_VRAM: u32 = 1 << 1;
pub const GEM_DOMAIN_GART: u32 = 1 << 2;
pub const GEM_DOMAIN_MAPPABLE: u32 = 1 << 3;
pub const GEM_DOMAIN_COHERENT: u32 = 1 << 4;
pub const GEM_DOMAIN_NO_SHARE: u32 = 1 << 5;

pub const GEM_CPU_PREP_NOWAIT: u32 = 1;
pub const GEM_CPU_PREP_WRITE: u32 = 4;

pub const SYNC_SYNCOBJ: u32 = 0;
pub const SYNC_TIMELINE_SYNCOBJ: u32 = 1;
pub const SYNC_TYPE_MASK: u32 = 0xf;

pub const VM_BIND_OP_MAP: u32 = 0;
pub const VM_BIND_OP_UNMAP: u32 = 1;
pub const VM_BIND_SPARSE: u32 = 1 << 8;
pub const VM_BIND_RUN_ASYNC: u32 = 1;

pub const EXEC_PUSH_NO_PREFETCH: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Getparam { pub param: u64, pub value: u64 }

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ChannelAlloc {
    pub fb_ctxdma_handle: u32,
    pub tt_ctxdma_handle: u32,
    pub channel: i32,
    pub pushbuf_domains: u32,
    pub notifier_handle: u32,
    pub subchan: [[u32; 2]; 8],
    pub nr_subchan: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ChannelFree { pub channel: i32 }

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GemInfo {
    pub handle: u32,
    pub domain: u32,
    pub size: u64,
    pub offset: u64,
    pub map_handle: u64,
    pub tile_mode: u32,
    pub tile_flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GemNew { pub info: GemInfo, pub channel_hint: u32, pub align: u32 }

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GemPushbuf {
    pub channel: u32,
    pub nr_buffers: u32,
    pub buffers: u64,
    pub nr_relocs: u32,
    pub nr_push: u32,
    pub relocs: u64,
    pub push: u64,
    pub suffix0: u32,
    pub suffix1: u32,
    pub vram_available: u64,
    pub gart_available: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GemCpuPrep { pub handle: u32, pub flags: u32 }

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Sync { pub flags: u32, pub handle: u32, pub timeline_value: u64 }

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VmInit { pub kernel_managed_addr: u64, pub kernel_managed_size: u64 }

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VmBindOp {
    pub op: u32,
    pub flags: u32,
    pub handle: u32,
    pub pad: u32,
    pub addr: u64,
    pub bo_offset: u64,
    pub range: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VmBind {
    pub op_count: u32,
    pub flags: u32,
    pub wait_count: u32,
    pub sig_count: u32,
    pub wait_ptr: u64,
    pub sig_ptr: u64,
    pub op_ptr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ExecPush { pub va: u64, pub va_len: u32, pub flags: u32 }

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Exec {
    pub channel: u32,
    pub push_count: u32,
    pub wait_count: u32,
    pub sig_count: u32,
    pub wait_ptr: u64,
    pub sig_ptr: u64,
    pub push_ptr: u64,
}

// ---- NVIF (DRM_NOUVEAU_NVIF) -------------------------------------------

pub const NVIF_IOCTL_V0_SCLASS: u8 = 0x01;
pub const NVIF_IOCTL_V0_NEW: u8 = 0x02;
pub const NVIF_IOCTL_V0_DEL: u8 = 0x03;
pub const NVIF_IOCTL_V0_MTHD: u8 = 0x04;
pub const NV_DEVICE: i32 = 0x0080;
pub const NV_DEVICE_V0_INFO: u8 = 0x00;
pub const NV_DEVICE_INFO_V0_PCIE: u8 = 0x03;
pub const NV_DEVICE_INFO_V0_TURING: u8 = 0x0c;
pub const NV_DEVICE_INFO_V0_AMPERE: u8 = 0x0d;
pub const NV_DEVICE_INFO_V0_ADA: u8 = 0x0e;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct NvifIoctlV0 {
    pub version: u8,
    pub type_: u8,
    pub pad02: [u8; 4],
    pub owner: u8,
    pub route: u8,
    pub token: u64,
    pub object: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct NvifNewV0 {
    pub version: u8,
    pub pad01: [u8; 6],
    pub route: u8,
    pub token: u64,
    pub object: u64,
    pub handle: u32,
    pub oclass: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct NvifSclassV0 { pub version: u8, pub count: u8, pub pad02: [u8; 6] }

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct NvifSclassOclassV0 { pub oclass: i32, pub minver: i16, pub maxver: i16 }

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct NvifMthdV0 { pub version: u8, pub method: u8, pub pad02: [u8; 6] }

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvDeviceInfoV0 {
    pub version: u8,
    pub platform: u8,
    pub chipset: u16,
    pub revision: u8,
    pub family: u8,
    pub pad06: [u8; 2],
    pub ram_size: u64,
    pub ram_user: u64,
    pub chip: [u8; 16],
    pub name: [u8; 64],
}

const _: () = assert!(size_of::<Getparam>() == 16);
const _: () = assert!(size_of::<ChannelAlloc>() == 88);
const _: () = assert!(size_of::<ChannelFree>() == 4);
const _: () = assert!(size_of::<GemInfo>() == 40);
const _: () = assert!(size_of::<GemNew>() == 48);
const _: () = assert!(size_of::<GemPushbuf>() == 64);
const _: () = assert!(size_of::<GemCpuPrep>() == 8);
const _: () = assert!(size_of::<Sync>() == 16);
const _: () = assert!(size_of::<VmInit>() == 16);
const _: () = assert!(size_of::<VmBindOp>() == 40);
const _: () = assert!(size_of::<VmBind>() == 40);
const _: () = assert!(size_of::<ExecPush>() == 16);
const _: () = assert!(size_of::<Exec>() == 40);
const _: () = assert!(size_of::<NvifIoctlV0>() == 24);
const _: () = assert!(size_of::<NvifNewV0>() == 32);
const _: () = assert!(size_of::<NvifSclassV0>() == 8);
const _: () = assert!(size_of::<NvifSclassOclassV0>() == 8);
const _: () = assert!(size_of::<NvifMthdV0>() == 8);
const _: () = assert!(size_of::<NvDeviceInfoV0>() == 104);
const _: () = assert!(core::mem::offset_of!(NvDeviceInfoV0, chip) == 24);
const _: () = assert!(core::mem::offset_of!(NvifNewV0, oclass) == 28);
