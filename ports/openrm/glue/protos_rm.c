/*
 * SPDX-License-Identifier: MIT
 *
 * Not built into libnvrm.a. Parsed by ports/openrm/gen-shim.py (clang JSON
 * AST) to recover the exact C prototype of every OS callback nv-kernel.o
 * imports, so the generated Rust stubs carry correct signatures.
 */
#include "nv.h"
#include "os-interface.h"
#include "nv-reg.h"
#include "export_nvswitch.h"
#include "nvlink_os.h"
#include "nvlink_export.h"
