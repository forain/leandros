/*
 * SPDX-License-Identifier: MIT
 *
 * LeandrOS <-> NVIDIA OpenRM: the in-kernel RM client path.
 *
 * Compiled by ports/openrm/build.sh with the same flags as lnv_glue.c (the
 * flags NVIDIA's Makefile uses for osapi.c) and archived into libnvrm.a.
 *
 * Contents:
 *   1. RM API wrappers for kernel clients. They go through
 *      rm_kernel_rmapi_op(), the entry point Linux's nvidia-modeset (nvkms)
 *      uses: privilege level KERNEL, parameters in kernel memory, the RM API
 *      lock taken inside. drivers/nvidia/src/client.rs builds on these.
 *   2. The ABI table: names and values of every struct size, field offset,
 *      class, control command and flag the Rust side mirrors. client.rs
 *      compares its own constants against it before it talks to RM, so a
 *      transcription error shows up as one log line (in QEMU too) instead of
 *      a misread parameter block on the GPU.
 *   3. Push-buffer encoders for the copy-engine self-test (AMPERE host class
 *      method headers, CE memset/copy/semaphore methods, GPFIFO entries),
 *      written with NVIDIA's own DRF macros.
 *   4. nv_add_mapping_context_to_file: the user-mapping context RM records
 *      for a later mmap(), kept per fd until the UAPI layer asks for it.
 */

#include "nv.h"
#include "os-interface.h"
#include "nv-kernel-rmapi-ops.h"
#include "nvos.h"
#include "nvstatus.h"

#include "class/cl0000.h"   /* NV01_ROOT */
#include "class/cl0080.h"   /* NV01_DEVICE_0 */
#include "class/cl2080.h"   /* NV20_SUBDEVICE_0, NV2080_ENGINE_TYPE_* */
#include "class/cl003e.h"   /* NV01_MEMORY_SYSTEM */
#include "class/cl0040.h"   /* NV01_MEMORY_LOCAL_USER */
#include "class/cl0076.h"   /* NV01_MEMORY_FRAMEBUFFER_CONSOLE */
#include "class/cl50a0.h"   /* NV50_MEMORY_VIRTUAL */
#include "class/cl90f1.h"   /* FERMI_VASPACE_A */
#include "class/cla06c.h"   /* KEPLER_CHANNEL_GROUP_A */
#include "class/cla06fsubch.h"
#include "class/clc56f.h"   /* AMPERE_CHANNEL_GPFIFO_A */
#include "class/clc6b5.h"   /* AMPERE_DMA_COPY_A */
#include "class/clc7b5.h"   /* AMPERE_DMA_COPY_B */
#include "class/clc361.h"   /* VOLTA_USERMODE_A */
#include "class/clb0b5sw.h" /* NVB0B5_ALLOCATION_PARAMETERS */
#include "class/cl0002.h"   /* NV01_CONTEXT_DMA */
#include "class/cl902d.h"   /* FERMI_TWOD_A */
#include "class/cla140.h"   /* KEPLER_INLINE_TO_MEMORY_B */
#include "class/clc697.h"   /* AMPERE_A */
#include "class/clc797.h"   /* AMPERE_B */
#include "class/clc6c0.h"   /* AMPERE_COMPUTE_A */
#include "class/clc7c0.h"   /* AMPERE_COMPUTE_B */
#include "alloc/alloc_channel.h"

#include "ctrl/ctrl0000/ctrl0000system.h"
#include "ctrl/ctrl0000/ctrl0000gpu.h"
#include "ctrl/ctrl0041.h"
#include "ctrl/ctrl0080/ctrl0080gpu.h"
#include "ctrl/ctrl2080/ctrl2080gpu.h"
#include "ctrl/ctrl2080/ctrl2080fb.h"
#include "ctrl/ctrl2080/ctrl2080gr.h"
#include "ctrl/ctrl2080/ctrl2080ce.h"
#include "ctrl/ctrla06c.h"
#include "ctrl/ctrla06f/ctrla06fgpfifo.h"
#include "ctrl/ctrlc36f.h"
#include "ctrl/ctrl2080/ctrl2080mc.h"
#include "ctrl/ctrl2080/ctrl2080tmr.h"
#include "nvgputypes.h"

#include "lnv_glue.h"
#include "lnv_rmapi.h"

/* g_heap_nvoc.h (internal): HEAP_OWNER_RM_SCRATCH_END + 1. */
#ifndef HEAP_OWNER_RM_KERNEL_CLIENT
#define HEAP_OWNER_RM_KERNEL_CLIENT 0xDEAF000AU
#endif

extern const char *nvstatusToString(NV_STATUS nvStatusIn);

/* ---------------------------------------------------------------------- */
/* 1. Kernel-client RM API                                                 */
/* ---------------------------------------------------------------------- */

static void lnv_ops_zero(nvidia_kernel_rmapi_ops_t *ops, NvU32 op)
{
    os_mem_set(ops, 0, sizeof(*ops));
    ops->op = op;
}

/*
 * NV04_ALLOC. *phObject == 0 lets RM pick the handle (written back). For
 * NV01_ROOT, hClient and hParent are 0 and pParams points at an NvHandle
 * (0: RM picks the client handle), as nvkms and UVM do.
 */
NvU32 lnv_rm_alloc(NvHandle hClient, NvHandle hParent, NvHandle *phObject,
                   NvU32 hClass, void *pParams, NvU32 paramsSize)
{
    nvidia_kernel_rmapi_ops_t ops;

    lnv_ops_zero(&ops, NV04_ALLOC);
    ops.params.alloc.hRoot         = hClient;
    ops.params.alloc.hObjectParent = hParent;
    ops.params.alloc.hObjectNew    = *phObject;
    ops.params.alloc.hClass        = hClass;
    ops.params.alloc.pAllocParms   = NV_PTR_TO_NvP64(pParams);
    ops.params.alloc.paramsSize    = paramsSize;
    rm_kernel_rmapi_op(NULL, &ops);
    if (ops.params.alloc.status == NV_OK)
        *phObject = ops.params.alloc.hObjectNew;
    return ops.params.alloc.status;
}

