//! LeandrOS OS-interface shim for NVIDIA's open-gpu-kernel-modules resource
//! manager (OpenRM), the same split X547's nvidia-haiku port uses: NVIDIA's
//! OS-agnostic RM (`nv-kernel.o`, here archived as `libnvrm.a` by
//! ports/openrm/build.sh) is linked into the kernel, and this crate supplies
//! the `os_*` / `nv_*` callbacks Linux's kernel-open/ layer would.
//!
//! Layout:
//!   - ports/openrm/glue/lnv_glue.c (inside libnvrm.a): data symbols, the
//!     registry table (+ nvidia-haiku's registry tweak), C-variadic printf
//!     family, nv_state_t accessors, callbacks that only touch RM structs.
//!   - `basic`:    heap, libc-like memory/string helpers, debug hooks.
//!   - `sync`:     spinlocks, sleeping mutex/rwlock/semaphore, wait queues,
//!                 context queries (sched::ksync / sched::irqctx).
//!   - `time`:     clocks, delays, nano timers, RC timer.
//!   - `mem`:      nv_alloc_pages & co (buddy), kernel mappings (mm::kva).
//!   - `pcios`:    os_pci_* on a PciFunction (full 4 KiB config), port I/O.
//!   - `work`:     os_queue_work_item / os_flush_work_queue (sched::workqueue).
//!   - `fw`:       nv_get_firmware (drivers::firmware::request_firmware).
//!   - `platform`: the constant answers, process identity, caps, ACPI, I2C,
//!                 NVLink libc wrappers.
//!   - `device`:   probe/start sequence, ISR + bottom half, watchdog.
//!   - `client`:   the in-kernel RM client API (rm_kernel_rmapi_op through
//!                 ports/openrm/glue/lnv_rmapi.c): alloc/control/free/map.
//!   - `params`:   RM classes, controls and parameter blocks, checked
//!                 against NVIDIA's headers at run time (`client::abi_check`).
//!   - `selftest`: `nvrm.selftest=ce`, real copy-engine work on the GPU.
//!   - `edutest`:  exercises the callbacks against QEMU's `edu` device.
//!   - `trace`:    first-use logging + call counters for every callback.
//!   - `stubs`     (generated): every other OS callback, logging + failing.
//!   - `rm_exports` (generated): keeps every RM entry point live in the link.
//!
//! Kernel command line (`nvrm.*`), see `Opts`.
//!
//! Licensing: OpenRM is dual MIT/GPLv2; LeandrOS uses it under MIT.

#![no_std]
#![allow(clippy::missing_safety_doc)]

extern crate alloc;

use core::ffi::{c_char, CStr};
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

#[macro_use]
mod trace;
mod basic;
pub mod client;
mod device;
mod edutest;
mod fw;
mod mem;
pub mod params;
mod pcios;
mod platform;
mod selftest;
mod sync;
mod time;
mod uapi;
mod uapi_abi;
mod uapi_kms;
mod uapi_vm;
mod work;
pub mod glue;
pub mod rm;
pub mod rm_exports;
pub mod stubs;

/// NV_STATUS values (src/common/sdk/nvidia/inc/nvstatuscodes.h).
pub const NV_OK: u32 = 0x0000_0000;
pub const NV_ERR_ILLEGAL_ACTION: u32 = 0x0000_0016;
pub const NV_ERR_INVALID_ADDRESS: u32 = 0x0000_001E;
pub const NV_ERR_INVALID_ARGUMENT: u32 = 0x0000_001F;
pub const NV_ERR_INVALID_REQUEST: u32 = 0x0000_003F;
pub const NV_ERR_NO_MEMORY: u32 = 0x0000_0051;
pub const NV_ERR_NOT_READY: u32 = 0x0000_0055;
pub const NV_ERR_NOT_SUPPORTED: u32 = 0x0000_0056;
pub const NV_ERR_OBJECT_NOT_FOUND: u32 = 0x0000_0057;
pub const NV_ERR_OPERATING_SYSTEM: u32 = 0x0000_0059;
pub const NV_ERR_TIMEOUT_RETRY: u32 = 0x0000_0066;
/// NvlStatus (src/common/nvlink/interface/nvlink_errors.h); returned negated.
pub const NVL_ERR_NOT_SUPPORTED: i32 = 13;

pub const PCI_VENDOR_NVIDIA: u16 = 0x10DE;

// ---------------------------------------------------------------- logging --

struct Serial;

impl Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        drivers::pci::serial_debug(s);
        Ok(())
    }
}

/// Diagnostic print: UART + the persisted klog ring (the stick's boot log),
/// NOT the framebuffer; callable from any context, including the RM ISR.
pub fn log(args: fmt::Arguments) {
    let _ = Serial.write_fmt(args);
}

