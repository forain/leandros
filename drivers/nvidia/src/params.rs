//! RM API constants and parameter blocks (OpenRM 570.86.16 SDK headers,
//! src/common/sdk/nvidia/inc), as `#[repr(C)]` Rust types.
//!
//! Every value and every size/offset here that RM reads is also exported by
//! the C glue (`lnv_abi_table` in ports/openrm/glue/lnv_rmapi.c, compiled
//! against NVIDIA's own headers); `client::abi_check` compares the two lists
//! entry by entry before anything talks to RM. Names in [`ABI`] are the C
//! table's names.

#![allow(non_upper_case_globals, non_snake_case, non_camel_case_types, dead_code)]

use core::mem::{offset_of, size_of};

pub type Handle = u32;

// ------------------------------------------------------------- classes --

pub const NV01_ROOT: u32 = 0x0000;
pub const NV01_DEVICE_0: u32 = 0x0080;
pub const NV20_SUBDEVICE_0: u32 = 0x2080;
pub const NV01_MEMORY_SYSTEM: u32 = 0x003e;
pub const NV01_MEMORY_LOCAL_USER: u32 = 0x0040;
pub const NV01_MEMORY_FRAMEBUFFER_CONSOLE: u32 = 0x0076;
pub const NV50_MEMORY_VIRTUAL: u32 = 0x50a0;
pub const FERMI_VASPACE_A: u32 = 0x90f1;
pub const KEPLER_CHANNEL_GROUP_A: u32 = 0xa06c;
pub const AMPERE_CHANNEL_GPFIFO_A: u32 = 0xc56f;
pub const AMPERE_DMA_COPY_A: u32 = 0xc6b5;
pub const AMPERE_DMA_COPY_B: u32 = 0xc7b5;
pub const VOLTA_USERMODE_A: u32 = 0xc361;
// The engine classes the nouveau-UAPI layer (`uapi`) instantiates on a
// graphics channel for NVK, and the context DMA its error notifier needs.
pub const NV01_CONTEXT_DMA: u32 = 0x0002;
pub const FERMI_TWOD_A: u32 = 0x902d;
pub const KEPLER_INLINE_TO_MEMORY_B: u32 = 0xa140;
pub const AMPERE_A: u32 = 0xc697;
pub const AMPERE_B: u32 = 0xc797;
pub const AMPERE_COMPUTE_A: u32 = 0xc6c0;
pub const AMPERE_COMPUTE_B: u32 = 0xc7c0;

// ------------------------------------------------------------ controls --

pub const NV0000_CTRL_CMD_SYSTEM_GET_BUILD_VERSION_V2: u32 = 0x13e;
pub const NV0000_CTRL_CMD_GPU_GET_ATTACHED_IDS: u32 = 0x201;
pub const NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2: u32 = 0x205;
pub const NV0041_CTRL_CMD_GET_SURFACE_PHYS_ATTR: u32 = 0x0041_0103;
pub const NV0080_CTRL_CMD_GPU_GET_CLASSLIST_V2: u32 = 0x0080_0292;
pub const NV2080_CTRL_CMD_GPU_GET_NAME_STRING: u32 = 0x2080_0110;
pub const NV2080_CTRL_CMD_GPU_GET_ENGINES_V2: u32 = 0x2080_0170;
pub const NV2080_CTRL_CMD_FB_GET_INFO_V2: u32 = 0x2080_1303;
pub const NV2080_CTRL_CMD_GR_GET_INFO_V2: u32 = 0x2080_1228;
pub const NV2080_CTRL_CMD_GR_GET_GPC_MASK: u32 = 0x2080_122a;
pub const NV2080_CTRL_CMD_GR_GET_TPC_MASK: u32 = 0x2080_122b;
pub const NV2080_CTRL_CMD_CE_GET_CAPS_V2: u32 = 0x2080_2a03;
pub const NVA06C_CTRL_CMD_GPFIFO_SCHEDULE: u32 = 0xa06c_0101;
pub const NVA06F_CTRL_CMD_GPFIFO_SCHEDULE: u32 = 0xa06f_0103;
pub const NVC36F_CTRL_CMD_GPFIFO_GET_WORK_SUBMIT_TOKEN: u32 = 0xc36f_0108;
pub const NVA06F_CTRL_CMD_BIND: u32 = 0xa06f_0104;
pub const NV2080_CTRL_CMD_MC_GET_ARCH_INFO: u32 = 0x2080_1701;
pub const NV2080_CTRL_CMD_TIMER_GET_TIME: u32 = 0x2080_0403;
pub const NV2080_CTRL_CMD_GPU_GET_SHORT_NAME_STRING: u32 = 0x2080_0111;

// ------------------------------------------------ sizes, indices, misc --

pub const NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_V2_MAX_STRING_SIZE: usize = 256;
pub const NV0000_CTRL_GPU_MAX_ATTACHED_GPUS: usize = 32;
pub const NV0000_CTRL_GPU_INVALID_ID: u32 = 0xffff_ffff;
pub const NV0080_CTRL_GPU_CLASSLIST_MAX_SIZE: usize = 100;
pub const NV2080_GPU_MAX_NAME_STRING_LENGTH: usize = 0x40;
pub const NV2080_GPU_MAX_ENGINES_LIST_SIZE: usize = 0x54;
pub const NV2080_CTRL_FB_INFO_MAX_LIST_SIZE: usize = 0x39;
pub const NV2080_CTRL_GR_INFO_MAX_SIZE: usize = 0x3a;
pub const NV2080_CTRL_CE_CAPS_TBL_SIZE: usize = 2;
pub const NV_MAX_SUBDEVICES: usize = 8;
pub const CC_CHAN_ALLOC_IV_SIZE_DWORD: usize = 3;
pub const CC_CHAN_ALLOC_NONCE_SIZE_DWORD: usize = 8;