/* NV01_FREE. Freeing the client handle frees every object under it. */
NvU32 lnv_rm_free(NvHandle hClient, NvHandle hParent, NvHandle hObject)
{
    nvidia_kernel_rmapi_ops_t ops;

    lnv_ops_zero(&ops, NV01_FREE);
    ops.params.free.hRoot         = hClient;
    ops.params.free.hObjectParent = hParent;
    ops.params.free.hObjectOld    = hObject;
    rm_kernel_rmapi_op(NULL, &ops);
    return ops.params.free.status;
}

/* NV04_CONTROL. */
NvU32 lnv_rm_control(NvHandle hClient, NvHandle hObject, NvU32 cmd,
                     void *pParams, NvU32 paramsSize)
{
    nvidia_kernel_rmapi_ops_t ops;

    lnv_ops_zero(&ops, NV04_CONTROL);
    ops.params.control.hClient    = hClient;
    ops.params.control.hObject    = hObject;
    ops.params.control.cmd        = cmd;
    ops.params.control.params     = NV_PTR_TO_NvP64(pParams);
    ops.params.control.paramsSize = paramsSize;
    rm_kernel_rmapi_op(NULL, &ops);
    return ops.params.control.status;
}

/*
 * NV04_MAP_MEMORY: a CPU mapping. For a kernel client the result is a kernel
 * virtual address (sysmem: nv_alloc_kernel_mapping; vidmem: a BAR1 window
 * through os_map_kernel_space; a USERMODE object: its BAR0 page).
 */
NvU32 lnv_rm_map_memory(NvHandle hClient, NvHandle hDevice, NvHandle hMemory,
                        NvU64 offset, NvU64 length, void **ppAddress, NvU32 flags)
{
    nvidia_kernel_rmapi_ops_t ops;

    lnv_ops_zero(&ops, NV04_MAP_MEMORY);
    ops.params.mapMemory.hClient = hClient;
    ops.params.mapMemory.hDevice = hDevice;
    ops.params.mapMemory.hMemory = hMemory;
    ops.params.mapMemory.offset  = offset;
    ops.params.mapMemory.length  = length;
    ops.params.mapMemory.flags   = flags;
    rm_kernel_rmapi_op(NULL, &ops);
    *ppAddress = NvP64_VALUE(ops.params.mapMemory.pLinearAddress);
    return ops.params.mapMemory.status;
}

NvU32 lnv_rm_unmap_memory(NvHandle hClient, NvHandle hDevice, NvHandle hMemory,
                          void *pAddress, NvU32 flags)
{
    nvidia_kernel_rmapi_ops_t ops;

    lnv_ops_zero(&ops, NV04_UNMAP_MEMORY);
    ops.params.unmapMemory.hClient        = hClient;
    ops.params.unmapMemory.hDevice        = hDevice;
    ops.params.unmapMemory.hMemory        = hMemory;
    ops.params.unmapMemory.pLinearAddress = NV_PTR_TO_NvP64(pAddress);
    ops.params.unmapMemory.flags          = flags;
    rm_kernel_rmapi_op(NULL, &ops);
    return ops.params.unmapMemory.status;
}

/*
 * NV04_MAP_MEMORY_DMA: map hMemory into a GPU virtual address space. With
 * hDma an NV50_MEMORY_VIRTUAL range, *pDmaOffset is the offset inside that
 * range on input and the absolute GPU VA on output.
 */
NvU32 lnv_rm_map_memory_dma(NvHandle hClient, NvHandle hDevice, NvHandle hDma,
                            NvHandle hMemory, NvU64 offset, NvU64 length,
                            NvU32 flags, NvU64 *pDmaOffset)
{
    nvidia_kernel_rmapi_ops_t ops;

    lnv_ops_zero(&ops, NV04_MAP_MEMORY_DMA);
    ops.params.mapMemoryDma.hClient   = hClient;
    ops.params.mapMemoryDma.hDevice   = hDevice;
    ops.params.mapMemoryDma.hDma      = hDma;
    ops.params.mapMemoryDma.hMemory   = hMemory;
    ops.params.mapMemoryDma.offset    = offset;
    ops.params.mapMemoryDma.length    = length;
    ops.params.mapMemoryDma.flags     = flags;
    ops.params.mapMemoryDma.dmaOffset = *pDmaOffset;
    rm_kernel_rmapi_op(NULL, &ops);
    *pDmaOffset = ops.params.mapMemoryDma.dmaOffset;
    return ops.params.mapMemoryDma.status;
}

NvU32 lnv_rm_unmap_memory_dma(NvHandle hClient, NvHandle hDevice, NvHandle hDma,
                              NvHandle hMemory, NvU32 flags, NvU64 dmaOffset)
{
    nvidia_kernel_rmapi_ops_t ops;

    lnv_ops_zero(&ops, NV04_UNMAP_MEMORY_DMA);
    ops.params.unmapMemoryDma.hClient   = hClient;
    ops.params.unmapMemoryDma.hDevice   = hDevice;
    ops.params.unmapMemoryDma.hDma      = hDma;
    ops.params.unmapMemoryDma.hMemory   = hMemory;
    ops.params.unmapMemoryDma.flags     = flags;
    ops.params.unmapMemoryDma.dmaOffset = dmaOffset;
    rm_kernel_rmapi_op(NULL, &ops);
    return ops.params.unmapMemoryDma.status;
}

/* NV04_DUP_OBJECT (another client's object into ours; for the UAPI layer). */
NvU32 lnv_rm_dup_object(NvHandle hClient, NvHandle hParent, NvHandle *phObject,
                        NvHandle hClientSrc, NvHandle hObjectSrc, NvU32 flags)
{
    nvidia_kernel_rmapi_ops_t ops;

    lnv_ops_zero(&ops, NV04_DUP_OBJECT);
    ops.params.dupObject.hClient    = hClient;
    ops.params.dupObject.hParent    = hParent;
    ops.params.dupObject.hObject    = *phObject;
    ops.params.dupObject.hClientSrc = hClientSrc;
    ops.params.dupObject.hObjectSrc = hObjectSrc;
    ops.params.dupObject.flags      = flags;
    rm_kernel_rmapi_op(NULL, &ops);
    if (ops.params.dupObject.status == NV_OK)
        *phObject = ops.params.dupObject.hObject;
    return ops.params.dupObject.status;
}