macro_rules! nvlog {
    ($($t:tt)*) => { $crate::log(format_args!($($t)*)) };
}
pub(crate) use nvlog;

extern "C" {
    /// Kernel whole-string console print: UART + klog + the framebuffer
    /// console (VT 1), staged line-atomically.
    fn serial_print_str_raw(s: *const u8, len: usize);
}

/// A milestone line that should also be readable on the PC's monitor: the
/// WHOLE line goes through the console sink in one call (UART + klog +
/// framebuffer), never split between the UART-only path and the screen —
/// the "phantom digits" of the first PC boot were exactly such a split.
/// Process context only (allocates); for a handful of lines, not ioctl logs.
pub fn log_screen(args: fmt::Arguments) {
    let s = alloc::fmt::format(args);
    unsafe { serial_print_str_raw(s.as_ptr(), s.len()) };
}

macro_rules! nvscreen {
    ($($t:tt)*) => { $crate::log_screen(format_args!($($t)*)) };
}
pub(crate) use nvscreen;

/// Milliseconds since the RM bring-up thread started (0 before), for log
/// lines: every step and first-use line carries it.
pub(crate) fn ms() -> u64 {
    let t0 = T0.load(Ordering::Relaxed);
    if t0 == 0 { 0 } else { sched::monotonic_ns().saturating_sub(t0) / 1_000_000 }
}
static T0: AtomicU64 = AtomicU64::new(0);

/// NV_DBG_* levels (os-interface.h): INFO 0, SETUP 1, USERERRORS 2,
/// WARNINGS 3, ERRORS 4. RM messages below this are dropped.
static LOG_MIN_LEVEL: AtomicU32 = AtomicU32::new(1);
static RM_LINES: AtomicU64 = AtomicU64::new(0);
/// RM messages naming an Xid (RM's channel/engine error reports), counted
/// whatever the log level.
static XID_LINES: AtomicU64 = AtomicU64::new(0);

pub(crate) fn xid_lines() -> u64 {
    XID_LINES.load(Ordering::Relaxed)
}

pub(crate) fn stub_hits() -> u32 {
    STUB_HITS.load(Ordering::Relaxed)
}

/// Sink for every RM message (nv_printf, nv_log_error, nvswitch_os_print),
/// already formatted by the C glue with RM's own nvDbgVsnprintf.
#[no_mangle]
pub unsafe extern "C" fn lnv_log_str(level: u32, msg: *const c_char) {
    if msg.is_null() {
        return;
    }
    let s = CStr::from_ptr(msg).to_bytes();
    if s.windows(3).any(|w| w == b"Xid") {
        XID_LINES.fetch_add(1, Ordering::Relaxed);
    }
    if level < LOG_MIN_LEVEL.load(Ordering::Relaxed) {
        return;
    }
    RM_LINES.fetch_add(1, Ordering::Relaxed);
    drivers::pci::serial_debug(core::str::from_utf8(s).unwrap_or("<non-utf8 RM message>\n"));
}

static STUB_LOG_BUDGET: AtomicU32 = AtomicU32::new(256);
static STUB_HITS: AtomicU32 = AtomicU32::new(0);

/// Called by every generated stub: name the unimplemented callback RM just
/// reached (first 256 calls, so a hot path cannot flood the console).
pub fn stub_called(name: &str) {
    STUB_HITS.fetch_add(1, Ordering::Relaxed);
    if STUB_LOG_BUDGET
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
        .is_ok()
    {
        nvlog!("[nvrm] +{}ms STUB (unimplemented OS callback, returns failure): {}\n", ms(), name);
    }
}

// ---------------------------------------------------------------- options --

/// What the boot path hands over (kernel/src/init.rs, one cfg'd line).
pub struct BootEnv<'a> {
    pub cmdline: &'a str,
    /// Limine framebuffer (the GOP console): physical base, size, pitch.
    pub fb_phys: u64,
    pub fb_width: u32,
    pub fb_height: u32,
    pub fb_pitch: u32,
    /// Write the persisted log out now (klog::flush_now): called before
    /// every step that can hard-hang the machine.
    pub log_flush: fn(),
    /// Physical address of the ACPI RSDP (Limine), 0 if none: RM finds the
    /// MCFG table, and with it PCIe extended config space, from it.
    pub rsdp_phys: u64,
}