pub const NV2080_CTRL_FB_INFO_INDEX_BAR1_SIZE: u32 = 0x05;
pub const NV2080_CTRL_FB_INFO_INDEX_RAM_SIZE: u32 = 0x07;
pub const NV2080_CTRL_FB_INFO_INDEX_TOTAL_RAM_SIZE: u32 = 0x08;
pub const NV2080_CTRL_FB_INFO_INDEX_HEAP_SIZE: u32 = 0x09;
pub const NV2080_CTRL_FB_INFO_INDEX_BUS_WIDTH: u32 = 0x0b;
pub const NV2080_CTRL_FB_INFO_INDEX_RAM_TYPE: u32 = 0x0d;
pub const NV2080_CTRL_FB_INFO_INDEX_HEAP_FREE: u32 = 0x16;
pub const NV2080_CTRL_FB_INFO_INDEX_FBP_COUNT: u32 = 0x19;
pub const NV2080_CTRL_FB_INFO_INDEX_L2CACHE_SIZE: u32 = 0x1b;
pub const NV2080_CTRL_FB_INFO_INDEX_LTC_COUNT: u32 = 0x22;

pub const NV2080_CTRL_GR_INFO_INDEX_SHADER_PIPE_COUNT: u32 = 0x07;
pub const NV2080_CTRL_GR_INFO_INDEX_SM_VERSION: u32 = 0x0c;
pub const NV2080_CTRL_GR_INFO_INDEX_MAX_WARPS_PER_SM: u32 = 0x0d;
pub const NV2080_CTRL_GR_INFO_INDEX_LITTER_NUM_GPCS: u32 = 0x14;
pub const NV2080_CTRL_GR_INFO_INDEX_LITTER_NUM_TPC_PER_GPC: u32 = 0x17;
pub const NV2080_CTRL_GR_INFO_INDEX_GPU_CORE_COUNT: u32 = 0x1d;
pub const NV2080_CTRL_GR_INFO_INDEX_LITTER_NUM_SM_PER_TPC: u32 = 0x20;

pub const NV2080_ENGINE_TYPE_GRAPHICS: u32 = 0x01;
pub const NV2080_ENGINE_TYPE_COPY0: u32 = 0x09;
pub const NV2080_ENGINE_TYPE_COPY9: u32 = 0x12;
pub const NV2080_ENGINE_TYPE_COPY10: u32 = 0x34;

/// NV2080_ENGINE_TYPE_COPY(i).
pub const fn engine_copy(i: u32) -> u32 {
    if i < 10 { NV2080_ENGINE_TYPE_COPY0 + i } else { NV2080_ENGINE_TYPE_COPY10 + i - 10 }
}

/// Inverse of [`engine_copy`]; None for a non-copy engine.
pub const fn copy_index(engine: u32) -> Option<u32> {
    if engine >= NV2080_ENGINE_TYPE_COPY0 && engine <= NV2080_ENGINE_TYPE_COPY9 {
        Some(engine - NV2080_ENGINE_TYPE_COPY0)
    } else if engine >= NV2080_ENGINE_TYPE_COPY10 && engine < NV2080_ENGINE_TYPE_COPY10 + 54 {
        Some(engine - NV2080_ENGINE_TYPE_COPY10 + 10)
    } else {
        None
    }
}

/// NV2080_CTRL_CE_CAPS_* (byte 0 bits).
pub const CE_CAPS_GRCE: u8 = 0x01;
pub const CE_CAPS_SHARED: u8 = 0x02;
pub const CE_CAPS_SYSMEM_READ: u8 = 0x04;
pub const CE_CAPS_SYSMEM_WRITE: u8 = 0x08;

pub const NVA06F_SUBCHANNEL_COPY_ENGINE: u32 = 4;
pub const NVB0B5_ALLOCATION_PARAMETERS_VERSION_1: u32 = 1;
pub const NV_VASPACE_ALLOCATION_INDEX_GPU_NEW: u32 = 0;
/// g_heap_nvoc.h: owner tag of allocations made by in-kernel RM clients.
pub const HEAP_OWNER_RM_KERNEL_CLIENT: u32 = 0xdeaf_000a;

// ----------------------------------------------- memory attribute bits --