const char *lnv_status_str(NvU32 status)
{
    return nvstatusToString((NV_STATUS)status);
}

/* ---------------------------------------------------------------------- */
/* 2. ABI table                                                            */
/* ---------------------------------------------------------------------- */

typedef struct
{
    const char *name;
    NvU64       value;
} lnv_abi_entry_t;

#define K(x)       { #x, (NvU64)(x) }
#define SZ(t)      { "sizeof " #t, (NvU64)sizeof(t) }
#define OFF(t, f)  { "offsetof " #t "." #f, (NvU64)NV_OFFSETOF(t, f) }
#define DEF(d, r, f, c) { #d #r #f #c, (NvU64)DRF_DEF(d, r, f, c) }

static const lnv_abi_entry_t lnv_abi_table[] =
{
    /* RM API op codes */
    K(NV01_FREE), K(NV04_ALLOC), K(NV04_CONTROL), K(NV04_MAP_MEMORY),
    K(NV04_UNMAP_MEMORY), K(NV04_MAP_MEMORY_DMA), K(NV04_UNMAP_MEMORY_DMA),

    /* classes */
    K(NV01_ROOT), K(NV01_DEVICE_0), K(NV20_SUBDEVICE_0), K(NV01_MEMORY_SYSTEM),
    K(NV01_MEMORY_LOCAL_USER), K(NV01_MEMORY_FRAMEBUFFER_CONSOLE),
    K(NV50_MEMORY_VIRTUAL), K(FERMI_VASPACE_A), K(KEPLER_CHANNEL_GROUP_A),
    K(AMPERE_CHANNEL_GPFIFO_A), K(AMPERE_DMA_COPY_A), K(AMPERE_DMA_COPY_B),
    K(VOLTA_USERMODE_A),

    /* control commands */
    K(NV0000_CTRL_CMD_SYSTEM_GET_BUILD_VERSION_V2),
    K(NV0000_CTRL_CMD_GPU_GET_ATTACHED_IDS),
    K(NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2),
    K(NV0041_CTRL_CMD_GET_SURFACE_PHYS_ATTR),
    K(NV0080_CTRL_CMD_GPU_GET_CLASSLIST_V2),
    K(NV2080_CTRL_CMD_GPU_GET_NAME_STRING),
    K(NV2080_CTRL_CMD_GPU_GET_ENGINES_V2),
    K(NV2080_CTRL_CMD_FB_GET_INFO_V2),
    K(NV2080_CTRL_CMD_GR_GET_INFO_V2),
    K(NV2080_CTRL_CMD_GR_GET_GPC_MASK),
    K(NV2080_CTRL_CMD_GR_GET_TPC_MASK),
    K(NV2080_CTRL_CMD_CE_GET_CAPS_V2),
    K(NVA06C_CTRL_CMD_GPFIFO_SCHEDULE),
    K(NVA06F_CTRL_CMD_GPFIFO_SCHEDULE),
    K(NVC36F_CTRL_CMD_GPFIFO_GET_WORK_SUBMIT_TOKEN),
    K(NVC36F_CTRL_CMD_GPFIFO_SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX),
    K(NVA06F_CTRL_CMD_BIND), K(NV2080_CTRL_CMD_MC_GET_ARCH_INFO), K(NV2080_CTRL_CMD_TIMER_GET_TIME),
    K(NV2080_CTRL_CMD_GPU_GET_SHORT_NAME_STRING),

    /* nouveau-UAPI layer (uapi.rs): engine classes, context DMA, VA and
     * depth/stencil kind attributes, mapping flags */
    K(NV01_CONTEXT_DMA), K(FERMI_TWOD_A), K(KEPLER_INLINE_TO_MEMORY_B), K(AMPERE_A), K(AMPERE_B),
    K(AMPERE_COMPUTE_A), K(AMPERE_COMPUTE_B),
    K(NVOS32_ALLOC_FLAGS_FIXED_ADDRESS_ALLOCATE), K(NVOS32_ALLOC_FLAGS_USE_BEGIN_END),
    K(NVOS32_ALLOC_FLAGS_SPARSE), K(NVOS32_TYPE_DEPTH), K(NVOS32_TYPE_STENCIL),
    DEF(OS32, _ATTR, _DEPTH, _8), DEF(OS32, _ATTR, _DEPTH, _16), DEF(OS32, _ATTR, _DEPTH, _32),
    DEF(OS32, _ATTR, _DEPTH, _64), DEF(OS32, _ATTR, _COMPR, _NONE),
    DEF(OS32, _ATTR, _FORMAT, _PITCH), DEF(OS32, _ATTR, _FORMAT, _BLOCK_LINEAR),
    DEF(OS32, _ATTR, _Z_TYPE, _FIXED), DEF(OS32, _ATTR, _Z_TYPE, _FLOAT),
    DEF(OS32, _ATTR, _ZS_PACKING, _S8), DEF(OS32, _ATTR, _ZS_PACKING, _Z24S8),
    DEF(OS32, _ATTR, _ZS_PACKING, _S8Z24), DEF(OS32, _ATTR, _ZS_PACKING, _Z32_X24S8),
    DEF(OS32, _ATTR, _ZS_PACKING, _Z16),
    DEF(OS46, _FLAGS, _PAGE_KIND, _VIRTUAL), DEF(OS46, _FLAGS, _CACHE_SNOOP, _DISABLE),
    DEF(OS46, _FLAGS, _DMA_OFFSET_FIXED, _TRUE),
    DEF(OS03, _FLAGS, _MAPPING, _KERNEL), DEF(OS03, _FLAGS, _HASH_TABLE, _DISABLE),
    K(NV_CHANNELGPFIFO_NOTIFICATION_TYPE_ERROR),
    K(NV_CHANNELGPFIFO_NOTIFICATION_TYPE_WORK_SUBMIT_TOKEN),
    K(NV_CHANNELGPFIFO_NOTIFICATION_TYPE__SIZE_1),
    SZ(NV_CONTEXT_DMA_ALLOCATION_PARAMS), OFF(NV_CONTEXT_DMA_ALLOCATION_PARAMS, hMemory),
    OFF(NV_CONTEXT_DMA_ALLOCATION_PARAMS, offset), OFF(NV_CONTEXT_DMA_ALLOCATION_PARAMS, limit),
    SZ(NVA06F_CTRL_BIND_PARAMS), SZ(NV2080_CTRL_MC_GET_ARCH_INFO_PARAMS),
    SZ(NV2080_CTRL_TIMER_GET_TIME_PARAMS), SZ(NV2080_CTRL_GPU_GET_SHORT_NAME_STRING_PARAMS),
    SZ(NvNotification), OFF(NvNotification, info32), OFF(NvNotification, info16), OFF(NvNotification, status),
    OFF(NV_MEMORY_ALLOCATION_PARAMS, format), OFF(NV_MEMORY_ALLOCATION_PARAMS, type),

    /* array sizes and indices */
    K(NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_V2_MAX_STRING_SIZE),
    K(NV0000_CTRL_GPU_MAX_ATTACHED_GPUS), K(NV0000_CTRL_GPU_INVALID_ID),
    K(NV0080_CTRL_GPU_CLASSLIST_MAX_SIZE), K(NV2080_GPU_MAX_NAME_STRING_LENGTH),
    K(NV2080_GPU_MAX_ENGINES_LIST_SIZE), K(NV2080_CTRL_FB_INFO_MAX_LIST_SIZE),
    K(NV2080_CTRL_GR_INFO_MAX_SIZE), K(NV2080_CTRL_CE_CAPS_TBL_SIZE),
    K(NV_MAX_SUBDEVICES), K(CC_CHAN_ALLOC_IV_SIZE_DWORD), K(CC_CHAN_ALLOC_NONCE_SIZE_DWORD),
    K(NV2080_CTRL_FB_INFO_INDEX_RAM_SIZE), K(NV2080_CTRL_FB_INFO_INDEX_TOTAL_RAM_SIZE),
    K(NV2080_CTRL_FB_INFO_INDEX_HEAP_SIZE), K(NV2080_CTRL_FB_INFO_INDEX_HEAP_FREE),
    K(NV2080_CTRL_FB_INFO_INDEX_BAR1_SIZE), K(NV2080_CTRL_FB_INFO_INDEX_BUS_WIDTH),
    K(NV2080_CTRL_FB_INFO_INDEX_RAM_TYPE), K(NV2080_CTRL_FB_INFO_INDEX_L2CACHE_SIZE),
    K(NV2080_CTRL_FB_INFO_INDEX_FBP_COUNT), K(NV2080_CTRL_FB_INFO_INDEX_LTC_COUNT),
    K(NV2080_CTRL_GR_INFO_INDEX_SHADER_PIPE_COUNT), K(NV2080_CTRL_GR_INFO_INDEX_SM_VERSION),
    K(NV2080_CTRL_GR_INFO_INDEX_LITTER_NUM_GPCS), K(NV2080_CTRL_GR_INFO_INDEX_LITTER_NUM_TPC_PER_GPC),
    K(NV2080_CTRL_GR_INFO_INDEX_LITTER_NUM_SM_PER_TPC), K(NV2080_CTRL_GR_INFO_INDEX_GPU_CORE_COUNT),
    K(NV2080_CTRL_GR_INFO_INDEX_MAX_WARPS_PER_SM),
    K(NV2080_ENGINE_TYPE_GRAPHICS), K(NV2080_ENGINE_TYPE_COPY0), K(NV2080_ENGINE_TYPE_COPY9),
    K(NV2080_ENGINE_TYPE_COPY10), K(NVA06F_SUBCHANNEL_COPY_ENGINE),
    K(NVB0B5_ALLOCATION_PARAMETERS_VERSION_1),
    K(NV_VASPACE_ALLOCATION_INDEX_GPU_NEW),
    K(HEAP_OWNER_RM_KERNEL_CLIENT),

    /* memory attributes and flags */
    K(NVOS32_TYPE_IMAGE),
    DEF(OS32, _ATTR, _LOCATION, _VIDMEM), DEF(OS32, _ATTR, _LOCATION, _PCI),
    DEF(OS32, _ATTR, _COHERENCY, _CACHED), DEF(OS32, _ATTR, _COHERENCY, _UNCACHED),
    DEF(OS32, _ATTR, _PHYSICALITY, _CONTIGUOUS), DEF(OS32, _ATTR, _PHYSICALITY, _NONCONTIGUOUS),
    DEF(OS32, _ATTR, _PAGE_SIZE, _4KB), DEF(OS32, _ATTR, _PAGE_SIZE, _BIG),
    DEF(OS32, _ATTR, _PAGE_SIZE, _DEFAULT),
    K(NVOS32_ALLOC_FLAGS_VIRTUAL), K(NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE),
    K(NVOS32_ALLOC_FLAGS_ALLOCATE_KERNEL_PRIVILEGED), K(NVOS32_ALLOC_FLAGS_PERSISTENT_VIDMEM),
    DEF(OS33, _FLAGS, _ACCESS, _READ_WRITE), DEF(OS33, _FLAGS, _ACCESS, _WRITE_ONLY),
    DEF(OS46, _FLAGS, _CACHE_SNOOP, _ENABLE), DEF(OS46, _FLAGS, _PAGE_SIZE, _4KB),
    DEF(OS46, _FLAGS, _PAGE_SIZE, _DEFAULT),
    K(NV0041_CTRL_CMD_GET_SURFACE_PHYS_ATTR_APERTURE_VIDMEM),
    K(NV0041_CTRL_CMD_GET_SURFACE_PHYS_ATTR_APERTURE_SYSMEM),
    K(NVC361_NOTIFY_CHANNEL_PENDING), K(NVC361_NV_USERMODE__SIZE),
    K(NVC56F_GP_ENTRY__SIZE),

    /* structures */
    SZ(NVOS21_PARAMETERS), SZ(NVOS54_PARAMETERS), SZ(nvidia_kernel_rmapi_ops_t),
    SZ(NV0000_ALLOC_PARAMETERS),
    SZ(NV0080_ALLOC_PARAMETERS),
    OFF(NV0080_ALLOC_PARAMETERS, hClientShare), OFF(NV0080_ALLOC_PARAMETERS, vaSpaceSize),
    OFF(NV0080_ALLOC_PARAMETERS, vaMode),
    SZ(NV2080_ALLOC_PARAMETERS),
    SZ(NV_VASPACE_ALLOCATION_PARAMETERS),
    OFF(NV_VASPACE_ALLOCATION_PARAMETERS, vaSize), OFF(NV_VASPACE_ALLOCATION_PARAMETERS, bigPageSize),
    OFF(NV_VASPACE_ALLOCATION_PARAMETERS, vaBase),
    SZ(NV_MEMORY_ALLOCATION_PARAMS),
    OFF(NV_MEMORY_ALLOCATION_PARAMS, attr), OFF(NV_MEMORY_ALLOCATION_PARAMS, attr2),
    OFF(NV_MEMORY_ALLOCATION_PARAMS, rangeLo), OFF(NV_MEMORY_ALLOCATION_PARAMS, size),
    OFF(NV_MEMORY_ALLOCATION_PARAMS, alignment), OFF(NV_MEMORY_ALLOCATION_PARAMS, offset),
    OFF(NV_MEMORY_ALLOCATION_PARAMS, limit), OFF(NV_MEMORY_ALLOCATION_PARAMS, address),
    OFF(NV_MEMORY_ALLOCATION_PARAMS, ctagOffset), OFF(NV_MEMORY_ALLOCATION_PARAMS, hVASpace),
    OFF(NV_MEMORY_ALLOCATION_PARAMS, numaNode),
    SZ(NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS),
    OFF(NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS, engineType),
    OFF(NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS, bIsCallingContextVgpuPlugin),
    SZ(NV_MEMORY_DESC_PARAMS),
    SZ(NV_CHANNEL_ALLOC_PARAMS),
    OFF(NV_CHANNEL_ALLOC_PARAMS, gpFifoOffset), OFF(NV_CHANNEL_ALLOC_PARAMS, gpFifoEntries),
    OFF(NV_CHANNEL_ALLOC_PARAMS, hVASpace), OFF(NV_CHANNEL_ALLOC_PARAMS, hUserdMemory),
    OFF(NV_CHANNEL_ALLOC_PARAMS, userdOffset), OFF(NV_CHANNEL_ALLOC_PARAMS, engineType),
    OFF(NV_CHANNEL_ALLOC_PARAMS, hObjectEccError), OFF(NV_CHANNEL_ALLOC_PARAMS, instanceMem),
    OFF(NV_CHANNEL_ALLOC_PARAMS, hPhysChannelGroup), OFF(NV_CHANNEL_ALLOC_PARAMS, errorNotifierMem),
    OFF(NV_CHANNEL_ALLOC_PARAMS, ProcessID), OFF(NV_CHANNEL_ALLOC_PARAMS, encryptIv),
    OFF(NV_CHANNEL_ALLOC_PARAMS, hmacNonce), OFF(NV_CHANNEL_ALLOC_PARAMS, tpcConfigID),
    SZ(NVB0B5_ALLOCATION_PARAMETERS),
    SZ(NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_V2_PARAMS),
    SZ(NV0000_CTRL_GPU_GET_ATTACHED_IDS_PARAMS),
    SZ(NV0000_CTRL_GPU_GET_ID_INFO_V2_PARAMS),
    SZ(NV0041_CTRL_GET_SURFACE_PHYS_ATTR_PARAMS),
    OFF(NV0041_CTRL_GET_SURFACE_PHYS_ATTR_PARAMS, memAperture),
    OFF(NV0041_CTRL_GET_SURFACE_PHYS_ATTR_PARAMS, contigSegmentSize),
    SZ(NV0080_CTRL_GPU_GET_CLASSLIST_V2_PARAMS),
    SZ(NV2080_CTRL_GPU_GET_NAME_STRING_PARAMS),
    SZ(NV2080_CTRL_GPU_GET_ENGINES_V2_PARAMS),
    SZ(NV2080_CTRL_FB_GET_INFO_V2_PARAMS),
    SZ(NV2080_CTRL_GR_GET_INFO_V2_PARAMS),
    OFF(NV2080_CTRL_GR_GET_INFO_V2_PARAMS, grRouteInfo),
    SZ(NV2080_CTRL_GR_GET_GPC_MASK_PARAMS), OFF(NV2080_CTRL_GR_GET_GPC_MASK_PARAMS, gpcMask),
    SZ(NV2080_CTRL_GR_GET_TPC_MASK_PARAMS), OFF(NV2080_CTRL_GR_GET_TPC_MASK_PARAMS, tpcMask),
    SZ(NV2080_CTRL_CE_GET_CAPS_V2_PARAMS),
    SZ(NVA06F_CTRL_GPFIFO_SCHEDULE_PARAMS),
    SZ(NVC36F_CTRL_CMD_GPFIFO_GET_WORK_SUBMIT_TOKEN_PARAMS),
    SZ(NVC36F_CTRL_GPFIFO_SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX_PARAMS),
    OFF(AmpereAControlGPFifo, GPGet), OFF(AmpereAControlGPFifo, GPPut),
    OFF(AmpereAControlGPFifo, Get), OFF(AmpereAControlGPFifo, Put),
};