/// `nvrm.*` options:
///
/// | option | meaning |
/// |---|---|
/// | `nvrm.start=1` | run the full bring-up (register writes, MSI, `rm_init_adapter`). Default: `rm_init_rm` + detection log only |
/// | `nvrm.stop_after=<step>` | stop after the named step (see `device::STEPS`) |
/// | `nvrm.timeout=<s>` | watchdog budget per step (default 90) |
/// | `nvrm.roottimeout=<s>` | how long to wait for the real root (firmware) (default 180) |
/// | `nvrm.loglevel=<n>` | minimum RM message level (0 INFO .. 4 ERRORS); default 0 with start=1, else 1 |
/// | `nvrm.reg=Name:val[,Name:val]` | extra RM registry dwords before init (val hex with 0x) |
/// | `nvrm.nomsi=1` | do not set up MSI (expect "no interrupts" failure; bisection aid) |
/// | `nvrm.testdev=VVVV:DDDD` | callback self-test against that PCI function (QEMU `edu` = 1234:11e8) |
/// | `nvrm.fakegpu=VVVV:DDDD` | QEMU rehearsal: the full start sequence on that function (implies start; skips the NVIDIA-only RM calls) |
/// | `nvrm.fakehang=1\|2` | with fakegpu: hang in the adapter step (1 polling the clock, 2 without OS calls) to exercise the watchdog |
/// | `nvrm.selftest=ce\|abi` | after `rm_init_adapter`: the in-kernel RM client self-test (`selftest`); `abi` also runs without a GPU |
/// | `nvrm.selftest_stop=<step>`, `nvrm.selftest_skip=<step>[,...]`, `nvrm.selftest_hold=<s>`, `nvrm.selftest_loops=<n>`, `nvrm.selftest_fbphys=<hex>` | self-test controls (see `selftest`) |
/// | `nvrm.uapi=0` | do not register the nouveau UAPI (`uapi`) after a successful bring-up |
/// | `nvrm.uapi_log=<n>` | nouveau-UAPI log level: 0 errors/summaries, 1 every ioctl (default), 2 with arguments |
/// | `nvrm.uapi_timeout=<s>` | a channel making no progress this long is killed (default 10) |
/// | `nvrm.nomcfg=1` | do not give RM the ACPI RSDP (RM then cannot reach the upstream bridge's config space; the pre-nvclient behaviour) |
///
/// A key given twice takes the last value, so `--cmdline` additions on the
/// test stick override its defaults.
#[derive(Clone, Copy)]
pub(crate) struct Opts {
    pub start: bool,
    pub stop_after: Option<usize>,
    pub timeout_s: u64,
    pub root_timeout_s: u64,
    pub nomsi: bool,
    pub testdev: Option<(u16, u16)>,
    /// QEMU rehearsal: run the start sequence on a non-NVIDIA function.
    pub fakegpu: Option<(u16, u16)>,
    /// Rehearsal of a hang in the adapter step: 1 = RM-like poll loop on
    /// os_get_current_tick, 2 = a loop that never calls the OS.
    pub fakehang: u32,
    /// `nvrm.selftest`: what to run after rm_init_adapter.
    pub selftest: selftest::Mode,
}

static mut CMDLINE: [u8; 1024] = [0; 1024];
static mut CMDLINE_LEN: usize = 0;

pub(crate) fn cmdline() -> &'static str {
    unsafe {
        let buf = &*core::ptr::addr_of!(CMDLINE);
        core::str::from_utf8(&buf[..CMDLINE_LEN]).unwrap_or("")
    }
}

/// Value of `key=` on the command line; the last occurrence wins.
pub(crate) fn opt_str(key: &str) -> Option<&'static str> {
    let mut found = None;
    for tok in cmdline().split_ascii_whitespace() {
        if let Some(v) = tok.strip_prefix(key).and_then(|r| r.strip_prefix('=')) {
            found = Some(v);
        }
    }
    found
}

pub(crate) fn parse_num(v: &str) -> Option<u64> {
    if let Some(h) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        u64::from_str_radix(h, 16).ok()
    } else {
        v.parse().ok()
    }
}

fn opt_num(key: &str) -> Option<u64> {
    opt_str(key).and_then(parse_num)
}

fn parse_opts() -> Opts {
    let stop_after = opt_str("nvrm.stop_after").map(|s| match device::step_index(s) {
        Some(i) => i,
        None => {
            nvlog!("[nvrm] unknown nvrm.stop_after={} (steps: {}); ignoring\n", s, device::STEP_LIST);
            usize::MAX
        }
    }).filter(|&i| i != usize::MAX);
    let ids = |key: &str| opt_str(key).and_then(|s| {
        let (v, d) = s.split_once(':')?;
        Some((u16::from_str_radix(v.trim_start_matches("0x"), 16).ok()?,
              u16::from_str_radix(d.trim_start_matches("0x"), 16).ok()?))
    });
    let testdev = ids("nvrm.testdev");
    let fakegpu = ids("nvrm.fakegpu");
    Opts {
        start: opt_num("nvrm.start").unwrap_or(0) != 0,
        stop_after,
        timeout_s: opt_num("nvrm.timeout").unwrap_or(90).max(5),
        root_timeout_s: opt_num("nvrm.roottimeout").unwrap_or(180).max(5),
        nomsi: opt_num("nvrm.nomsi").unwrap_or(0) != 0,
        testdev,
        fakegpu,
        fakehang: opt_num("nvrm.fakehang").unwrap_or(0) as u32,
        selftest: selftest::mode_from_cmdline(),
    }
}

