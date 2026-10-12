//! The i915 DRM UAPI subset Mesa's iris (and ANV) use on a Gen9 GPU: ioctl
//! numbers, argument structures, and the pure halves of the answers —
//! GETPARAM values, DRM_IOCTL_I915_QUERY blobs and the query protocol, and
//! the validation of EXECBUFFER2 / exec objects / context parameters.
//!
//! Layouts are pinned by const asserts whose sizes and offsets come from a
//! compiled probe of Mesa 25.3.6's include/drm-uapi/i915_drm.h (the header
//! iris is built against). No kernel state here: `drivers/i915-tests` runs
//! all of it on the host.
//!
//! This is the G3 foundation; the stateful backend (GEM objects, VMs,
//! contexts, submission) that answers through `gpu_uapi` builds on it.

#![allow(dead_code)]

use alloc::vec::Vec;

use super::hw::{Sseu, topology_blob};

// ── errno values the UAPI returns (Linux) ────────────────────────────────────

pub const ENOENT: i32 = 2;
pub const EINTR: i32 = 4;
pub const EIO: i32 = 5;
pub const ENOMEM: i32 = 12;
pub const EFAULT: i32 = 14;
pub const EBUSY: i32 = 16;
pub const ENODEV: i32 = 19;
pub const EINVAL: i32 = 22;
pub const ENOSPC: i32 = 28;
pub const ENOSYS: i32 = 38;
pub const ETIME: i32 = 62;

// ── ioctl numbers (nr = low byte; DRM_COMMAND_BASE = 0x40) ──────────────────

pub const NR_GETPARAM: u32 = 0x46;
pub const NR_SETPARAM: u32 = 0x47;
pub const NR_GEM_EXECBUFFER2: u32 = 0x69; // _WR variant shares the nr
pub const NR_GEM_BUSY: u32 = 0x57;
pub const NR_GEM_CREATE: u32 = 0x5b;
pub const NR_GEM_PREAD: u32 = 0x5c;
pub const NR_GEM_PWRITE: u32 = 0x5d;
pub const NR_GEM_MMAP: u32 = 0x5e;
pub const NR_GEM_SET_DOMAIN: u32 = 0x5f;
pub const NR_GEM_SW_FINISH: u32 = 0x60;
pub const NR_GEM_SET_TILING: u32 = 0x61;
pub const NR_GEM_GET_TILING: u32 = 0x62;
pub const NR_GEM_GET_APERTURE: u32 = 0x63;
pub const NR_GEM_MMAP_OFFSET: u32 = 0x64; // = MMAP_GTT
pub const NR_GEM_MADVISE: u32 = 0x66;
pub const NR_GEM_WAIT: u32 = 0x6c;
pub const NR_GEM_CONTEXT_CREATE: u32 = 0x6d; // and CREATE_EXT
pub const NR_GEM_CONTEXT_DESTROY: u32 = 0x6e;
pub const NR_GEM_SET_CACHING: u32 = 0x6f;
pub const NR_GEM_GET_CACHING: u32 = 0x70;
pub const NR_REG_READ: u32 = 0x71;
pub const NR_GET_RESET_STATS: u32 = 0x72;
pub const NR_GEM_USERPTR: u32 = 0x73;
pub const NR_GEM_CONTEXT_GETPARAM: u32 = 0x74;
pub const NR_GEM_CONTEXT_SETPARAM: u32 = 0x75;
pub const NR_QUERY: u32 = 0x79;
pub const NR_GEM_VM_CREATE: u32 = 0x7a;
pub const NR_GEM_VM_DESTROY: u32 = 0x7b;
pub const NR_GEM_CREATE_EXT: u32 = 0x7c;