NvU32 lnv_abi_count(void)
{
    return (NvU32)(sizeof(lnv_abi_table) / sizeof(lnv_abi_table[0]));
}

/* Entry i: its name (NUL-terminated) and value. NULL past the end. */
const char *lnv_abi_entry(NvU32 i, NvU64 *value)
{
    if (i >= lnv_abi_count())
        return NULL;
    *value = lnv_abi_table[i].value;
    return lnv_abi_table[i].name;
}

/* ---------------------------------------------------------------------- */
/* 3. Push-buffer encoders (self-test)                                     */
/* ---------------------------------------------------------------------- */

/* Incrementing-method header (host class AMPERE_CHANNEL_GPFIFO_A). */
static NvU32 lnv_mthd(NvU32 subch, NvU32 method, NvU32 count)
{
    return DRF_DEF(C56F, _DMA, _SEC_OP, _INC_METHOD) |
           DRF_NUM(C56F, _DMA, _METHOD_ADDRESS, method >> 2) |
           DRF_NUM(C56F, _DMA, _METHOD_SUBCHANNEL, subch) |
           DRF_NUM(C56F, _DMA, _METHOD_COUNT, count);
}

/* Bind `cls` to subchannel `subch`. Returns dwords written. */
NvU32 lnv_push_set_object(NvU32 *p, NvU32 subch, NvU32 cls)
{
    p[0] = lnv_mthd(subch, NVC56F_SET_OBJECT, 1);
    p[1] = cls;
    return 2;
}

