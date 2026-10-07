/*
 * SPDX-License-Identifier: MIT
 *
 * Not built into libnvrm.a. Parsed by ports/openrm/gen-shim.py with the
 * libspdm include paths/defines (from openspdm.mk) to recover the prototypes
 * of the libspdm crypto backend RM expects the OS to provide (Linux does it
 * in kernel-open/nvidia/libspdm_*.c on top of the kernel crypto API).
 */
#include "library/spdm_crypt_lib.h"
#include "hal/library/cryptlib.h"
#include "nvspdm_cryptlib_extensions.h"