/// Full ioctl numbers as libdrm/Mesa issue them (from the compiled probe).
pub mod ioctl {
    pub const GETPARAM: u32 = 0xc010_6446;
    pub const QUERY: u32 = 0xc010_6479;
    pub const GEM_CREATE: u32 = 0xc010_645b;
    pub const GEM_CREATE_EXT: u32 = 0xc018_647c;
    pub const GEM_MMAP_OFFSET: u32 = 0xc020_6464;
    pub const GEM_MMAP: u32 = 0xc028_645e;
    pub const GEM_EXECBUFFER2: u32 = 0x4040_6469;
    pub const GEM_EXECBUFFER2_WR: u32 = 0xc040_6469;
    pub const GEM_CONTEXT_CREATE_EXT: u32 = 0xc010_646d;
    pub const GEM_CONTEXT_DESTROY: u32 = 0x4008_646e;
    pub const GEM_CONTEXT_GETPARAM: u32 = 0xc018_6474;
    pub const GEM_SET_CACHING: u32 = 0x4008_646f;
    pub const GEM_GET_CACHING: u32 = 0xc008_6470;
    pub const GEM_MADVISE: u32 = 0xc00c_6466;
    pub const GEM_SET_TILING: u32 = 0xc010_6461;
    pub const GEM_GET_TILING: u32 = 0xc010_6462;
    pub const GEM_GET_APERTURE: u32 = 0x8010_6463;
    pub const GEM_USERPTR: u32 = 0xc018_6473;
    pub const REG_READ: u32 = 0xc010_6471;
    pub const GET_RESET_STATS: u32 = 0xc018_6472;
    pub const GEM_VM_CREATE: u32 = 0xc010_647a;
    pub const GEM_VM_DESTROY: u32 = 0x4010_647b;
    pub const GEM_PWRITE: u32 = 0x4020_645d;
    pub const GEM_PREAD: u32 = 0x4020_645c;
    pub const GEM_BUSY: u32 = 0xc008_6457;
    pub const GEM_WAIT: u32 = 0xc010_646c;
    pub const GEM_SET_DOMAIN: u32 = 0x400c_645f;
    pub const GEM_CONTEXT_SETPARAM: u32 = 0xc018_6475;
}

/// Argument size encoded in an ioctl number.
pub const fn ioc_size(cmd: u32) -> u32 { (cmd >> 16) & 0x3FFF }