/* Address modes: 0 = GPU virtual, 1 = physical local FB (vidmem),
 * 2 = physical coherent sysmem. */
static NvU32 lnv_ce_phys_target(NvU32 mode)
{
    return mode == 2 ? DRF_DEF(C7B5, _SET_DST_PHYS_MODE, _TARGET, _COHERENT_SYSMEM)
                     : DRF_DEF(C7B5, _SET_DST_PHYS_MODE, _TARGET, _LOCAL_FB);
}

static NvU32 lnv_ce_flush(NvBool flush)
{
    return flush ? (DRF_DEF(C7B5, _LAUNCH_DMA, _FLUSH_ENABLE, _TRUE) |
                    DRF_DEF(C7B5, _LAUNCH_DMA, _FLUSH_TYPE, _SYS))
                 : DRF_DEF(C7B5, _LAUNCH_DMA, _FLUSH_ENABLE, _FALSE);
}

/*
 * 32-bit pattern fill of a pitch-linear rectangle: `width` elements of 4
 * bytes per line, `height` lines `pitch` bytes apart (height 1: a plain
 * linear fill of width*4 bytes). REMAP from CONST_A, as UVM's memset_4.
 */
NvU32 lnv_push_ce_memset(NvU32 *p, NvU32 subch, NvU64 dst, NvU32 dst_mode,
                         NvU32 pattern, NvU32 width, NvU32 height, NvU32 pitch,
                         NvBool flush)
{
    NvU32 n = 0;
    NvU32 launch;

    p[n++] = lnv_mthd(subch, NVC7B5_SET_REMAP_CONST_A, 3);
    p[n++] = pattern;                          /* SET_REMAP_CONST_A */
    p[n++] = pattern;                          /* SET_REMAP_CONST_B */
    p[n++] = DRF_DEF(C7B5, _SET_REMAP_COMPONENTS, _DST_X, _CONST_A) |
             DRF_DEF(C7B5, _SET_REMAP_COMPONENTS, _COMPONENT_SIZE, _FOUR) |
             DRF_DEF(C7B5, _SET_REMAP_COMPONENTS, _NUM_SRC_COMPONENTS, _ONE) |
             DRF_DEF(C7B5, _SET_REMAP_COMPONENTS, _NUM_DST_COMPONENTS, _ONE);
    if (dst_mode != 0)
    {
        p[n++] = lnv_mthd(subch, NVC7B5_SET_DST_PHYS_MODE, 1);
        p[n++] = lnv_ce_phys_target(dst_mode);
    }
    p[n++] = lnv_mthd(subch, NVC7B5_OFFSET_OUT_UPPER, 2);
    p[n++] = DRF_NUM(C7B5, _OFFSET_OUT_UPPER, _UPPER, NvU64_HI32(dst));
    p[n++] = NvU64_LO32(dst);
    if (height > 1)
    {
        p[n++] = lnv_mthd(subch, NVC7B5_PITCH_IN, 4);
        p[n++] = pitch;                        /* PITCH_IN */
        p[n++] = pitch;                        /* PITCH_OUT */
        p[n++] = width;                        /* LINE_LENGTH_IN */
        p[n++] = height;                       /* LINE_COUNT */
    }
    else
    {
        p[n++] = lnv_mthd(subch, NVC7B5_LINE_LENGTH_IN, 1);
        p[n++] = width;
    }
    launch = DRF_DEF(C7B5, _LAUNCH_DMA, _DATA_TRANSFER_TYPE, _NON_PIPELINED) |
             DRF_DEF(C7B5, _LAUNCH_DMA, _SRC_MEMORY_LAYOUT, _PITCH) |
             DRF_DEF(C7B5, _LAUNCH_DMA, _DST_MEMORY_LAYOUT, _PITCH) |
             DRF_DEF(C7B5, _LAUNCH_DMA, _REMAP_ENABLE, _TRUE) |
             DRF_DEF(C7B5, _LAUNCH_DMA, _DISABLE_PLC, _TRUE) |
             (height > 1 ? DRF_DEF(C7B5, _LAUNCH_DMA, _MULTI_LINE_ENABLE, _TRUE)
                         : DRF_DEF(C7B5, _LAUNCH_DMA, _MULTI_LINE_ENABLE, _FALSE)) |
             (dst_mode != 0 ? DRF_DEF(C7B5, _LAUNCH_DMA, _DST_TYPE, _PHYSICAL)
                            : DRF_DEF(C7B5, _LAUNCH_DMA, _DST_TYPE, _VIRTUAL)) |
             lnv_ce_flush(flush);
    p[n++] = lnv_mthd(subch, NVC7B5_LAUNCH_DMA, 1);
    p[n++] = launch;
    return n;
}

