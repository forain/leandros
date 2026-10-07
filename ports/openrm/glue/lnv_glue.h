/*
 * SPDX-License-Identifier: MIT
 *
 * The only structure the LeandrOS Rust shim shares with OpenRM is nv_state_t,
 * and it never sees its layout: the shim allocates lnv_state_size() bytes
 * (8-byte aligned) and fills it through these accessors. Declared on the Rust
 * side in drivers/nvidia/src/glue.rs; keep the two in sync.
 */
#ifndef LNV_GLUE_H
#define LNV_GLUE_H

#include "nvtypes.h"

struct nv_state_t;

NvU64  lnv_state_size(void);
void   lnv_state_init(struct nv_state_t *nv, void *os_state, void *pci_handle,
                      NvU32 domain, NvU8 bus, NvU8 slot, NvU8 function,
                      NvU16 vendor_id, NvU16 device_id,
                      NvU16 subsystem_vendor, NvU16 subsystem_id,
                      NvU32 interrupt_line);
/* nv_bar_index: 0 = REGS (BAR0), 1 = FB (BAR1), 2 = IMEM (BAR2/3);
 * pci_bar: the PCI BAR number it was decoded from. */
NvBool lnv_state_set_bar(struct nv_state_t *nv, NvU32 nv_bar_index, NvU32 pci_bar,
                         NvU64 cpu_address, NvU64 size);
void  *lnv_state_os_state(struct nv_state_t *nv);
void  *lnv_state_pci_handle(struct nv_state_t *nv);

#endif