pub const NVOS32_TYPE_IMAGE: u32 = 0;
pub const OS32_ATTR_LOCATION_VIDMEM: u32 = 0;
pub const OS32_ATTR_LOCATION_PCI: u32 = 0x0200_0000;
pub const OS32_ATTR_COHERENCY_UNCACHED: u32 = 0;
pub const OS32_ATTR_COHERENCY_CACHED: u32 = 0x2000_0000;
pub const OS32_ATTR_PHYSICALITY_NONCONTIGUOUS: u32 = 0x0800_0000;
pub const OS32_ATTR_PHYSICALITY_CONTIGUOUS: u32 = 0x1000_0000;
pub const OS32_ATTR_PAGE_SIZE_DEFAULT: u32 = 0;
pub const OS32_ATTR_PAGE_SIZE_4KB: u32 = 0x0080_0000;
pub const OS32_ATTR_PAGE_SIZE_BIG: u32 = 0x0100_0000;
pub const NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE: u32 = 0x0000_0100;
pub const NVOS32_ALLOC_FLAGS_PERSISTENT_VIDMEM: u32 = 0x0001_0000;
pub const NVOS32_ALLOC_FLAGS_VIRTUAL: u32 = 0x0008_0000;
pub const NVOS32_ALLOC_FLAGS_ALLOCATE_KERNEL_PRIVILEGED: u32 = 0x0800_0000;
pub const NVOS32_ALLOC_FLAGS_FIXED_ADDRESS_ALLOCATE: u32 = 0x0000_0010;
pub const NVOS32_ALLOC_FLAGS_USE_BEGIN_END: u32 = 0x0002_0000;
pub const NVOS32_ALLOC_FLAGS_SPARSE: u32 = 0x0400_0000;
pub const NVOS32_TYPE_DEPTH: u32 = 1;
pub const NVOS32_TYPE_STENCIL: u32 = 16;
/// NVOS32_ATTR fields that select a depth/stencil PTE kind (DRF_DEF values).
pub const OS32_ATTR_DEPTH_8: u32 = 0x1;
pub const OS32_ATTR_DEPTH_16: u32 = 0x2;
pub const OS32_ATTR_DEPTH_32: u32 = 0x4;
pub const OS32_ATTR_DEPTH_64: u32 = 0x5;
pub const OS32_ATTR_COMPR_NONE: u32 = 0;
pub const OS32_ATTR_FORMAT_PITCH: u32 = 0;
pub const OS32_ATTR_FORMAT_BLOCK_LINEAR: u32 = 0x0002_0000;
pub const OS32_ATTR_Z_TYPE_FIXED: u32 = 0;
pub const OS32_ATTR_Z_TYPE_FLOAT: u32 = 0x0004_0000;
pub const OS32_ATTR_ZS_PACKING_S8: u32 = 0;
pub const OS32_ATTR_ZS_PACKING_Z24S8: u32 = 0;
pub const OS32_ATTR_ZS_PACKING_S8Z24: u32 = 0x0008_0000;
pub const OS32_ATTR_ZS_PACKING_Z32_X24S8: u32 = 0x0028_0000;
pub const OS32_ATTR_ZS_PACKING_Z16: u32 = 0x0038_0000;
pub const OS46_FLAGS_PAGE_KIND_VIRTUAL: u32 = 0x8;
pub const OS46_FLAGS_CACHE_SNOOP_DISABLE: u32 = 0;
pub const OS46_FLAGS_DMA_OFFSET_FIXED_TRUE: u32 = 0x8000;
pub const OS03_FLAGS_MAPPING_KERNEL: u32 = 0x0010_0000;
pub const OS03_FLAGS_HASH_TABLE_DISABLE: u32 = 0x2000_0000;
pub const NV_CHANNELGPFIFO_NOTIFICATION_TYPE_ERROR: u32 = 0;
pub const OS33_FLAGS_ACCESS_READ_WRITE: u32 = 0;
pub const OS33_FLAGS_ACCESS_WRITE_ONLY: u32 = 2;
pub const OS46_FLAGS_CACHE_SNOOP_ENABLE: u32 = 0x10;
pub const OS46_FLAGS_PAGE_SIZE_DEFAULT: u32 = 0;
pub const OS46_FLAGS_PAGE_SIZE_4KB: u32 = 0x100;

pub const NV0041_CTRL_CMD_GET_SURFACE_PHYS_ATTR_APERTURE_VIDMEM: u32 = 0;
pub const NV0041_CTRL_CMD_GET_SURFACE_PHYS_ATTR_APERTURE_SYSMEM: u32 = 1;
pub const NVC361_NOTIFY_CHANNEL_PENDING: usize = 0x90;
pub const NVC361_NV_USERMODE__SIZE: u64 = 0x1_0000;
pub const NVC56F_GP_ENTRY__SIZE: usize = 8;
/// AmpereAControlGPFifo (USERD) field offsets.
pub const USERD_PUT: usize = 0x40;
pub const USERD_GET: usize = 0x44;
pub const USERD_GP_GET: usize = 0x88;
pub const USERD_GP_PUT: usize = 0x8c;

// ---------------------------------------------------- parameter blocks --

/// NVOS21_PARAMETERS (only checked: the C glue fills it).
#[repr(C)]
pub struct Nvos21 { pub hRoot: u32, pub hObjectParent: u32, pub hObjectNew: u32, pub hClass: u32, pub pAllocParms: u64, pub paramsSize: u32, pub status: u32 }