/* Linear copy of `bytes` from src to dst (modes as above). */
NvU32 lnv_push_ce_copy(NvU32 *p, NvU32 subch, NvU64 dst, NvU32 dst_mode,
                       NvU64 src, NvU32 src_mode, NvU32 bytes, NvBool flush)
{
    NvU32 n = 0;
    NvU32 launch;

    if (src_mode != 0)
    {
        p[n++] = lnv_mthd(subch, NVC7B5_SET_SRC_PHYS_MODE, 1);
        p[n++] = lnv_ce_phys_target(src_mode);
    }
    if (dst_mode != 0)
    {
        p[n++] = lnv_mthd(subch, NVC7B5_SET_DST_PHYS_MODE, 1);
        p[n++] = lnv_ce_phys_target(dst_mode);
    }
    p[n++] = lnv_mthd(subch, NVC7B5_OFFSET_IN_UPPER, 4);
    p[n++] = DRF_NUM(C7B5, _OFFSET_IN_UPPER, _UPPER, NvU64_HI32(src));
    p[n++] = NvU64_LO32(src);
    p[n++] = DRF_NUM(C7B5, _OFFSET_OUT_UPPER, _UPPER, NvU64_HI32(dst));
    p[n++] = NvU64_LO32(dst);
    p[n++] = lnv_mthd(subch, NVC7B5_LINE_LENGTH_IN, 1);
    p[n++] = bytes;
    launch = DRF_DEF(C7B5, _LAUNCH_DMA, _DATA_TRANSFER_TYPE, _NON_PIPELINED) |
             DRF_DEF(C7B5, _LAUNCH_DMA, _SRC_MEMORY_LAYOUT, _PITCH) |
             DRF_DEF(C7B5, _LAUNCH_DMA, _DST_MEMORY_LAYOUT, _PITCH) |
             DRF_DEF(C7B5, _LAUNCH_DMA, _MULTI_LINE_ENABLE, _FALSE) |
             DRF_DEF(C7B5, _LAUNCH_DMA, _REMAP_ENABLE, _FALSE) |
             DRF_DEF(C7B5, _LAUNCH_DMA, _DISABLE_PLC, _TRUE) |
             (src_mode != 0 ? DRF_DEF(C7B5, _LAUNCH_DMA, _SRC_TYPE, _PHYSICAL)
                            : DRF_DEF(C7B5, _LAUNCH_DMA, _SRC_TYPE, _VIRTUAL)) |
             (dst_mode != 0 ? DRF_DEF(C7B5, _LAUNCH_DMA, _DST_TYPE, _PHYSICAL)
                            : DRF_DEF(C7B5, _LAUNCH_DMA, _DST_TYPE, _VIRTUAL)) |
             lnv_ce_flush(flush);
    p[n++] = lnv_mthd(subch, NVC7B5_LAUNCH_DMA, 1);
    p[n++] = launch;
    return n;
}

