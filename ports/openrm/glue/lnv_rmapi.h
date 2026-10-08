/*
 * SPDX-License-Identifier: MIT
 *
 * The in-kernel RM client path (lnv_rmapi.c). Declared on the Rust side in
 * drivers/nvidia/src/client.rs (RM API) and selftest.rs (push encoders);
 * keep the three in sync.
 */
#ifndef LNV_RMAPI_H
#define LNV_RMAPI_H

#include "nvtypes.h"

/* RM API for kernel clients (rm_kernel_rmapi_op). Return an NV_STATUS. */
NvU32 lnv_rm_alloc(NvHandle hClient, NvHandle hParent, NvHandle *phObject,
                   NvU32 hClass, void *pParams, NvU32 paramsSize);
NvU32 lnv_rm_free(NvHandle hClient, NvHandle hParent, NvHandle hObject);
NvU32 lnv_rm_control(NvHandle hClient, NvHandle hObject, NvU32 cmd,
                     void *pParams, NvU32 paramsSize);
NvU32 lnv_rm_map_memory(NvHandle hClient, NvHandle hDevice, NvHandle hMemory,
                        NvU64 offset, NvU64 length, void **ppAddress, NvU32 flags);
NvU32 lnv_rm_unmap_memory(NvHandle hClient, NvHandle hDevice, NvHandle hMemory,
                          void *pAddress, NvU32 flags);
NvU32 lnv_rm_map_memory_dma(NvHandle hClient, NvHandle hDevice, NvHandle hDma,
                            NvHandle hMemory, NvU64 offset, NvU64 length,
                            NvU32 flags, NvU64 *pDmaOffset);
NvU32 lnv_rm_unmap_memory_dma(NvHandle hClient, NvHandle hDevice, NvHandle hDma,
                              NvHandle hMemory, NvU32 flags, NvU64 dmaOffset);
NvU32 lnv_rm_dup_object(NvHandle hClient, NvHandle hParent, NvHandle *phObject,
                        NvHandle hClientSrc, NvHandle hObjectSrc, NvU32 flags);
const char *lnv_status_str(NvU32 status);

/* ABI table: entry i's name and value; NULL past the end. */
NvU32 lnv_abi_count(void);
const char *lnv_abi_entry(NvU32 i, NvU64 *value);

/* Push encoders; each returns the number of dwords written at p.
 * Address modes: 0 GPU virtual, 1 physical vidmem, 2 physical coherent sysmem. */
NvU32 lnv_push_set_object(NvU32 *p, NvU32 subch, NvU32 cls);
NvU32 lnv_push_ce_memset(NvU32 *p, NvU32 subch, NvU64 dst, NvU32 dst_mode,
                         NvU32 pattern, NvU32 width, NvU32 height, NvU32 pitch,
                         NvBool flush);
NvU32 lnv_push_ce_copy(NvU32 *p, NvU32 subch, NvU64 dst, NvU32 dst_mode,
                       NvU64 src, NvU32 src_mode, NvU32 bytes, NvBool flush);
NvU32 lnv_push_ce_sem_release(NvU32 *p, NvU32 subch, NvU64 sem, NvU32 payload, NvBool intr);
NvU64 lnv_gp_entry(NvU64 pb, NvU32 bytes);
NvU32 lnv_push_host_fence(NvU32 *p, NvU64 sem, NvU64 payload, NvBool intr);
NvU64 lnv_gp_entry_sync(NvU64 va, NvU32 bytes, NvBool wait);

/* User-mapping context recorded by nv_add_mapping_context_to_file. */
#define LNV_MMAP_RANGES 8
typedef struct
{
    NvU64 alloc;          /* control device: the nv_alloc_pages private */
    NvU64 page_index;
    NvU64 access_start;   /* GPU device: bus-address window of the mapping */
    NvU64 access_size;
    NvU64 range_start[LNV_MMAP_RANGES];
    NvU64 range_size[LNV_MMAP_RANGES];
    NvU32 nranges;
    NvU32 prot;
    NvU32 caching;
    NvU32 is_ctl;
} lnv_mmap_ctx_t;

NvBool lnv_mapping_context_take(NvU32 fd, lnv_mmap_ctx_t *out);

#endif
