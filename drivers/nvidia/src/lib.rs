//! LeandrOS OS-interface shim for NVIDIA's open-gpu-kernel-modules resource
//! manager (OpenRM), the same split X547's nvidia-haiku port uses: NVIDIA's
//! OS-agnostic RM (`nv-kernel.o`, here archived as `libnvrm.a` by
//! ports/openrm/build.sh) is linked into the kernel, and this crate supplies
//! the `os_*` / `nv_*` callbacks Linux's kernel-open/ layer would.
//!
//! Layout:
//!   - ports/openrm/glue/lnv_glue.c (inside libnvrm.a): data symbols, the
//!     registry table, C-variadic printf family, nv_state_t accessors.
//!   - `stubs`      (generated): every other OS callback, logging + failing.
//!   - `rm_exports` (generated): keeps every RM entry point live in the link.
//!   - `rm`, `glue`: typed declarations of what this crate calls.
//!
//!   - `basic`: real heap / memory / string / debug / spinlock callbacks.
//!   - `interim`: spinning mutex/rwlock/semaphore + clock callbacks, enough
//!     for `rm_init_rm` (replace with sleeping primitives in G2).
//!
//! Status (lane/openrm, G1 feasibility spike): links and boots. Built with
//! LEANDROS_NVRM_SELFTEST=1, `rm_init_rm` runs to completion (returns true)
//! in QEMU with no GPU. Without an NVIDIA GPU nothing else runs.
//!
//! Licensing: OpenRM is dual MIT/GPLv2; LeandrOS uses it under MIT.

#![no_std]
#![allow(clippy::missing_safety_doc)]

extern crate alloc;

use core::ffi::{c_char, CStr};
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU32, Ordering};

mod basic;
mod interim;
pub mod glue;
pub mod rm;
pub mod rm_exports;
pub mod stubs;

/// NV_STATUS values (src/common/sdk/nvidia/inc/nvstatuscodes.h).
pub const NV_OK: u32 = 0x0000_0000;
pub const NV_ERR_NOT_SUPPORTED: u32 = 0x0000_0056;
/// NvlStatus (src/common/nvlink/interface/nvlink_errors.h); returned negated.
pub const NVL_ERR_NOT_SUPPORTED: i32 = 13;

pub const PCI_VENDOR_NVIDIA: u16 = 0x10DE;

/// NV_DBG_* levels (os-interface.h): INFO 0, SETUP 1, USERERRORS 2,
/// WARNINGS 3, ERRORS 4. RM messages below this are dropped.
const LOG_MIN_LEVEL: u32 = 1;

// ---------------------------------------------------------------- logging --

struct Serial;

impl Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        drivers::pci::serial_debug(s);
        Ok(())
    }
}

/// Lock-free serial print; callable from any context, including an RM ISR.
pub fn log(args: fmt::Arguments) {
    let _ = Serial.write_fmt(args);
}

macro_rules! nvlog {
    ($($t:tt)*) => { $crate::log(format_args!($($t)*)) };
}
pub(crate) use nvlog;

/// Sink for every RM message (nv_printf, nv_log_error, nvswitch_os_print),
/// already formatted by the C glue with RM's own nvDbgVsnprintf.
#[no_mangle]
pub unsafe extern "C" fn lnv_log_str(level: u32, msg: *const c_char) {
    if msg.is_null() || level < LOG_MIN_LEVEL {
        return;
    }
    let s = CStr::from_ptr(msg).to_bytes();
    drivers::pci::serial_debug(core::str::from_utf8(s).unwrap_or("<non-utf8 RM message>\n"));
}

static STUB_LOG_BUDGET: AtomicU32 = AtomicU32::new(256);

/// Called by every generated stub: name the unimplemented callback RM just
/// reached (first 256 calls, so a hot path cannot flood the serial line).
pub fn stub_called(name: &str) {
    if STUB_LOG_BUDGET
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
        .is_ok()
    {
        nvlog!("[nvrm] unimplemented OS callback: {}\n", name);
    }
}

// ------------------------------------------------------------------ probe --

/// Boot-time entry: look for NVIDIA display controllers and, if one exists,
/// start the RM bring-up sequence (rm_init_rm -> per-GPU nv_state_t ->
/// rm_init_private_state). With no NVIDIA device this logs one line and
/// returns without touching RM.
pub fn init() {
    let gpus: alloc::vec::Vec<_> = drivers::pci::scan()
        .into_iter()
        .filter(|d| d.vendor_id == PCI_VENDOR_NVIDIA && d.class == 0x03)
        .collect();
    if gpus.is_empty() {
        nvlog!("[nvrm] OpenRM linked; no NVIDIA display controller present\n");
        // Build with LEANDROS_NVRM_SELFTEST=1 to run rm_init_rm anyway: it
        // executes real RM code in our address space and must fail cleanly
        // at the first stubbed callback.
        if option_env!("LEANDROS_NVRM_SELFTEST").is_some() {
            let ok = unsafe { rm::rm_init_rm(core::ptr::null_mut()) };
            nvlog!("[nvrm] selftest: rm_init_rm returned {}\n", ok);
        }
        return;
    }
    for d in &gpus {
        nvlog!(
            "[nvrm] found {:02x}:{:02x}.{} {:04x}:{:04x}\n",
            d.bus, d.dev, d.func, d.vendor_id, d.device_id
        );
    }
    // SAFETY: RM global init; sp (the Linux alt-stack cookie) is unused on
    // x86_64 and Haiku passes NULL too.
    if !unsafe { rm::rm_init_rm(core::ptr::null_mut()) } {
        nvlog!("[nvrm] rm_init_rm failed (OS callbacks still stubs)\n");
        return;
    }
    for d in &gpus {
        if let Err(e) = unsafe { rm::probe_device(d) } {
            nvlog!("[nvrm] probe {:02x}:{:02x}.{} failed: {}\n", d.bus, d.dev, d.func, e);
        }
    }
}