/*
 * Copy-engine semaphore release: writes the 32-bit `payload` to the GPU VA
 * `sem` once everything before it on this channel is done, with a SYS
 * flush first (UVM's semaphore_release). `intr` additionally raises a
 * non-stall interrupt.
 */
NvU32 lnv_push_ce_sem_release(NvU32 *p, NvU32 subch, NvU64 sem, NvU32 payload, NvBool intr)
{
    NvU32 n = 0;

    p[n++] = lnv_mthd(subch, NVC7B5_SET_SEMAPHORE_A, 3);
    p[n++] = DRF_NUM(C7B5, _SET_SEMAPHORE_A, _UPPER, NvU64_HI32(sem));
    p[n++] = NvU64_LO32(sem);
    p[n++] = payload;
    p[n++] = lnv_mthd(subch, NVC7B5_LAUNCH_DMA, 1);
    p[n++] = DRF_DEF(C7B5, _LAUNCH_DMA, _DATA_TRANSFER_TYPE, _NONE) |
             DRF_DEF(C7B5, _LAUNCH_DMA, _SEMAPHORE_TYPE, _RELEASE_ONE_WORD_SEMAPHORE) |
             DRF_DEF(C7B5, _LAUNCH_DMA, _DISABLE_PLC, _TRUE) |
             (intr ? DRF_DEF(C7B5, _LAUNCH_DMA, _INTERRUPT_TYPE, _NON_BLOCKING)
                   : DRF_DEF(C7B5, _LAUNCH_DMA, _INTERRUPT_TYPE, _NONE)) |
             lnv_ce_flush(NV_TRUE);
    return n;
}

/*
 * The fence segment the nouveau-UAPI layer appends to every EXEC, on the
 * channel's host class: a 64-bit semaphore release with wait-for-idle, then a
 * SYS membar, then (optionally) a non-stall interrupt -- Linux nouveau's
 * gv100_fence_emit32 sequence with a 64-bit payload. Methods go to
 * subchannel 0 (host methods are recognised by address, not subchannel).
 */
NvU32 lnv_push_host_fence(NvU32 *p, NvU64 sem, NvU64 payload, NvBool intr)
{
    NvU32 n = 0;

    p[n++] = lnv_mthd(0, NVC56F_SEM_ADDR_LO, 5);
    p[n++] = DRF_NUM(C56F, _SEM_ADDR_LO, _OFFSET, NvU64_LO32(sem) >> 2);
    p[n++] = DRF_NUM(C56F, _SEM_ADDR_HI, _OFFSET, NvU64_HI32(sem));
    p[n++] = NvU64_LO32(payload);
    p[n++] = NvU64_HI32(payload);
    p[n++] = DRF_DEF(C56F, _SEM_EXECUTE, _OPERATION, _RELEASE) |
             DRF_DEF(C56F, _SEM_EXECUTE, _RELEASE_WFI, _EN) |
             DRF_DEF(C56F, _SEM_EXECUTE, _PAYLOAD_SIZE, _64BIT) |
             DRF_DEF(C56F, _SEM_EXECUTE, _RELEASE_TIMESTAMP, _DIS);
    p[n++] = lnv_mthd(0, NVC56F_MEM_OP_A, 4);
    p[n++] = 0;
    p[n++] = 0;
    p[n++] = DRF_DEF(C56F, _MEM_OP_C, _MEMBAR_TYPE, _SYS_MEMBAR);
    p[n++] = DRF_DEF(C56F, _MEM_OP_D, _OPERATION, _MEMBAR);
    if (intr)
    {
        p[n++] = lnv_mthd(0, NVC56F_NON_STALL_INTERRUPT, 1);
        p[n++] = 0;
    }
    return n;
}

