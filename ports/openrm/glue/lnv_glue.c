/*
 * SPDX-License-Identifier: MIT
 *
 * LeandrOS <-> NVIDIA OpenRM C glue.
 *
 * Compiled by ports/openrm/build.sh with exactly the flags NVIDIA's
 * src/nvidia/Makefile uses for nv-kernel.o, and archived next to it in
 * libnvrm.a. Everything that needs OpenRM's own headers lives here so the Rust
 * shim (drivers/nvidia) never has to mirror RM struct layouts:
 *
 *   - data symbols RM reads directly (os_page_*, os_cc_*, nv_parms, ...)
 *   - the registry-defaults table (nv-reg.h), and os_registry_init(), which
 *     pushes it into RM the same way kernel-open/nvidia/os-registry.c does
 *   - the C-variadic / va_list entry points (nv_printf, os_snprintf, ...),
 *     formatted with RM's own nvDbgVsnprintf and handed to Rust as a string
 *   - nv_state_t accessors for the probe path (lnv_glue.h)
 *
 * Every other OS callback is a Rust extern "C" fn in drivers/nvidia.
 */

#define NV_DEFINE_REGISTRY_KEY_TABLE

#include "nv.h"
#include "os-interface.h"
#include "nv-reg.h"
#include "core/printf.h"
#include "export_nvswitch.h"

#include "lnv_glue.h"

/* Rust side: drivers/nvidia/src/log.rs */
extern void lnv_log_str(NvU32 level, const char *msg);

/* ---------------------------------------------------------------------- */
/* Data symbols                                                            */
/* ---------------------------------------------------------------------- */

NvU32  os_page_size  = 4096;
NvU64  os_page_mask  = ~(NvU64)(4096 - 1);
NvU8   os_page_shift = 12;
NvBool os_cc_enabled                = NV_FALSE;
NvBool os_cc_sev_snp_enabled        = NV_FALSE;
NvBool os_cc_snp_vtom_enabled       = NV_FALSE;
NvBool os_cc_tdx_enabled            = NV_FALSE;
NvBool os_dma_buf_enabled           = NV_FALSE;
NvBool os_imex_channel_is_supported = NV_FALSE;

/* No /dev/nvidia-caps: capability entries are created under a NULL root and
 * RM skips them (os.c checks for NULL). */
nv_cap_t *nvidia_caps_root = NULL;

/* ---------------------------------------------------------------------- */
/* Registry                                                                */
/* ---------------------------------------------------------------------- */

NV_STATUS NV_API_CALL os_registry_init(void)
{
    nv_parm_t *entry;
    unsigned int i;

    /* Same order as Linux: module-parameter strings first (none here), then
     * every table default, so RM sees e.g. EnableGpuFirmware exactly as a
     * stock `modprobe nvidia` with no options would. */
    for (i = 0; (entry = &nv_parms[i])->name != NULL; i++)
    {
        rm_write_registry_dword(NULL, NULL, entry->name, *entry->data);
    }

    return NV_OK;
}

/* ---------------------------------------------------------------------- */
/* printf family                                                           */
/* ---------------------------------------------------------------------- */

#define LNV_LOG_BUF 512

static void lnv_vlog(NvU32 level, const char *fmt, va_list ap)
{
    /* On the caller's stack: callable from ISR context without allocating. */
    char buf[LNV_LOG_BUF];
    nvDbgVsnprintf(buf, sizeof(buf), fmt, ap);
    buf[sizeof(buf) - 1] = '\0';
    lnv_log_str(level, buf);
}

int NV_API_CALL nv_printf(NvU32 debuglevel, const char *printf_format, ...)
{
    va_list ap;
    va_start(ap, printf_format);
    lnv_vlog(debuglevel, printf_format, ap);
    va_end(ap);
    return 0;
}

NV_STATUS NV_API_CALL nv_log_error(nv_state_t *nv, NvU32 error_number,
                                   const char *format, va_list ap)
{
    (void)nv;
    (void)error_number;
    lnv_vlog(NV_DBG_ERRORS, format, ap);
    return NV_OK;
}

NvS32 NV_API_CALL os_snprintf(char *buf, NvU32 size, const char *fmt, ...)
{
    va_list ap;
    int n;
    va_start(ap, fmt);
    n = nvDbgVsnprintf(buf, size, fmt, ap);
    va_end(ap);
    return n;
}

int nvswitch_os_vsnprintf(char *buf, NvLength size, const char *fmt, va_list arglist)
{
    return nvDbgVsnprintf(buf, (NvU32)size, fmt, arglist);
}

int nvswitch_os_snprintf(char *buf, NvLength size, const char *fmt, ...)
{
    va_list ap;
    int n;
    va_start(ap, fmt);
    n = nvDbgVsnprintf(buf, (NvU32)size, fmt, ap);
    va_end(ap);
    return n;
}

void nvswitch_os_print(int log_level, const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    lnv_vlog((NvU32)log_level, fmt, ap);
    va_end(ap);
}

void nvswitch_os_assert_log(const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    lnv_vlog(NV_DBG_ERRORS, fmt, ap);
    va_end(ap);
}

/* ---------------------------------------------------------------------- */
/* nv_state_t accessors (see lnv_glue.h)                                   */
/* ---------------------------------------------------------------------- */

NvU64 lnv_state_size(void)
{
    return sizeof(nv_state_t);
}

void lnv_state_init(nv_state_t *nv, void *os_state, void *pci_handle,
                    NvU32 domain, NvU8 bus, NvU8 slot, NvU8 function,
                    NvU16 vendor_id, NvU16 device_id,
                    NvU16 subsystem_vendor, NvU16 subsystem_id,
                    NvU32 interrupt_line)
{
    os_mem_set(nv, 0, sizeof(*nv));
    nv->os_state             = os_state;
    nv->handle               = pci_handle;
    nv->pci_info.domain      = domain;
    nv->pci_info.bus         = bus;
    nv->pci_info.slot        = slot;
    nv->pci_info.function    = function;
    nv->pci_info.vendor_id   = vendor_id;
    nv->pci_info.device_id   = device_id;
    nv->subsystem_vendor     = subsystem_vendor;
    nv->subsystem_id         = subsystem_id;
    nv->interrupt_line       = interrupt_line;
    nv->regs                 = &nv->bars[NV_GPU_BAR_INDEX_REGS];
    nv->fb                   = &nv->bars[NV_GPU_BAR_INDEX_FB];
}

NvBool lnv_state_set_bar(nv_state_t *nv, NvU32 nv_bar_index, NvU32 pci_bar,
                         NvU64 cpu_address, NvU64 size)
{
    if (nv_bar_index >= NV_GPU_NUM_BARS)
        return NV_FALSE;
    nv->bars[nv_bar_index].offset      = NVRM_PCICFG_BAR_OFFSET(pci_bar);
    nv->bars[nv_bar_index].cpu_address = cpu_address;
    nv->bars[nv_bar_index].size        = size;
    return NV_TRUE;
}

void *lnv_state_os_state(nv_state_t *nv)
{
    return nv->os_state;
}

void *lnv_state_pci_handle(nv_state_t *nv)
{
    return nv->handle;
}