// ── structures ───────────────────────────────────────────────────────────────

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct Getparam { pub param: i32, pub _pad: u32, pub value_ptr: u64 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct Query { pub num_items: u32, pub flags: u32, pub items_ptr: u64 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct QueryItem { pub query_id: u64, pub length: i32, pub flags: u32, pub data_ptr: u64 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct GemCreate { pub size: u64, pub handle: u32, pub _pad: u32 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct GemCreateExt { pub size: u64, pub handle: u32, pub flags: u32, pub extensions: u64 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct UserExtension { pub next_extension: u64, pub name: u32, pub flags: u32, pub rsvd: [u32; 4] }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct CreateExtMemoryRegions { pub base: UserExtension, pub pad: u32, pub num_regions: u32, pub regions: u64 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct MemoryClassInstance { pub class: u16, pub instance: u16 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct GemMmapOffset { pub handle: u32, pub pad: u32, pub offset: u64, pub flags: u64, pub extensions: u64 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct GemMmap { pub handle: u32, pub pad: u32, pub offset: u64, pub size: u64, pub addr_ptr: u64, pub flags: u64 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct GemSetDomain { pub handle: u32, pub read_domains: u32, pub write_domain: u32 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct GemCaching { pub handle: u32, pub caching: u32 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct GemBusy { pub handle: u32, pub busy: u32 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct GemWait { pub bo_handle: u32, pub flags: u32, pub timeout_ns: i64 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct GemMadvise { pub handle: u32, pub madv: u32, pub retained: u32 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct GemTiling { pub handle: u32, pub tiling_mode: u32, pub stride: u32, pub swizzle_mode: u32 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct GemGetTiling { pub handle: u32, pub tiling_mode: u32, pub swizzle_mode: u32, pub phys_swizzle_mode: u32 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct GemGetAperture { pub aper_size: u64, pub aper_available_size: u64 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct GemUserptr { pub user_ptr: u64, pub user_size: u64, pub flags: u32, pub handle: u32 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct Execbuffer2 {
    pub buffers_ptr: u64,
    pub buffer_count: u32,
    pub batch_start_offset: u32,
    pub batch_len: u32,
    pub dr1: u32,
    pub dr4: u32,
    pub num_cliprects: u32,
    pub cliprects_ptr: u64,
    pub flags: u64,
    /// Context id (low 32 bits).
    pub rsvd1: u64,
    /// In-fence fd (FENCE_IN, low 32) / out-fence fd (FENCE_OUT, high 32).
    pub rsvd2: u64,
}

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct ExecObject2 {
    pub handle: u32,
    pub relocation_count: u32,
    pub relocs_ptr: u64,
    pub alignment: u64,
    pub offset: u64,
    pub flags: u64,
    pub rsvd1: u64,
    pub rsvd2: u64,
}

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct ExecFence { pub handle: u32, pub flags: u32 }

/// drm_i915_gem_execbuffer_ext_timeline_fences (extension name 0).
#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct ExecTimelineFences { pub base: UserExtension, pub fence_count: u64, pub handles_ptr: u64, pub values_ptr: u64 }

/// drm_i915_gem_pwrite / drm_i915_gem_pread (same layout).
#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct GemPrw { pub handle: u32, pub pad: u32, pub offset: u64, pub size: u64, pub data_ptr: u64 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct ContextCreateExt { pub ctx_id: u32, pub flags: u32, pub extensions: u64 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct ContextDestroy { pub ctx_id: u32, pub pad: u32 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct ContextParam { pub ctx_id: u32, pub size: u32, pub param: u64, pub value: u64 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct ContextCreateExtSetparam { pub base: UserExtension, pub param: ContextParam }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct VmControl { pub extensions: u64, pub flags: u32, pub vm_id: u32 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct ResetStats { pub ctx_id: u32, pub flags: u32, pub reset_count: u32, pub batch_active: u32, pub batch_pending: u32, pub pad: u32 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct RegRead { pub offset: u64, pub val: u64 }

#[repr(C)] #[derive(Clone, Copy, Default, Debug)]
pub struct EngineClassInstance { pub class: u16, pub instance: u16 }

const _: () = {
    assert!(core::mem::size_of::<ExecTimelineFences>() == 56);
    assert!(core::mem::size_of::<GemPrw>() == 32);
    use core::mem::{offset_of, size_of};
    assert!(size_of::<Getparam>() == 16 && offset_of!(Getparam, value_ptr) == 8);
    assert!(size_of::<Query>() == 16);
    assert!(size_of::<QueryItem>() == 24 && offset_of!(QueryItem, length) == 8 && offset_of!(QueryItem, data_ptr) == 16);
    assert!(size_of::<GemCreate>() == 16);
    assert!(size_of::<GemCreateExt>() == 24 && offset_of!(GemCreateExt, extensions) == 16);
    assert!(size_of::<UserExtension>() == 32);
    assert!(size_of::<CreateExtMemoryRegions>() == 48 && offset_of!(CreateExtMemoryRegions, regions) == 40);
    assert!(size_of::<GemMmapOffset>() == 32 && offset_of!(GemMmapOffset, offset) == 8 && offset_of!(GemMmapOffset, flags) == 16);
    assert!(size_of::<GemMmap>() == 40 && offset_of!(GemMmap, addr_ptr) == 24 && offset_of!(GemMmap, flags) == 32);
    assert!(size_of::<GemSetDomain>() == 12);
    assert!(size_of::<GemCaching>() == 8);
    assert!(size_of::<GemBusy>() == 8);
    assert!(size_of::<GemWait>() == 16 && offset_of!(GemWait, timeout_ns) == 8);
    assert!(size_of::<GemMadvise>() == 12);
    assert!(size_of::<GemTiling>() == 16);
    assert!(size_of::<GemGetTiling>() == 16);
    assert!(size_of::<GemGetAperture>() == 16);
    assert!(size_of::<GemUserptr>() == 24);
    assert!(size_of::<Execbuffer2>() == 64);
    assert!(offset_of!(Execbuffer2, batch_start_offset) == 12 && offset_of!(Execbuffer2, batch_len) == 16);
    assert!(offset_of!(Execbuffer2, flags) == 40 && offset_of!(Execbuffer2, rsvd1) == 48 && offset_of!(Execbuffer2, rsvd2) == 56);
    assert!(size_of::<ExecObject2>() == 56 && offset_of!(ExecObject2, offset) == 24 && offset_of!(ExecObject2, flags) == 32);
    assert!(size_of::<ExecFence>() == 8);
    assert!(size_of::<ContextCreateExt>() == 16);
    assert!(size_of::<ContextDestroy>() == 8);
    assert!(size_of::<ContextParam>() == 24 && offset_of!(ContextParam, value) == 16);
    assert!(size_of::<ContextCreateExtSetparam>() == 56);
    assert!(size_of::<VmControl>() == 16);
    assert!(size_of::<ResetStats>() == 24);
    assert!(size_of::<RegRead>() == 16);
    assert!(size_of::<EngineClassInstance>() == 4);
};

// ── flags ────────────────────────────────────────────────────────────────────

pub const EXEC_RING_MASK: u64 = 0x3F;
pub const EXEC_DEFAULT: u64 = 0;
pub const EXEC_RENDER: u64 = 1;
pub const EXEC_BSD: u64 = 2;
pub const EXEC_BLT: u64 = 3;
pub const EXEC_VEBOX: u64 = 4;
pub const EXEC_CONSTANTS_MASK: u64 = 3 << 6;
pub const EXEC_GEN7_SOL_RESET: u64 = 1 << 8;
pub const EXEC_SECURE: u64 = 1 << 9;
pub const EXEC_IS_PINNED: u64 = 1 << 10;
pub const EXEC_NO_RELOC: u64 = 1 << 11;
pub const EXEC_HANDLE_LUT: u64 = 1 << 12;
pub const EXEC_BSD_MASK: u64 = 3 << 13;
pub const EXEC_RESOURCE_STREAMER: u64 = 1 << 15;
pub const EXEC_FENCE_IN: u64 = 1 << 16;
pub const EXEC_FENCE_OUT: u64 = 1 << 17;
pub const EXEC_BATCH_FIRST: u64 = 1 << 18;
pub const EXEC_FENCE_ARRAY: u64 = 1 << 19;
pub const EXEC_FENCE_SUBMIT: u64 = 1 << 20;
pub const EXEC_USE_EXTENSIONS: u64 = 1 << 21;
pub const EXEC_UNKNOWN_FLAGS: u64 = !((1 << 22) - 1);

pub const EXEC_OBJECT_NEEDS_FENCE: u64 = 1 << 0;
pub const EXEC_OBJECT_NEEDS_GTT: u64 = 1 << 1;
pub const EXEC_OBJECT_WRITE: u64 = 1 << 2;
pub const EXEC_OBJECT_SUPPORTS_48B_ADDRESS: u64 = 1 << 3;
pub const EXEC_OBJECT_PINNED: u64 = 1 << 4;
pub const EXEC_OBJECT_PAD_TO_SIZE: u64 = 1 << 5;
pub const EXEC_OBJECT_ASYNC: u64 = 1 << 6;
pub const EXEC_OBJECT_CAPTURE: u64 = 1 << 7;
pub const EXEC_OBJECT_UNKNOWN_FLAGS: u64 = !0xFF;

pub const EXECBUFFER_EXT_TIMELINE_FENCES: u32 = 0;

pub const EXEC_FENCE_WAIT: u32 = 1 << 0;
pub const EXEC_FENCE_SIGNAL: u32 = 1 << 1;

pub const CONTEXT_CREATE_FLAGS_USE_EXTENSIONS: u32 = 1 << 0;
pub const CONTEXT_CREATE_FLAGS_SINGLE_TIMELINE: u32 = 1 << 1;
pub const CONTEXT_CREATE_EXT_SETPARAM: u32 = 0;

pub const CONTEXT_PARAM_BAN_PERIOD: u64 = 0x1;
pub const CONTEXT_PARAM_NO_ZEROMAP: u64 = 0x2;
pub const CONTEXT_PARAM_GTT_SIZE: u64 = 0x3;
pub const CONTEXT_PARAM_NO_ERROR_CAPTURE: u64 = 0x4;
pub const CONTEXT_PARAM_BANNABLE: u64 = 0x5;
pub const CONTEXT_PARAM_PRIORITY: u64 = 0x6;
pub const CONTEXT_PARAM_SSEU: u64 = 0x7;
pub const CONTEXT_PARAM_RECOVERABLE: u64 = 0x8;
pub const CONTEXT_PARAM_VM: u64 = 0x9;
pub const CONTEXT_PARAM_ENGINES: u64 = 0xa;
pub const CONTEXT_PARAM_PERSISTENCE: u64 = 0xb;
pub const CONTEXT_PARAM_RINGSIZE: u64 = 0xc;
pub const CONTEXT_PARAM_PROTECTED_CONTENT: u64 = 0xd;
pub const CONTEXT_PARAM_LOW_LATENCY: u64 = 0xe;

pub const GEM_CREATE_EXT_MEMORY_REGIONS: u32 = 0;
pub const GEM_CREATE_EXT_PROTECTED_CONTENT: u32 = 1;
pub const GEM_CREATE_EXT_SET_PAT: u32 = 2;
pub const GEM_CREATE_EXT_FLAG_NEEDS_CPU_ACCESS: u32 = 1;

pub const MMAP_OFFSET_GTT: u64 = 0;
pub const MMAP_OFFSET_WC: u64 = 1;
pub const MMAP_OFFSET_WB: u64 = 2;
pub const MMAP_OFFSET_UC: u64 = 3;
pub const MMAP_OFFSET_FIXED: u64 = 4;
/// Legacy GEM_MMAP flag.
pub const MMAP_WC: u64 = 1;

pub const QUERY_TOPOLOGY_INFO: u64 = 1;
pub const QUERY_ENGINE_INFO: u64 = 2;
pub const QUERY_PERF_CONFIG: u64 = 3;
pub const QUERY_MEMORY_REGIONS: u64 = 4;
pub const QUERY_HWCONFIG_BLOB: u64 = 5;
pub const QUERY_GEOMETRY_SUBSLICES: u64 = 6;
pub const QUERY_GUC_SUBMISSION_VERSION: u64 = 7;

pub const MEMORY_CLASS_SYSTEM: u16 = 0;
pub const MEMORY_CLASS_DEVICE: u16 = 1;

pub const CACHING_NONE: u32 = 0;
pub const CACHING_CACHED: u32 = 1;
pub const CACHING_DISPLAY: u32 = 2;

pub const GEM_DOMAIN_CPU: u32 = 0x1;
pub const GEM_DOMAIN_RENDER: u32 = 0x2;
pub const GEM_DOMAIN_SAMPLER: u32 = 0x4;
pub const GEM_DOMAIN_COMMAND: u32 = 0x8;
pub const GEM_DOMAIN_INSTRUCTION: u32 = 0x10;
pub const GEM_DOMAIN_VERTEX: u32 = 0x20;
pub const GEM_DOMAIN_GTT: u32 = 0x40;
pub const GEM_DOMAIN_WC: u32 = 0x80;

pub const TILING_NONE: u32 = 0;
pub const TILING_X: u32 = 1;
pub const TILING_Y: u32 = 2;
pub const BIT_6_SWIZZLE_NONE: u32 = 0;

pub const MADV_WILLNEED: u32 = 0;
pub const MADV_DONTNEED: u32 = 1;

pub const ENGINE_INFO_HAS_LOGICAL_INSTANCE: u64 = 1 << 0;

// ── GETPARAM ─────────────────────────────────────────────────────────────────

pub mod param {
    pub const IRQ_ACTIVE: i32 = 1;
    pub const ALLOW_BATCHBUFFER: i32 = 2;
    pub const LAST_DISPATCH: i32 = 3;
    pub const CHIPSET_ID: i32 = 4;
    pub const HAS_GEM: i32 = 5;
    pub const NUM_FENCES_AVAIL: i32 = 6;
    pub const HAS_OVERLAY: i32 = 7;
    pub const HAS_PAGEFLIPPING: i32 = 8;
    pub const HAS_EXECBUF2: i32 = 9;
    pub const HAS_BSD: i32 = 10;
    pub const HAS_BLT: i32 = 11;
    pub const HAS_RELAXED_FENCING: i32 = 12;
    pub const HAS_COHERENT_RINGS: i32 = 13;
    pub const HAS_EXEC_CONSTANTS: i32 = 14;
    pub const HAS_RELAXED_DELTA: i32 = 15;
    pub const HAS_GEN7_SOL_RESET: i32 = 16;
    pub const HAS_LLC: i32 = 17;
    pub const HAS_ALIASING_PPGTT: i32 = 18;
    pub const HAS_WAIT_TIMEOUT: i32 = 19;
    pub const HAS_SEMAPHORES: i32 = 20;
    pub const HAS_PRIME_VMAP_FLUSH: i32 = 21;
    pub const HAS_VEBOX: i32 = 22;
    pub const HAS_SECURE_BATCHES: i32 = 23;
    pub const HAS_PINNED_BATCHES: i32 = 24;
    pub const HAS_EXEC_NO_RELOC: i32 = 25;
    pub const HAS_EXEC_HANDLE_LUT: i32 = 26;
    pub const HAS_WT: i32 = 27;
    pub const CMD_PARSER_VERSION: i32 = 28;
    pub const HAS_COHERENT_PHYS_GTT: i32 = 29;
    pub const MMAP_VERSION: i32 = 30;
    pub const HAS_BSD2: i32 = 31;
    pub const REVISION: i32 = 32;
    pub const SUBSLICE_TOTAL: i32 = 33;
    pub const EU_TOTAL: i32 = 34;
    pub const HAS_GPU_RESET: i32 = 35;
    pub const HAS_RESOURCE_STREAMER: i32 = 36;
    pub const HAS_EXEC_SOFTPIN: i32 = 37;
    pub const HAS_POOLED_EU: i32 = 38;
    pub const MIN_EU_IN_POOL: i32 = 39;
    pub const MMAP_GTT_VERSION: i32 = 40;
    pub const HAS_SCHEDULER: i32 = 41;
    pub const HUC_STATUS: i32 = 42;
    pub const HAS_EXEC_ASYNC: i32 = 43;
    pub const HAS_EXEC_FENCE: i32 = 44;
    pub const HAS_EXEC_CAPTURE: i32 = 45;
    pub const SLICE_MASK: i32 = 46;
    pub const SUBSLICE_MASK: i32 = 47;
    pub const HAS_EXEC_BATCH_FIRST: i32 = 48;
    pub const HAS_EXEC_FENCE_ARRAY: i32 = 49;
    pub const HAS_CONTEXT_ISOLATION: i32 = 50;
    pub const CS_TIMESTAMP_FREQUENCY: i32 = 51;
    pub const MMAP_GTT_COHERENT: i32 = 52;
    pub const HAS_EXEC_SUBMIT_FENCE: i32 = 53;
    pub const PERF_REVISION: i32 = 54;
    pub const HAS_EXEC_TIMELINE_FENCES: i32 = 55;
    pub const HAS_USERPTR_PROBE: i32 = 56;
    pub const OA_TIMESTAMP_FREQUENCY: i32 = 57;
    pub const PXP_STATUS: i32 = 58;
    pub const HAS_CONTEXT_FREQ_HINT: i32 = 59;
}

/// I915_SCHEDULER_CAP_* bits.
pub const SCHEDULER_CAP_ENABLED: i32 = 1 << 0;
pub const SCHEDULER_CAP_PRIORITY: i32 = 1 << 1;

/// What the device answers GETPARAM / QUERY from.
#[derive(Clone, Debug)]
pub struct Facts {
    pub device_id: u16,
    pub revision: u8,
    pub has_llc: bool,
    pub sseu: Sseu,
    pub cs_timestamp_hz: u32,
    /// Engines exposed, as (uapi class, instance).
    pub engines: Vec<(u16, u16)>,
    /// System memory size (bytes) and how much is free.
    pub ram_total: u64,
    pub ram_free: u64,
    /// CPU-visible GGTT aperture (GET_APERTURE).
    pub aperture: u64,
    /// Feature switches the backend actually implements.
    pub exec_fence: bool,
    pub exec_fence_array: bool,
    pub timeline_fences: bool,
}

impl Facts {
    fn has_class(&self, c: u16) -> bool { self.engines.iter().any(|e| e.0 == c) }
}

/// `i915_getparam_ioctl` for a Gen9 big-core part with full 48-bit PPGTT,
/// execlist submission and softpin. `Err(errno)` as Linux answers.
pub fn getparam(p: i32, f: &Facts) -> Result<i32, i32> {
    use param::*;
    let s = &f.sseu;
    Ok(match p {
        IRQ_ACTIVE | ALLOW_BATCHBUFFER | LAST_DISPATCH => return Err(ENODEV),
        CHIPSET_ID => f.device_id as i32,
        REVISION => f.revision as i32,
        HAS_GEM | HAS_EXECBUF2 | HAS_RELAXED_FENCING | HAS_COHERENT_RINGS | HAS_RELAXED_DELTA
        | HAS_WAIT_TIMEOUT | HAS_PRIME_VMAP_FLUSH | HAS_PINNED_BATCHES | HAS_EXEC_NO_RELOC
        | HAS_EXEC_HANDLE_LUT | HAS_COHERENT_PHYS_GTT | HAS_EXEC_SOFTPIN | HAS_EXEC_ASYNC
        | HAS_EXEC_CAPTURE | HAS_EXEC_BATCH_FIRST | MMAP_GTT_COHERENT | HAS_PAGEFLIPPING
        | HAS_GPU_RESET => 1,
        HAS_GEN7_SOL_RESET | HAS_OVERLAY | HAS_SECURE_BATCHES | HAS_EXEC_CONSTANTS | HAS_WT
        | HAS_RESOURCE_STREAMER | HAS_POOLED_EU | MIN_EU_IN_POOL | HAS_SEMAPHORES
        | HAS_EXEC_SUBMIT_FENCE | HAS_USERPTR_PROBE | CMD_PARSER_VERSION | HAS_BSD2 => 0,
        NUM_FENCES_AVAIL => 32,
        HAS_BSD => f.has_class(2) as i32,
        HAS_BLT => f.has_class(1) as i32,
        HAS_VEBOX => f.has_class(3) as i32,
        HAS_LLC => f.has_llc as i32,
        // INTEL_PPGTT_FULL.
        HAS_ALIASING_PPGTT => 2,
        MMAP_VERSION => 1,
        // >= 4: MMAP_OFFSET exists (Mesa's has_mmap_offset).
        MMAP_GTT_VERSION => 4,
        SUBSLICE_TOTAL => s.subslices_total() as i32,
        EU_TOTAL => s.eu_total as i32,
        SLICE_MASK => s.slice_mask as i32,
        SUBSLICE_MASK => s.subslice_mask as i32,
        HAS_SCHEDULER => SCHEDULER_CAP_ENABLED,
        HAS_EXEC_FENCE => f.exec_fence as i32,
        HAS_EXEC_FENCE_ARRAY => f.exec_fence_array as i32,
        HAS_EXEC_TIMELINE_FENCES => f.timeline_fences as i32,
        // Bitmask of engine classes whose contexts are isolated (all of ours:
        // every context has its own logical ring context and PPGTT).
        HAS_CONTEXT_ISOLATION => f.engines.iter().fold(0, |m, e| m | 1 << e.0),
        CS_TIMESTAMP_FREQUENCY => f.cs_timestamp_hz as i32,
        HUC_STATUS | PXP_STATUS => return Err(ENODEV),
        PERF_REVISION | OA_TIMESTAMP_FREQUENCY | HAS_CONTEXT_FREQ_HINT => return Err(EINVAL),
        _ => return Err(EINVAL),
    })
}

// ── QUERY ────────────────────────────────────────────────────────────────────

/// One query item's answer: the blob, or an errno for `item.length`.
pub fn query_blob(id: u64, item_flags: u32, f: &Facts) -> Result<Vec<u8>, i32> {
    match id {
        QUERY_TOPOLOGY_INFO => {
            if item_flags != 0 { return Err(EINVAL); }
            Ok(topology_blob(&f.sseu))
        }
        QUERY_ENGINE_INFO => {
            if item_flags != 0 { return Err(EINVAL); }
            Ok(engine_info_blob(&f.engines))
        }
        QUERY_MEMORY_REGIONS => {
            if item_flags != 0 { return Err(EINVAL); }
            Ok(memory_regions_blob(f.ram_total, f.ram_free))
        }
        // Gen9 has no GuC submission / hwconfig table / geometry subslices.
        QUERY_HWCONFIG_BLOB | QUERY_GEOMETRY_SUBSLICES | QUERY_GUC_SUBMISSION_VERSION => Err(ENODEV),
        QUERY_PERF_CONFIG => Err(ENODEV),
        _ => Err(EINVAL),
    }
}

/// The `copy_query_item` protocol: `length == 0` asks for the size; a short
/// buffer is EINVAL; otherwise the data is copied. Returns the value to
/// store in `item.length` and whether to copy the blob.
pub fn query_length(user_len: i32, blob: &Result<Vec<u8>, i32>) -> (i32, bool) {
    match blob {
        Err(e) => (-*e, false),
        Ok(b) => {
            let need = b.len() as i32;
            if user_len == 0 { (need, false) }
            else if user_len < need { (-EINVAL, false) }
            else { (need, true) }
        }
    }
}

/// struct drm_i915_query_engine_info + drm_i915_engine_info[].
pub fn engine_info_blob(engines: &[(u16, u16)]) -> Vec<u8> {
    let mut v = Vec::with_capacity(16 + 56 * engines.len());
    v.extend_from_slice(&(engines.len() as u32).to_le_bytes());
    v.extend_from_slice(&[0u8; 12]);
    for &(class, inst) in engines {
        v.extend_from_slice(&class.to_le_bytes());
        v.extend_from_slice(&inst.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // rsvd0
        v.extend_from_slice(&ENGINE_INFO_HAS_LOGICAL_INSTANCE.to_le_bytes()); // flags
        v.extend_from_slice(&0u64.to_le_bytes()); // capabilities
        v.extend_from_slice(&inst.to_le_bytes()); // logical_instance
        v.extend_from_slice(&[0u8; 6]); // rsvd1[3]
        v.extend_from_slice(&[0u8; 24]); // rsvd2[3]
    }
    v
}

/// struct drm_i915_query_memory_regions with the single system region.
pub fn memory_regions_blob(total: u64, free: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(16 + 88);
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&[0u8; 12]);
    v.extend_from_slice(&MEMORY_CLASS_SYSTEM.to_le_bytes());
    v.extend_from_slice(&0u16.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(&total.to_le_bytes());
    v.extend_from_slice(&free.to_le_bytes());
    v.extend_from_slice(&[0u8; 64]); // probed/unallocated cpu-visible (0: not small-BAR) + rsvd
    v
}

// ── EXECBUFFER2 validation ───────────────────────────────────────────────────

/// Canonical 48-bit GPU address -> the plain offset (`gen8_noncanonical_addr`).
pub fn noncanonical(addr: u64) -> u64 { addr & ((1 << 48) - 1) }

/// A canonical 48-bit address (`gen8_canonical_addr`): bit 47 sign-extended.
pub fn canonical(addr: u64) -> u64 { (((addr << 16) as i64) >> 16) as u64 }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecPlan {
    /// Index into the context's engine map (or the legacy ring selector).
    pub engine: u32,
    /// Index of the batch object in the exec list.
    pub batch_index: u32,
    pub batch_start: u32,
    pub fence_array: bool,
    /// cliprects_ptr is an i915_user_extension chain.
    pub extensions: bool,
    pub fence_in: bool,
    pub fence_out: bool,
}

/// Static checks of an EXECBUFFER2 (`i915_gem_check_execbuffer` +
/// `eb_select_engine` shape): what to run where, or the errno.
/// `user_engines` = Some(n) when the context has an engine map of n
/// entries; None = legacy ring selection (DEFAULT/RENDER/BLT/...).
pub fn check_execbuf(e: &Execbuffer2, user_engines: Option<u32>, legacy_ok: &dyn Fn(u64) -> bool) -> Result<ExecPlan, i32> {
    if e.flags & EXEC_UNKNOWN_FLAGS != 0 { return Err(EINVAL); }
    if e.buffer_count < 1 || e.buffer_count > 1 << 20 { return Err(EINVAL); }
    if e.batch_start_offset & 7 != 0 { return Err(EINVAL); }
    // DR1/DR4 must be zero; cliprects only as the fence array / extensions.
    if e.dr1 != 0 || e.dr4 != 0 { return Err(EINVAL); }
    if e.flags & (EXEC_SECURE | EXEC_RESOURCE_STREAMER | EXEC_FENCE_SUBMIT) != 0 {
        return Err(EINVAL);
    }
    let fence_array = e.flags & EXEC_FENCE_ARRAY != 0;
    // USE_EXTENSIONS: cliprects_ptr is the extension chain (timeline fences),
    // which cannot share the field with a fence array.
    let extensions = e.flags & EXEC_USE_EXTENSIONS != 0;
    if extensions && (fence_array || e.num_cliprects != 0) { return Err(EINVAL); }
    if !fence_array && !extensions && (e.num_cliprects != 0 || e.cliprects_ptr != 0) { return Err(EINVAL); }
    let ring = e.flags & EXEC_RING_MASK;
    let engine = match user_engines {
        Some(n) => {
            if ring >= n as u64 { return Err(EINVAL); }
            ring as u32
        }
        None => {
            if !legacy_ok(ring) { return Err(EINVAL); }
            ring as u32
        }
    };
    let batch_index = if e.flags & EXEC_BATCH_FIRST != 0 { 0 } else { e.buffer_count - 1 };
    Ok(ExecPlan {
        engine, batch_index, batch_start: e.batch_start_offset, fence_array, extensions,
        fence_in: e.flags & EXEC_FENCE_IN != 0, fence_out: e.flags & EXEC_FENCE_OUT != 0,
    })
}

/// Per-object checks: softpin only (no relocations), page-aligned canonical
/// 48-bit offsets that fit the VM, known flags. Returns the plain offset.
pub fn check_exec_object(o: &ExecObject2, vm_size: u64, obj_size: u64) -> Result<u64, i32> {
    if o.flags & EXEC_OBJECT_UNKNOWN_FLAGS != 0 { return Err(EINVAL); }
    if o.relocation_count != 0 { return Err(EINVAL); }
    if o.flags & EXEC_OBJECT_PINNED == 0 { return Err(EINVAL); }
    if o.offset != canonical(o.offset) { return Err(EINVAL); }
    let off = noncanonical(o.offset);
    if off & 0xFFF != 0 { return Err(EINVAL); }
    if off >= 1 << 32 && o.flags & EXEC_OBJECT_SUPPORTS_48B_ADDRESS == 0 { return Err(EINVAL); }
    if off.checked_add(obj_size).map_or(true, |end| end > vm_size) { return Err(EINVAL); }
    Ok(off)
}

/// CONTEXT GETPARAM answers that need no context state.
pub fn context_getparam_static(p: u64) -> Option<u64> {
    match p {
        CONTEXT_PARAM_GTT_SIZE => Some(1 << 48),
        CONTEXT_PARAM_BANNABLE => Some(1),
        CONTEXT_PARAM_RECOVERABLE => Some(1),
        CONTEXT_PARAM_PRIORITY => Some(0),
        CONTEXT_PARAM_PERSISTENCE => Some(1),
        CONTEXT_PARAM_NO_ERROR_CAPTURE => Some(0),
        _ => None,
    }
}

/// Decode an I915_CONTEXT_PARAM_ENGINES value (struct
/// i915_context_param_engines: u64 extensions, then class/instance pairs)
/// of `size` bytes into the requested engines, checking each exists.
pub fn parse_engine_map(bytes: &[u8], have: &[(u16, u16)]) -> Result<Vec<(u16, u16)>, i32> {
    if bytes.len() < 8 || (bytes.len() - 8) % 4 != 0 { return Err(EINVAL); }
    let ext = u64::from_le_bytes(bytes[..8].try_into().unwrap());
    if ext != 0 { return Err(EINVAL); } // load-balance / bond extensions not supported
    let mut v = Vec::new();
    for c in bytes[8..].chunks(4) {
        let class = u16::from_le_bytes([c[0], c[1]]);
        let inst = u16::from_le_bytes([c[2], c[3]]);
        // I915_ENGINE_CLASS_INVALID / _NONE marks an unused slot.
        if class == 0xFFFF && inst == 0xFFFF { v.push((class, inst)); continue; }
        if !have.contains(&(class, inst)) { return Err(EINVAL); }
        v.push((class, inst));
    }
    if v.len() > 64 { return Err(EINVAL); }
    Ok(v)
}