/* A GPFIFO entry for a client push segment: `bytes` at GPU VA `va`; `wait`
 * sets SYNC_WAIT (nouveau's DRM_NOUVEAU_EXEC_PUSH_NO_PREFETCH). */
NvU64 lnv_gp_entry_sync(NvU64 va, NvU32 bytes, NvBool wait)
{
    NvU32 e0 = DRF_DEF(C56F, _GP_ENTRY0, _FETCH, _UNCONDITIONAL) |
               DRF_NUM(C56F, _GP_ENTRY0, _GET, NvU64_LO32(va) >> 2);
    NvU32 e1 = DRF_NUM(C56F, _GP_ENTRY1, _GET_HI, NvU64_HI32(va)) |
               DRF_DEF(C56F, _GP_ENTRY1, _LEVEL, _MAIN) |
               DRF_NUM(C56F, _GP_ENTRY1, _LENGTH, bytes >> 2) |
               (wait ? DRF_DEF(C56F, _GP_ENTRY1, _SYNC, _WAIT)
                     : DRF_DEF(C56F, _GP_ENTRY1, _SYNC, _PROCEED));
    return ((NvU64)e1 << 32) | e0;
}

/* One GPFIFO entry: a main-level push segment of `bytes` at GPU VA `pb`. */
NvU64 lnv_gp_entry(NvU64 pb, NvU32 bytes)
{
    NvU32 e0 = DRF_DEF(C56F, _GP_ENTRY0, _FETCH, _UNCONDITIONAL) |
               DRF_NUM(C56F, _GP_ENTRY0, _GET, NvU64_LO32(pb) >> 2);
    NvU32 e1 = DRF_NUM(C56F, _GP_ENTRY1, _GET_HI, NvU64_HI32(pb)) |
               DRF_DEF(C56F, _GP_ENTRY1, _LEVEL, _MAIN) |
               DRF_NUM(C56F, _GP_ENTRY1, _LENGTH, bytes >> 2) |
               DRF_DEF(C56F, _GP_ENTRY1, _SYNC, _PROCEED);
    return ((NvU64)e1 << 32) | e0;
}

/* ---------------------------------------------------------------------- */
/* 4. User-mapping context (nv_add_mapping_context_to_file)               */
/* ---------------------------------------------------------------------- */

/*
 * Linux keeps this in the nv_linux_file_private_t of the fd the client
 * passes in NV_ESC_RM_MAP_MEMORY, and its mmap() handler consumes it.
 * LeandrOS has no /dev/nvidia* files yet, so the context is kept in a small
 * fd-keyed table until the UAPI layer takes it (lnv_mapping_context_take).
 * Kernel clients never get here: their NV04_MAP_MEMORY is a kernel mapping.
 */
#define LNV_MMAP_SLOTS  16

static struct
{
    NvBool         valid;
    NvU32          fd;
    lnv_mmap_ctx_t ctx;
} lnv_mmap_slots[LNV_MMAP_SLOTS];

NV_STATUS NV_API_CALL nv_add_mapping_context_to_file(
    nv_state_t *nv, nv_usermap_access_params_t *nvuap, NvU32 prot,
    void *pAllocPriv, NvU64 pageIndex, NvU32 fd)
{
    NvU32 i, slot = LNV_MMAP_SLOTS;
    lnv_mmap_ctx_t *c;

    for (i = 0; i < LNV_MMAP_SLOTS; i++)
    {
        if (lnv_mmap_slots[i].valid && lnv_mmap_slots[i].fd == fd)
            return NV_ERR_STATE_IN_USE;      /* as Linux: one pending map per fd */
        if (!lnv_mmap_slots[i].valid && slot == LNV_MMAP_SLOTS)
            slot = i;
    }
    if (slot == LNV_MMAP_SLOTS)
        return NV_ERR_INSUFFICIENT_RESOURCES;

    c = &lnv_mmap_slots[slot].ctx;
    os_mem_set(c, 0, sizeof(*c));
    c->is_ctl  = NV_IS_CTL_DEVICE(nv) ? 1 : 0;
    c->prot    = prot;
    c->caching = nvuap->caching;
    if (c->is_ctl)
    {
        c->alloc      = (NvU64)(NvUPtr)pAllocPriv;
        c->page_index = pageIndex;
    }
    else
    {
        if (nvuap->memArea.numRanges > LNV_MMAP_RANGES)
            return NV_ERR_INSUFFICIENT_RESOURCES;
        for (i = 0; i < nvuap->memArea.numRanges; i++)
        {
            c->range_start[i] = nvuap->memArea.pRanges[i].start;
            c->range_size[i]  = nvuap->memArea.pRanges[i].size;
        }
        c->nranges      = (NvU32)nvuap->memArea.numRanges;
        c->access_start = nvuap->access_start;
        c->access_size  = nvuap->access_size;
    }
    lnv_mmap_slots[slot].fd    = fd;
    lnv_mmap_slots[slot].valid = NV_TRUE;
    return NV_OK;
}

/* For the UAPI layer's mmap(): take (and clear) the context of `fd`. */
NvBool lnv_mapping_context_take(NvU32 fd, lnv_mmap_ctx_t *out)
{
    NvU32 i;

    for (i = 0; i < LNV_MMAP_SLOTS; i++)
    {
        if (lnv_mmap_slots[i].valid && lnv_mmap_slots[i].fd == fd)
        {
            *out = lnv_mmap_slots[i].ctx;
            lnv_mmap_slots[i].valid = NV_FALSE;
            return NV_TRUE;
        }
    }
    return NV_FALSE;
}