// ------------------------------------------------------------------ entry --

/// Boot-time entry (init task, boot context, before userspace exists).
///
/// - No NVIDIA display controller and no `nvrm.testdev`: one log line, RM is
///   never entered (unless built with LEANDROS_NVRM_SELFTEST=1, or with
///   `nvrm.selftest=abi`, which runs the device-independent RM client checks
///   in the `[nvrm]` thread).
/// - Otherwise a kernel thread `[nvrm]` runs `rm_init_rm` and, with
///   `nvrm.start=1`, the whole probe/start sequence (see `device`).
pub fn init(env: &BootEnv) {
    let n = env.cmdline.len().min(1023);
    unsafe {
        let buf = &mut *core::ptr::addr_of_mut!(CMDLINE);
        buf[..n].copy_from_slice(&env.cmdline.as_bytes()[..n]);
        CMDLINE_LEN = n;
    }
    let opts = parse_opts();
    let level = opt_num("nvrm.loglevel").unwrap_or(if opts.start { 0 } else { 1 });
    LOG_MIN_LEVEL.store(level as u32, Ordering::Relaxed);
    platform::set_boot_fb(env.fb_phys, env.fb_width, env.fb_height, env.fb_pitch);
    platform::set_rsdp(if opt_num("nvrm.nomcfg").unwrap_or(0) != 0 { 0 } else { env.rsdp_phys });
    device::set_log_flush(env.log_flush);

    let mut opts = opts;
    let mut gpus: alloc::vec::Vec<_> = drivers::pci::scan()
        .into_iter()
        .filter(|d| d.vendor_id == PCI_VENDOR_NVIDIA && d.class == 0x03)
        .collect();
    if let Some((v, d)) = opts.fakegpu {
        if let Some(dev) = drivers::pci::scan().into_iter().find(|p| p.vendor_id == v && p.device_id == d) {
            nvlog!("[nvrm] nvrm.fakegpu: rehearsing the start sequence on {:04x}:{:04x}\n", v, d);
            gpus.insert(0, dev);
            opts.start = true;
        }
    }
    for d in &gpus {
        nvlog!(
            "[nvrm] found {:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}{:02x}\n",
            d.bus, d.dev, d.func, d.vendor_id, d.device_id, d.class, d.subclass
        );
    }
    if gpus.is_empty() && opts.testdev.is_none() {
        nvlog!("[nvrm] OpenRM 570.86.16 linked; no NVIDIA display controller present{}\n",
               if opts.start { " (nvrm.start=1: nothing to start)" } else { "" });
        match opts.selftest {
            selftest::Mode::Ce => nvlog!("[nvrm] nvrm.selftest=ce: no NVIDIA GPU, self-test skipped\n"),
            selftest::Mode::Abi => {
                nvlog!("[nvrm] nvrm.selftest=abi: running the device-independent RM client checks\n");
                *PENDING.lock() = Some((opts, None));
            }
            selftest::Mode::None => {}
        }
        if option_env!("LEANDROS_NVRM_SELFTEST").is_some() {
            let ok = unsafe { rm::rm_init_rm(core::ptr::null_mut()) };
            nvlog!("[nvrm] selftest: rm_init_rm returned {}\n", ok);
        }
        return;
    }
    nvlog!(
        "[nvrm] OpenRM 570.86.16: {} GPU(s); nvrm.start={} stop_after={} timeout={}s roottimeout={}s loglevel={} nomsi={}{}\n",
        gpus.len(), opts.start as u32,
        opts.stop_after.map_or("-", |i| device::STEPS[i]),
        opts.timeout_s, opts.root_timeout_s, level, opts.nomsi as u32,
        if opts.start { "" } else { " (default: rm_init_rm + detection only; add nvrm.start=1 to bring the GPU up)" }
    );
    let gpu = gpus.first().map(|d| d.function());
    *PENDING.lock() = Some((opts, gpu));
}

static PENDING: spin::Mutex<Option<(Opts, Option<drivers::pci::PciFunction>)>> = spin::Mutex::new(None);

/// Second boot hook, after userspace init has been spawned (so the `[nvrm]`
/// kernel thread does not shift init's pid) and after the system work queue
/// and the request_firmware backend exist.
pub fn start_thread() {
    if let Some((opts, gpu)) = PENDING.lock().take() {
        if gpu.is_none() && opts.testdev.is_none() && opts.selftest == selftest::Mode::Abi {
            device::spawn_abi_selftest();
        } else {
            device::spawn(opts, gpu);
        }
    }
}