#[repr(C)]
#[derive(Default)]
pub struct Nv0080AllocParameters {
    pub deviceId: u32,
    pub hClientShare: Handle,
    pub hTargetClient: Handle,
    pub hTargetDevice: Handle,
    pub flags: u32,
    pub vaSpaceSize: u64,
    pub vaStartInternal: u64,
    pub vaLimitInternal: u64,
    pub vaMode: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct Nv2080AllocParameters {
    pub subDeviceId: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct NvVaspaceAllocationParameters {
    pub index: u32,
    pub flags: u32,
    pub vaSize: u64,
    pub vaStartInternal: u64,
    pub vaLimitInternal: u64,
    pub bigPageSize: u32,
    pub vaBase: u64,
}

#[repr(C)]
#[derive(Default)]
pub struct NvMemoryAllocationParams {
    pub owner: u32,
    pub r#type: u32,
    pub flags: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: i32,
    pub attr: u32,
    pub attr2: u32,
    pub format: u32,
    pub comprCovg: u32,
    pub zcullCovg: u32,
    pub rangeLo: u64,
    pub rangeHi: u64,
    pub size: u64,
    pub alignment: u64,
    pub offset: u64,
    pub limit: u64,
    pub address: u64,
    pub ctagOffset: u32,
    pub hVASpace: Handle,
    pub internalflags: u32,
    pub tag: u32,
    pub numaNode: i32,
}

#[repr(C)]
#[derive(Default)]
pub struct NvChannelGroupAllocationParameters {
    pub hObjectError: Handle,
    pub hObjectEccError: Handle,
    pub hVASpace: Handle,
    pub engineType: u32,
    pub bIsCallingContextVgpuPlugin: u8,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct NvMemoryDescParams {
    pub base: u64,
    pub size: u64,
    pub addressSpace: u32,
    pub cacheAttrib: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct NvChannelAllocParams {
    pub hObjectError: Handle,
    pub hObjectBuffer: Handle,
    pub gpFifoOffset: u64,
    pub gpFifoEntries: u32,
    pub flags: u32,
    pub hContextShare: Handle,
    pub hVASpace: Handle,
    pub hUserdMemory: [Handle; NV_MAX_SUBDEVICES],
    pub userdOffset: [u64; NV_MAX_SUBDEVICES],
    pub engineType: u32,
    pub cid: u32,
    pub subDeviceId: u32,
    pub hObjectEccError: Handle,
    pub instanceMem: NvMemoryDescParams,
    pub userdMem: NvMemoryDescParams,
    pub ramfcMem: NvMemoryDescParams,
    pub mthdbufMem: NvMemoryDescParams,
    pub hPhysChannelGroup: Handle,
    pub internalFlags: u32,
    pub errorNotifierMem: NvMemoryDescParams,
    pub eccErrorNotifierMem: NvMemoryDescParams,
    pub ProcessID: u32,
    pub SubProcessID: u32,
    pub encryptIv: [u32; CC_CHAN_ALLOC_IV_SIZE_DWORD],
    pub decryptIv: [u32; CC_CHAN_ALLOC_IV_SIZE_DWORD],
    pub hmacNonce: [u32; CC_CHAN_ALLOC_NONCE_SIZE_DWORD],
    pub tpcConfigID: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct Nvb0b5AllocationParameters {
    pub version: u32,
    pub engineType: u32,
}

#[repr(C)]
pub struct Nv0000CtrlSystemGetBuildVersionV2Params {
    pub driverVersionBuffer: [u8; NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_V2_MAX_STRING_SIZE],
    pub versionBuffer: [u8; NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_V2_MAX_STRING_SIZE],
    pub driverBranch: [u8; NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_V2_MAX_STRING_SIZE],
    pub titleBuffer: [u8; NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_V2_MAX_STRING_SIZE],
    pub changelistNumber: u32,
    pub officialChangelistNumber: u32,
}

#[repr(C)]
pub struct Nv0000CtrlGpuGetAttachedIdsParams {
    pub gpuIds: [u32; NV0000_CTRL_GPU_MAX_ATTACHED_GPUS],
}

#[repr(C)]
#[derive(Default)]
pub struct Nv0000CtrlGpuGetIdInfoV2Params {
    pub gpuId: u32,
    pub gpuFlags: u32,
    pub deviceInstance: u32,
    pub subDeviceInstance: u32,
    pub sliStatus: u32,
    pub boardId: u32,
    pub gpuInstance: u32,
    pub numaId: i32,
}

#[repr(C)]
#[derive(Default)]
pub struct Nv0041CtrlGetSurfacePhysAttrParams {
    pub memOffset: u64,
    pub memFormat: u32,
    pub comprOffset: u32,
    pub comprFormat: u32,
    pub memAperture: u32,
    pub gpuCacheAttr: u32,
    pub gpuP2PCacheAttr: u32,
    pub mmuContext: u32,
    pub contigSegmentSize: u64,
}

#[repr(C)]
pub struct Nv0080CtrlGpuGetClasslistV2Params {
    pub numClasses: u32,
    pub classList: [u32; NV0080_CTRL_GPU_CLASSLIST_MAX_SIZE],
}

#[repr(C)]
pub struct Nv2080CtrlGpuGetNameStringParams {
    pub gpuNameStringFlags: u32,
    /// union { ascii[64]; unicode[64] (u16) }: the larger member.
    pub gpuNameString: [u8; 2 * NV2080_GPU_MAX_NAME_STRING_LENGTH],
}

#[repr(C)]
pub struct Nv2080CtrlGpuGetEnginesV2Params {
    pub engineCount: u32,
    pub engineList: [u32; NV2080_GPU_MAX_ENGINES_LIST_SIZE],
}

/// NVXXXX_CTRL_XXX_INFO.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct CtrlInfo {
    pub index: u32,
    pub data: u32,
}

#[repr(C)]
pub struct Nv2080CtrlFbGetInfoV2Params {
    pub fbInfoListSize: u32,
    pub fbInfoList: [CtrlInfo; NV2080_CTRL_FB_INFO_MAX_LIST_SIZE],
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct GrRouteInfo {
    pub flags: u32,
    pub route: u64,
}

#[repr(C)]
pub struct Nv2080CtrlGrGetInfoV2Params {
    pub grInfoListSize: u32,
    pub grInfoList: [CtrlInfo; NV2080_CTRL_GR_INFO_MAX_SIZE],
    pub grRouteInfo: GrRouteInfo,
}

#[repr(C)]
#[derive(Default)]
pub struct Nv2080CtrlGrGetGpcMaskParams {
    pub grRouteInfo: GrRouteInfo,
    pub gpcMask: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct Nv2080CtrlGrGetTpcMaskParams {
    pub grRouteInfo: GrRouteInfo,
    pub gpcId: u32,
    pub tpcMask: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct Nv2080CtrlCeGetCapsV2Params {
    pub ceEngineType: u32,
    pub capsTbl: [u8; NV2080_CTRL_CE_CAPS_TBL_SIZE],
}

#[repr(C)]
#[derive(Default)]
pub struct Nva06fCtrlGpfifoScheduleParams {
    pub bEnable: u8,
    pub bSkipSubmit: u8,
}

#[repr(C)]
#[derive(Default)]
pub struct Nvc36fCtrlGetWorkSubmitTokenParams {
    pub workSubmitToken: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct NvContextDmaAllocationParams {
    pub hSubDevice: Handle,
    pub flags: u32,
    pub hMemory: Handle,
    pub offset: u64,
    pub limit: u64,
}

#[repr(C)]
#[derive(Default)]
pub struct Nva06fCtrlBindParams {
    pub engineType: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct Nv2080CtrlMcGetArchInfoParams {
    pub architecture: u32,
    pub implementation: u32,
    pub revision: u32,
    pub subRevision: u8,
}

#[repr(C)]
#[derive(Default)]
pub struct Nv2080CtrlTimerGetTimeParams {
    pub time_nsec: u64,
}

#[repr(C)]
pub struct Nv2080CtrlGpuGetShortNameStringParams {
    pub gpuShortNameString: [u8; NV2080_GPU_MAX_NAME_STRING_LENGTH],
}

/// NvNotification (nvgputypes.h): 16 bytes.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct NvNotification {
    pub timeStamp: [u32; 2],
    pub info32: u32,
    pub info16: u16,
    pub status: u16,
}

/// A parameter block RM may read: zero-initialised by default.
pub fn zeroed<T>() -> alloc::boxed::Box<T> {
    // SAFETY: every type here is plain old data; all-zero is valid.
    unsafe { alloc::boxed::Box::<T>::new_zeroed().assume_init() }
}

// ---------------------------------------------------------- ABI table --

macro_rules! k { ($n:ident) => { (stringify!($n), $n as u64) }; }
macro_rules! sz { ($c:literal, $t:ty) => { (concat!("sizeof ", $c), size_of::<$t>() as u64) }; }
macro_rules! off { ($c:literal, $f:literal, $t:ty, $fld:ident) => {
    (concat!("offsetof ", $c, ".", $f), offset_of!($t, $fld) as u64)
}; }

/// Our side of `lnv_abi_table`: (C name, value). `client::abi_check`
/// requires every entry to be present in the C table with the same value.
pub static ABI: &[(&str, u64)] = &[
    ("NV01_FREE", 0x00), ("NV04_ALLOC", 0x15), ("NV04_CONTROL", 0x36), ("NV04_MAP_MEMORY", 0x21),
    ("NV04_UNMAP_MEMORY", 0x22), ("NV04_MAP_MEMORY_DMA", 0x2e), ("NV04_UNMAP_MEMORY_DMA", 0x2f),
    k!(NV01_ROOT), k!(NV01_DEVICE_0), k!(NV20_SUBDEVICE_0), k!(NV01_MEMORY_SYSTEM),
    k!(NV01_MEMORY_LOCAL_USER), k!(NV01_MEMORY_FRAMEBUFFER_CONSOLE), k!(NV50_MEMORY_VIRTUAL),
    k!(FERMI_VASPACE_A), k!(KEPLER_CHANNEL_GROUP_A), k!(AMPERE_CHANNEL_GPFIFO_A),
    k!(AMPERE_DMA_COPY_A), k!(AMPERE_DMA_COPY_B), k!(VOLTA_USERMODE_A),
    k!(NV0000_CTRL_CMD_SYSTEM_GET_BUILD_VERSION_V2), k!(NV0000_CTRL_CMD_GPU_GET_ATTACHED_IDS),
    k!(NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2), k!(NV0041_CTRL_CMD_GET_SURFACE_PHYS_ATTR),
    k!(NV0080_CTRL_CMD_GPU_GET_CLASSLIST_V2), k!(NV2080_CTRL_CMD_GPU_GET_NAME_STRING),
    k!(NV2080_CTRL_CMD_GPU_GET_ENGINES_V2), k!(NV2080_CTRL_CMD_FB_GET_INFO_V2),
    k!(NV2080_CTRL_CMD_GR_GET_INFO_V2), k!(NV2080_CTRL_CMD_GR_GET_GPC_MASK),
    k!(NV2080_CTRL_CMD_GR_GET_TPC_MASK), k!(NV2080_CTRL_CMD_CE_GET_CAPS_V2),
    k!(NVA06C_CTRL_CMD_GPFIFO_SCHEDULE), k!(NVA06F_CTRL_CMD_GPFIFO_SCHEDULE),
    k!(NVC36F_CTRL_CMD_GPFIFO_GET_WORK_SUBMIT_TOKEN),
    k!(NVA06F_CTRL_CMD_BIND), k!(NV2080_CTRL_CMD_MC_GET_ARCH_INFO), k!(NV2080_CTRL_CMD_TIMER_GET_TIME),
    k!(NV2080_CTRL_CMD_GPU_GET_SHORT_NAME_STRING),
    k!(NV01_CONTEXT_DMA), k!(FERMI_TWOD_A), k!(KEPLER_INLINE_TO_MEMORY_B), k!(AMPERE_A), k!(AMPERE_B),
    k!(AMPERE_COMPUTE_A), k!(AMPERE_COMPUTE_B),
    k!(NVOS32_ALLOC_FLAGS_FIXED_ADDRESS_ALLOCATE), k!(NVOS32_ALLOC_FLAGS_USE_BEGIN_END), k!(NVOS32_ALLOC_FLAGS_SPARSE),
    k!(NVOS32_TYPE_DEPTH), k!(NVOS32_TYPE_STENCIL),
    k!(OS32_ATTR_DEPTH_8), k!(OS32_ATTR_DEPTH_16), k!(OS32_ATTR_DEPTH_32), k!(OS32_ATTR_DEPTH_64),
    k!(OS32_ATTR_COMPR_NONE), k!(OS32_ATTR_FORMAT_PITCH), k!(OS32_ATTR_FORMAT_BLOCK_LINEAR),
    k!(OS32_ATTR_Z_TYPE_FIXED), k!(OS32_ATTR_Z_TYPE_FLOAT),
    k!(OS32_ATTR_ZS_PACKING_S8), k!(OS32_ATTR_ZS_PACKING_Z24S8), k!(OS32_ATTR_ZS_PACKING_S8Z24),
    k!(OS32_ATTR_ZS_PACKING_Z32_X24S8), k!(OS32_ATTR_ZS_PACKING_Z16),
    k!(OS46_FLAGS_PAGE_KIND_VIRTUAL), k!(OS46_FLAGS_CACHE_SNOOP_DISABLE), k!(OS46_FLAGS_DMA_OFFSET_FIXED_TRUE),
    k!(OS03_FLAGS_MAPPING_KERNEL), k!(OS03_FLAGS_HASH_TABLE_DISABLE),
    k!(NV_CHANNELGPFIFO_NOTIFICATION_TYPE_ERROR),
    sz!("NV_CONTEXT_DMA_ALLOCATION_PARAMS", NvContextDmaAllocationParams),
    off!("NV_CONTEXT_DMA_ALLOCATION_PARAMS", "hMemory", NvContextDmaAllocationParams, hMemory),
    off!("NV_CONTEXT_DMA_ALLOCATION_PARAMS", "offset", NvContextDmaAllocationParams, offset),
    off!("NV_CONTEXT_DMA_ALLOCATION_PARAMS", "limit", NvContextDmaAllocationParams, limit),
    sz!("NVA06F_CTRL_BIND_PARAMS", Nva06fCtrlBindParams),
    sz!("NV2080_CTRL_MC_GET_ARCH_INFO_PARAMS", Nv2080CtrlMcGetArchInfoParams),
    sz!("NV2080_CTRL_TIMER_GET_TIME_PARAMS", Nv2080CtrlTimerGetTimeParams),
    sz!("NV2080_CTRL_GPU_GET_SHORT_NAME_STRING_PARAMS", Nv2080CtrlGpuGetShortNameStringParams),
    sz!("NvNotification", NvNotification),
    off!("NvNotification", "info32", NvNotification, info32),
    off!("NvNotification", "status", NvNotification, status),
    off!("NV_MEMORY_ALLOCATION_PARAMS", "format", NvMemoryAllocationParams, format),
    off!("NV_MEMORY_ALLOCATION_PARAMS", "type", NvMemoryAllocationParams, r#type),
    k!(NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_V2_MAX_STRING_SIZE), k!(NV0000_CTRL_GPU_MAX_ATTACHED_GPUS),
    k!(NV0000_CTRL_GPU_INVALID_ID), k!(NV0080_CTRL_GPU_CLASSLIST_MAX_SIZE),
    k!(NV2080_GPU_MAX_NAME_STRING_LENGTH), k!(NV2080_GPU_MAX_ENGINES_LIST_SIZE),
    k!(NV2080_CTRL_FB_INFO_MAX_LIST_SIZE), k!(NV2080_CTRL_GR_INFO_MAX_SIZE),
    k!(NV2080_CTRL_CE_CAPS_TBL_SIZE), k!(NV_MAX_SUBDEVICES), k!(CC_CHAN_ALLOC_IV_SIZE_DWORD),
    k!(CC_CHAN_ALLOC_NONCE_SIZE_DWORD),
    k!(NV2080_CTRL_FB_INFO_INDEX_RAM_SIZE), k!(NV2080_CTRL_FB_INFO_INDEX_TOTAL_RAM_SIZE),
    k!(NV2080_CTRL_FB_INFO_INDEX_HEAP_SIZE), k!(NV2080_CTRL_FB_INFO_INDEX_HEAP_FREE),
    k!(NV2080_CTRL_FB_INFO_INDEX_BAR1_SIZE), k!(NV2080_CTRL_FB_INFO_INDEX_BUS_WIDTH),
    k!(NV2080_CTRL_FB_INFO_INDEX_RAM_TYPE), k!(NV2080_CTRL_FB_INFO_INDEX_L2CACHE_SIZE),
    k!(NV2080_CTRL_FB_INFO_INDEX_FBP_COUNT), k!(NV2080_CTRL_FB_INFO_INDEX_LTC_COUNT),
    k!(NV2080_CTRL_GR_INFO_INDEX_SHADER_PIPE_COUNT), k!(NV2080_CTRL_GR_INFO_INDEX_SM_VERSION),
    k!(NV2080_CTRL_GR_INFO_INDEX_LITTER_NUM_GPCS), k!(NV2080_CTRL_GR_INFO_INDEX_LITTER_NUM_TPC_PER_GPC),
    k!(NV2080_CTRL_GR_INFO_INDEX_LITTER_NUM_SM_PER_TPC), k!(NV2080_CTRL_GR_INFO_INDEX_GPU_CORE_COUNT),
    k!(NV2080_CTRL_GR_INFO_INDEX_MAX_WARPS_PER_SM),
    k!(NV2080_ENGINE_TYPE_GRAPHICS), k!(NV2080_ENGINE_TYPE_COPY0), k!(NV2080_ENGINE_TYPE_COPY9),
    k!(NV2080_ENGINE_TYPE_COPY10), k!(NVA06F_SUBCHANNEL_COPY_ENGINE),
    k!(NVB0B5_ALLOCATION_PARAMETERS_VERSION_1), k!(NV_VASPACE_ALLOCATION_INDEX_GPU_NEW),
    k!(HEAP_OWNER_RM_KERNEL_CLIENT),
    k!(NVOS32_TYPE_IMAGE),
    k!(OS32_ATTR_LOCATION_VIDMEM), k!(OS32_ATTR_LOCATION_PCI),
    k!(OS32_ATTR_COHERENCY_CACHED), k!(OS32_ATTR_COHERENCY_UNCACHED),
    k!(OS32_ATTR_PHYSICALITY_CONTIGUOUS), k!(OS32_ATTR_PHYSICALITY_NONCONTIGUOUS),
    k!(OS32_ATTR_PAGE_SIZE_4KB), k!(OS32_ATTR_PAGE_SIZE_BIG), k!(OS32_ATTR_PAGE_SIZE_DEFAULT),
    k!(NVOS32_ALLOC_FLAGS_VIRTUAL), k!(NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE),
    k!(NVOS32_ALLOC_FLAGS_ALLOCATE_KERNEL_PRIVILEGED), k!(NVOS32_ALLOC_FLAGS_PERSISTENT_VIDMEM),
    k!(OS33_FLAGS_ACCESS_READ_WRITE), k!(OS33_FLAGS_ACCESS_WRITE_ONLY),
    k!(OS46_FLAGS_CACHE_SNOOP_ENABLE), k!(OS46_FLAGS_PAGE_SIZE_4KB), k!(OS46_FLAGS_PAGE_SIZE_DEFAULT),
    k!(NV0041_CTRL_CMD_GET_SURFACE_PHYS_ATTR_APERTURE_VIDMEM), k!(NV0041_CTRL_CMD_GET_SURFACE_PHYS_ATTR_APERTURE_SYSMEM),
    k!(NVC361_NOTIFY_CHANNEL_PENDING), k!(NVC361_NV_USERMODE__SIZE), k!(NVC56F_GP_ENTRY__SIZE),
    sz!("NVOS21_PARAMETERS", Nvos21),
    sz!("NV0080_ALLOC_PARAMETERS", Nv0080AllocParameters),
    off!("NV0080_ALLOC_PARAMETERS", "hClientShare", Nv0080AllocParameters, hClientShare),
    off!("NV0080_ALLOC_PARAMETERS", "vaSpaceSize", Nv0080AllocParameters, vaSpaceSize),
    off!("NV0080_ALLOC_PARAMETERS", "vaMode", Nv0080AllocParameters, vaMode),
    sz!("NV2080_ALLOC_PARAMETERS", Nv2080AllocParameters),
    sz!("NV_VASPACE_ALLOCATION_PARAMETERS", NvVaspaceAllocationParameters),
    off!("NV_VASPACE_ALLOCATION_PARAMETERS", "vaSize", NvVaspaceAllocationParameters, vaSize),
    off!("NV_VASPACE_ALLOCATION_PARAMETERS", "bigPageSize", NvVaspaceAllocationParameters, bigPageSize),
    off!("NV_VASPACE_ALLOCATION_PARAMETERS", "vaBase", NvVaspaceAllocationParameters, vaBase),
    sz!("NV_MEMORY_ALLOCATION_PARAMS", NvMemoryAllocationParams),
    off!("NV_MEMORY_ALLOCATION_PARAMS", "attr", NvMemoryAllocationParams, attr),
    off!("NV_MEMORY_ALLOCATION_PARAMS", "attr2", NvMemoryAllocationParams, attr2),
    off!("NV_MEMORY_ALLOCATION_PARAMS", "rangeLo", NvMemoryAllocationParams, rangeLo),
    off!("NV_MEMORY_ALLOCATION_PARAMS", "size", NvMemoryAllocationParams, size),
    off!("NV_MEMORY_ALLOCATION_PARAMS", "alignment", NvMemoryAllocationParams, alignment),
    off!("NV_MEMORY_ALLOCATION_PARAMS", "offset", NvMemoryAllocationParams, offset),
    off!("NV_MEMORY_ALLOCATION_PARAMS", "limit", NvMemoryAllocationParams, limit),
    off!("NV_MEMORY_ALLOCATION_PARAMS", "address", NvMemoryAllocationParams, address),
    off!("NV_MEMORY_ALLOCATION_PARAMS", "ctagOffset", NvMemoryAllocationParams, ctagOffset),
    off!("NV_MEMORY_ALLOCATION_PARAMS", "hVASpace", NvMemoryAllocationParams, hVASpace),
    off!("NV_MEMORY_ALLOCATION_PARAMS", "numaNode", NvMemoryAllocationParams, numaNode),
    sz!("NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS", NvChannelGroupAllocationParameters),
    off!("NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS", "engineType", NvChannelGroupAllocationParameters, engineType),
    off!("NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS", "bIsCallingContextVgpuPlugin", NvChannelGroupAllocationParameters, bIsCallingContextVgpuPlugin),
    sz!("NV_MEMORY_DESC_PARAMS", NvMemoryDescParams),
    sz!("NV_CHANNEL_ALLOC_PARAMS", NvChannelAllocParams),
    off!("NV_CHANNEL_ALLOC_PARAMS", "gpFifoOffset", NvChannelAllocParams, gpFifoOffset),
    off!("NV_CHANNEL_ALLOC_PARAMS", "gpFifoEntries", NvChannelAllocParams, gpFifoEntries),
    off!("NV_CHANNEL_ALLOC_PARAMS", "hVASpace", NvChannelAllocParams, hVASpace),
    off!("NV_CHANNEL_ALLOC_PARAMS", "hUserdMemory", NvChannelAllocParams, hUserdMemory),
    off!("NV_CHANNEL_ALLOC_PARAMS", "userdOffset", NvChannelAllocParams, userdOffset),
    off!("NV_CHANNEL_ALLOC_PARAMS", "engineType", NvChannelAllocParams, engineType),
    off!("NV_CHANNEL_ALLOC_PARAMS", "hObjectEccError", NvChannelAllocParams, hObjectEccError),
    off!("NV_CHANNEL_ALLOC_PARAMS", "instanceMem", NvChannelAllocParams, instanceMem),
    off!("NV_CHANNEL_ALLOC_PARAMS", "hPhysChannelGroup", NvChannelAllocParams, hPhysChannelGroup),
    off!("NV_CHANNEL_ALLOC_PARAMS", "errorNotifierMem", NvChannelAllocParams, errorNotifierMem),
    off!("NV_CHANNEL_ALLOC_PARAMS", "ProcessID", NvChannelAllocParams, ProcessID),
    off!("NV_CHANNEL_ALLOC_PARAMS", "encryptIv", NvChannelAllocParams, encryptIv),
    off!("NV_CHANNEL_ALLOC_PARAMS", "hmacNonce", NvChannelAllocParams, hmacNonce),
    off!("NV_CHANNEL_ALLOC_PARAMS", "tpcConfigID", NvChannelAllocParams, tpcConfigID),
    sz!("NVB0B5_ALLOCATION_PARAMETERS", Nvb0b5AllocationParameters),
    sz!("NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_V2_PARAMS", Nv0000CtrlSystemGetBuildVersionV2Params),
    sz!("NV0000_CTRL_GPU_GET_ATTACHED_IDS_PARAMS", Nv0000CtrlGpuGetAttachedIdsParams),
    sz!("NV0000_CTRL_GPU_GET_ID_INFO_V2_PARAMS", Nv0000CtrlGpuGetIdInfoV2Params),
    sz!("NV0041_CTRL_GET_SURFACE_PHYS_ATTR_PARAMS", Nv0041CtrlGetSurfacePhysAttrParams),
    off!("NV0041_CTRL_GET_SURFACE_PHYS_ATTR_PARAMS", "memAperture", Nv0041CtrlGetSurfacePhysAttrParams, memAperture),
    off!("NV0041_CTRL_GET_SURFACE_PHYS_ATTR_PARAMS", "contigSegmentSize", Nv0041CtrlGetSurfacePhysAttrParams, contigSegmentSize),
    sz!("NV0080_CTRL_GPU_GET_CLASSLIST_V2_PARAMS", Nv0080CtrlGpuGetClasslistV2Params),
    sz!("NV2080_CTRL_GPU_GET_NAME_STRING_PARAMS", Nv2080CtrlGpuGetNameStringParams),
    sz!("NV2080_CTRL_GPU_GET_ENGINES_V2_PARAMS", Nv2080CtrlGpuGetEnginesV2Params),
    sz!("NV2080_CTRL_FB_GET_INFO_V2_PARAMS", Nv2080CtrlFbGetInfoV2Params),
    sz!("NV2080_CTRL_GR_GET_INFO_V2_PARAMS", Nv2080CtrlGrGetInfoV2Params),
    off!("NV2080_CTRL_GR_GET_INFO_V2_PARAMS", "grRouteInfo", Nv2080CtrlGrGetInfoV2Params, grRouteInfo),
    sz!("NV2080_CTRL_GR_GET_GPC_MASK_PARAMS", Nv2080CtrlGrGetGpcMaskParams),
    off!("NV2080_CTRL_GR_GET_GPC_MASK_PARAMS", "gpcMask", Nv2080CtrlGrGetGpcMaskParams, gpcMask),
    sz!("NV2080_CTRL_GR_GET_TPC_MASK_PARAMS", Nv2080CtrlGrGetTpcMaskParams),
    off!("NV2080_CTRL_GR_GET_TPC_MASK_PARAMS", "tpcMask", Nv2080CtrlGrGetTpcMaskParams, tpcMask),
    sz!("NV2080_CTRL_CE_GET_CAPS_V2_PARAMS", Nv2080CtrlCeGetCapsV2Params),
    sz!("NVA06F_CTRL_GPFIFO_SCHEDULE_PARAMS", Nva06fCtrlGpfifoScheduleParams),
    sz!("NVC36F_CTRL_CMD_GPFIFO_GET_WORK_SUBMIT_TOKEN_PARAMS", Nvc36fCtrlGetWorkSubmitTokenParams),
    ("offsetof AmpereAControlGPFifo.GPGet", USERD_GP_GET as u64),
    ("offsetof AmpereAControlGPFifo.GPPut", USERD_GP_PUT as u64),
    ("offsetof AmpereAControlGPFifo.Get", USERD_GET as u64),
    ("offsetof AmpereAControlGPFifo.Put", USERD_PUT as u64),
];
