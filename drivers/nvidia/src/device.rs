//! GPU bring-up: the probe/start sequence of Linux `nv_pci_probe` +
//! `nv_start_device` (and nvidia-haiku's Device.cpp), run step by step in the
//! `[nvrm]` kernel thread, with a watchdog thread `[nvrm-wd]`.
//!
//! Every step logs `[nvrm] +<ms> step <n>/<N> <name>: ...` when it starts and
//! `... <name> ok|FAILED (<ms> ms)` when it ends. `nvrm.stop_after=<name>`
//! stops after any step. The log is flushed to the stick before each step
//! that can hang the machine.
//!
//! Safety rails:
//!   - Without `nvrm.start=1` only `rm_init_rm` runs (no device access).
//!   - The watchdog logs a heartbeat while a step runs, and after
//!     `nvrm.timeout` seconds dumps the state, sets ABORT and lets the RM
//!     thread park itself at its next sleepable callback (`abort_point`). If
//!     the thread never gets there, the watchdog turns off bus mastering and
//!     masks the MSI so the GPU stops writing to RAM.
//!   - RM's own bug checks park the RM thread instead of panicking.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use drivers::irq::{self, IrqHandle, IrqReturn};
use drivers::pci::PciFunction;
use sched::workqueue::{Work, WorkQueue};

use crate::glue::{self, NvState, NV_GPU_BAR_INDEX_FB, NV_GPU_BAR_INDEX_IMEM, NV_GPU_BAR_INDEX_REGS};
use crate::{nvlog, rm, Opts, NV_OK};

pub const STEPS: [&str; 12] = [
    "rminit", "handle", "bars", "locks", "supported", "private",
    "root", "firmware", "pcicmd", "msi", "adapter", "done",
];
pub const STEP_LIST: &str = "rminit handle bars locks supported private root firmware pcicmd msi adapter";

const S_RMINIT: usize = 0;
const S_HANDLE: usize = 1;
const S_BARS: usize = 2;
const S_LOCKS: usize = 3;
const S_SUPPORTED: usize = 4;
const S_PRIVATE: usize = 5;
const S_ROOT: usize = 6;
const S_FIRMWARE: usize = 7;
const S_PCICMD: usize = 8;
const S_MSI: usize = 9;
const S_ADAPTER: usize = 10;
const S_DONE: usize = 11;

pub fn step_index(name: &str) -> Option<usize> {
    STEPS.iter().position(|s| *s == name)
}

static STEP: AtomicUsize = AtomicUsize::new(usize::MAX);
static STEP_T0: AtomicU64 = AtomicU64::new(0);
static RM_PID: AtomicU32 = AtomicU32::new(0);
static ABORT: AtomicBool = AtomicBool::new(false);
static PARKED: AtomicBool = AtomicBool::new(false);
static FINISHED: AtomicBool = AtomicBool::new(false);
static ISR_COUNT: AtomicU64 = AtomicU64::new(0);
static ISR_HANDLED: AtomicU64 = AtomicU64::new(0);
static BH_COUNT: AtomicU64 = AtomicU64::new(0);
static LOG_FLUSH: AtomicUsize = AtomicUsize::new(0);
static GPU: spin::Mutex<Option<PciFunction>> = spin::Mutex::new(None);
static MSI: spin::Mutex<Option<IrqHandle>> = spin::Mutex::new(None);
static mut BH_QUEUE: Option<Arc<WorkQueue>> = None;
static mut BH_WORK: Option<&'static Work> = None;

pub(crate) fn set_log_flush(f: fn()) {
    LOG_FLUSH.store(f as usize, Ordering::Release);
}

/// Write the persisted log out now (task context).
pub(crate) fn flush_log() {
    let f = LOG_FLUSH.load(Ordering::Acquire);
    if f != 0 && sched::irqctx::can_sleep() {
        let f: fn() = unsafe { core::mem::transmute(f) };
        f();
    }
}

fn now_ms(t0: u64) -> u64 {
    (sched::monotonic_ns() - t0) / 1_000_000
}

fn enter(i: usize, what: &str) {
    STEP.store(i, Ordering::Release);
    STEP_T0.store(sched::monotonic_ns(), Ordering::Release);
    nvlog!("[nvrm] +{}ms step {}/{} {}: {}\n", crate::ms(), i, S_DONE - 1, STEPS[i], what);
}

fn leave(i: usize, ok: bool, opts: &Opts) -> bool {
    let t = now_ms(STEP_T0.load(Ordering::Acquire));
    nvlog!("[nvrm] +{}ms step {} {} ({} ms)\n", crate::ms(), STEPS[i], if ok { "ok" } else { "FAILED" }, t);
    if !ok {
        return false;
    }
    if opts.stop_after == Some(i) {
        nvlog!("[nvrm] +{}ms stopping after step {} (nvrm.stop_after)\n", crate::ms(), STEPS[i]);
        return false;
    }
    true
}

// ------------------------------------------------------------- abort --

fn park(site: &str) -> ! {
    PARKED.store(true, Ordering::Release);
    nvlog!("[nvrm] +{}ms RM thread (pid {}) PARKED in {}: RM is abandoned for this boot; the system keeps running\n",
           crate::ms(), sched::current_pid(), site);
    flush_log();
    loop {
        sched::kthread::msleep(60_000);
    }
}

/// Called from sleepable callbacks: parks the RM thread once the watchdog
/// has given up on the current step.
pub(crate) fn abort_point(site: &str) {
    if ABORT.load(Ordering::Relaxed)
        && sched::current_pid() == RM_PID.load(Ordering::Relaxed)
        && sched::irqctx::can_sleep()
    {
        park(site);
    }
}

/// RM hit an unrecoverable condition (os_bug_check). Park if this is a
/// sleepable thread; otherwise there is nothing safe left but panic.
pub(crate) fn fatal(site: &str) {
    crate::trace::dump_last();
    if sched::irqctx::can_sleep() && sched::current_pid() != 0 {
        park(site);
    }
    panic!("NVRM fatal error in {} (atomic context)", site);
}

fn quiesce_gpu(why: &str) {
    if let Some(f) = *GPU.lock() {
        let cmd = f.command();
        f.set_bus_master(false);
        nvlog!("[nvrm] +{}ms {}: bus mastering off (COMMAND {:#06x} -> {:#06x})\n", crate::ms(), why, cmd, f.command());
    }
    if let Some(h) = MSI.lock().as_ref() {
        irq::msi_set_masked(h, true);
    }
}

fn dump_state(why: &str) {
    let step = STEP.load(Ordering::Acquire);
    nvlog!("[nvrm] ==== state dump ({}) ====\n", why);
    nvlog!("[nvrm] step={} running {} ms; rm pid {}; isr {} (handled {}), bh {}, work items {}, stubs hit {}, RM log lines {}\n",
           STEPS.get(step).copied().unwrap_or("-"), now_ms(STEP_T0.load(Ordering::Acquire)),
           RM_PID.load(Ordering::Relaxed), ISR_COUNT.load(Ordering::Relaxed), ISR_HANDLED.load(Ordering::Relaxed),
           BH_COUNT.load(Ordering::Relaxed), crate::work::items_run(),
           crate::STUB_HITS.load(Ordering::Relaxed), crate::RM_LINES.load(Ordering::Relaxed));
    let (live, pages, _) = mm::kva::stats();
    nvlog!("[nvrm] sysmem: {} allocations, {} pages live, {} failures; kva: {} mappings, {} pages; buddy free {} MiB\n",
           crate::mem::ALLOCS_LIVE.load(Ordering::Relaxed), crate::mem::PAGES_LIVE.load(Ordering::Relaxed),
           crate::mem::ALLOC_FAILS.load(Ordering::Relaxed), live, pages, mm::buddy::free_pages() / 256);
    if let Some(f) = *GPU.lock() {
        nvlog!("[nvrm] PCI {:02x}:{:02x}.{} COMMAND={:#06x} STATUS={:#06x}\n", f.bus, f.dev, f.func, f.command(), f.read16(0x06));
    }
    crate::trace::dump_last();
    crate::trace::dump_counts();
}

fn watchdog(opts: Opts) {
    let mut last_beat = 0u64;
    let mut abort_at = 0u64;
    loop {
        sched::kthread::msleep(1000);
        if FINISHED.load(Ordering::Acquire) || PARKED.load(Ordering::Acquire) {
            return;
        }
        let step = STEP.load(Ordering::Acquire);
        let el = now_ms(STEP_T0.load(Ordering::Acquire)) / 1000;
        let budget = if step == S_ROOT { opts.root_timeout_s + 30 } else { opts.timeout_s };
        if el >= 10 && el / 10 != last_beat {
            last_beat = el / 10;
            nvlog!("[nvrm] +{}ms wd: step {} running {} s (budget {} s); isr {}, bh {}, stubs {}, RM lines {}\n",
                   crate::ms(), STEPS.get(step).copied().unwrap_or("-"), el, budget,
                   ISR_COUNT.load(Ordering::Relaxed), BH_COUNT.load(Ordering::Relaxed),
                   crate::STUB_HITS.load(Ordering::Relaxed), crate::RM_LINES.load(Ordering::Relaxed));
            if last_beat % 3 == 0 {
                crate::trace::dump_last();
            }
        }
        if !ABORT.load(Ordering::Acquire) && el >= budget {
            dump_state("WATCHDOG: step over budget");
            nvlog!("[nvrm] +{}ms WATCHDOG: aborting RM bring-up in step {} after {} s\n",
                   crate::ms(), STEPS.get(step).copied().unwrap_or("-"), el);
            ABORT.store(true, Ordering::Release);
            abort_at = sched::monotonic_ns();
            flush_log();
        }
        if abort_at != 0 && now_ms(abort_at) > 10_000 {
            nvlog!("[nvrm] +{}ms WATCHDOG: RM thread did not reach an abort point in 10 s (busy loop with no OS call?)\n", crate::ms());
            quiesce_gpu("watchdog");
            dump_state("WATCHDOG: RM thread unresponsive");
            flush_log();
            return;
        }
    }
}

// ----------------------------------------------------------- ISR / BH --

extern "C" fn nv_isr(ctx: usize) -> IrqReturn {
    let n = ISR_COUNT.fetch_add(1, Ordering::Relaxed);
    let mut need_bh = 0u32;
    let handled = unsafe { rm::rm_isr(core::ptr::null_mut(), ctx as *mut NvState, &mut need_bh) };
    if handled {
        ISR_HANDLED.fetch_add(1, Ordering::Relaxed);
    }
    if n < 8 || (n + 1) % 10_000 == 0 {
        nvlog!("[nvrm] +{}ms ISR #{} cpu{} rm_isr={} bottom-half={}\n", crate::ms(), n + 1,
               unsafe { sched::cpu_id() }, handled, need_bh);
    }
    if handled && need_bh != 0 {
        unsafe {
            if let (Some(q), Some(w)) = (&*core::ptr::addr_of!(BH_QUEUE), *core::ptr::addr_of!(BH_WORK)) {
                q.queue(w);
            }
        }
    }
    IrqReturn::Handled
}

extern "C" fn nv_bh(ctx: usize) {
    let n = BH_COUNT.fetch_add(1, Ordering::Relaxed);
    if n < 8 {
        nvlog!("[nvrm] +{}ms bottom half #{} (rm_isr_bh)\n", crate::ms(), n + 1);
    }
    unsafe { rm::rm_isr_bh(core::ptr::null_mut(), ctx as *mut NvState) };
}

// --------------------------------------------------------------- helpers --

fn cstr(s: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len() + 1);
    v.extend_from_slice(s.as_bytes());
    v.push(0);
    v
}

/// `nvrm.reg=Name:val,Name:val` -> global RM registry dwords.
fn apply_registry_overrides() {
    let Some(spec) = crate::opt_str("nvrm.reg") else { return };
    for item in spec.split(',') {
        let Some((name, val)) = item.split_once(':') else {
            nvlog!("[nvrm] nvrm.reg: bad item '{}' (want Name:value)\n", item);
            continue;
        };
        let Some(v) = crate::parse_num(val) else {
            nvlog!("[nvrm] nvrm.reg: bad value in '{}'\n", item);
            continue;
        };
        let n = cstr(name);
        let st = unsafe { rm::rm_write_registry_dword(core::ptr::null_mut(), core::ptr::null_mut(), n.as_ptr() as _, v as u32) };
        nvlog!("[nvrm] registry {} = {:#x} (status {:#x})\n", name, v, st);
    }
}

fn read_reg(nv: *mut NvState, name: &str) -> Option<u32> {
    let n = cstr(name);
    let mut v = 0u32;
    let st = unsafe { rm::rm_read_registry_dword(core::ptr::null_mut(), nv, n.as_ptr() as _, &mut v) };
    if st == NV_OK { Some(v) } else { None }
}

fn log_config(f: &PciFunction) {
    nvlog!("[nvrm]   vendor {:04x} device {:04x} rev {:02x} class {:06x} subsys {:04x}:{:04x} COMMAND {:#06x} STATUS {:#06x}\n",
           f.vendor_id(), f.device_id(), f.read8(0x08), f.read32(0x08) >> 8, f.read16(0x2C), f.read16(0x2E),
           f.command(), f.read16(0x06));
    let caps: Vec<_> = f.capabilities();
    let mut s = alloc::string::String::new();
    for (id, off) in &caps {
        s.push_str(&alloc::format!(" {:02x}@{:#x}", id, off));
    }
    nvlog!("[nvrm]   capabilities:{}\n", s);
    let mut s = alloc::string::String::new();
    for (id, ver, off) in f.ext_capabilities() {
        s.push_str(&alloc::format!(" {:04x}v{}@{:#x}", id, ver, off));
    }
    nvlog!("[nvrm]   extended capabilities:{}\n", if s.is_empty() { " (none / no ECAM)".into() } else { s });
    if let Some(m) = f.find_capability(drivers::pci::cap::MSI) {
        let c = f.read16(m + 2);
        nvlog!("[nvrm]   MSI control {:#06x}: enabled={} 64-bit={} per-vector-mask={} multi-cap={}\n",
               c, c & 1, (c >> 7) & 1, (c >> 8) & 1, 1 << ((c >> 1) & 7));
    }
    if let Some(m) = f.find_capability(drivers::pci::cap::MSIX) {
        let c = f.read16(m + 2);
        nvlog!("[nvrm]   MSI-X control {:#06x}: enabled={} table size {}\n", c, c >> 15, (c & 0x7FF) + 1);
    }
}

/// Which GSP image RM will ask for, from the PCI device id (RM itself picks
/// by PMC_BOOT_42; this is only for the early preload/check).
fn gsp_image_for(device_id: u16) -> &'static str {
    match device_id >> 8 {
        0x1E | 0x1F | 0x21 | 0x20 => "gsp_tu10x.bin",
        _ => "gsp_ga10x.bin",
    }
}

/// nvrm.fakegpu: stand-in for rm_init_adapter. Without nvrm.fakehang it
/// returns false ("RM would fail here"); with it, it hangs the way a stuck
/// GSP boot would, so the watchdog path can be rehearsed in QEMU.
fn rehearse_adapter(opts: &Opts) -> bool {
    nvlog!("[nvrm]   rehearsal: rm_init_adapter not called (fakehang={})\n", opts.fakehang);
    match opts.fakehang {
        1 => loop {
            // _kgspRpcRecvPoll: gpuCheckTimeout -> os_get_current_tick, nothing else.
            unsafe { crate::time::os_get_current_tick() };
            core::hint::spin_loop();
        },
        2 => loop {
            core::hint::spin_loop();
        },
        _ => false,
    }
}

// ------------------------------------------------------------- the thread --

pub(crate) fn spawn(opts: Opts, gpu: Option<PciFunction>) {
    if sched::kthread::spawn("nvrm", move || main(opts, gpu)).is_none() {
        nvlog!("[nvrm] could not start the [nvrm] kernel thread\n");
    }
}

fn main(opts: Opts, gpu: Option<PciFunction>) {
    crate::T0.store(sched::monotonic_ns(), Ordering::Relaxed);
    RM_PID.store(sched::current_pid(), Ordering::Relaxed);
    nvlog!("[nvrm] +0ms [nvrm] thread pid {} on cpu{}\n", sched::current_pid(), unsafe { sched::cpu_id() });
    if !crate::work::init() {
        nvlog!("[nvrm] could not create the nvrm-q work queue (using the system queue)\n");
    }

    enter(S_RMINIT, "rm_init_rm (RM core init, no device access)");
    let ok = unsafe { rm::rm_init_rm(core::ptr::null_mut()) };
    if !leave(S_RMINIT, ok, &opts) {
        finish("rm_init_rm");
        return;
    }
    apply_registry_overrides();

    if let Some((v, d)) = opts.testdev {
        crate::edutest::run(v, d);
        finish("testdev");
        return;
    }
    let Some(f) = gpu else {
        finish("no GPU");
        return;
    };
    if !opts.start {
        nvlog!("[nvrm] +{}ms nvrm.start not set: GPU {:02x}:{:02x}.{} left untouched (add nvrm.start=1 to the kernel cmdline)\n",
               crate::ms(), f.bus, f.dev, f.func);
        finish("detection only");
        return;
    }
    *GPU.lock() = Some(f);
    if sched::kthread::spawn("nvrm-wd", move || watchdog(opts)).is_none() {
        nvlog!("[nvrm] WARNING: no watchdog thread\n");
    }
    let r = start(&opts, f);
    finish(r);
}

fn finish(how: &str) {
    STEP.store(S_DONE, Ordering::Release);
    FINISHED.store(true, Ordering::Release);
    nvlog!("[nvrm] +{}ms ==== bring-up finished: {} ====\n", crate::ms(), how);
    crate::trace::dump_counts();
    nvlog!("[nvrm] stubs hit {}, RM log lines {}, isr {}, bh {}\n",
           crate::STUB_HITS.load(Ordering::Relaxed), crate::RM_LINES.load(Ordering::Relaxed),
           ISR_COUNT.load(Ordering::Relaxed), BH_COUNT.load(Ordering::Relaxed));
    flush_log();
}

/// Steps handle .. adapter. Returns the outcome for the final line.
fn start(opts: &Opts, f: PciFunction) -> &'static str {
    // ---- handle
    enter(S_HANDLE, "os_pci_init_handle + config space");
    let handle = unsafe { crate::pcios::os_pci_init_handle(0, f.bus, f.dev, f.func, core::ptr::null_mut(), core::ptr::null_mut()) };
    if handle.is_null() {
        leave(S_HANDLE, false, opts);
        return "no PCI handle";
    }
    log_config(&f);
    if !leave(S_HANDLE, true, opts) {
        return "stopped";
    }

    // ---- bars + nv_state_t
    enter(S_BARS, "BAR sizing (decode off for microseconds) + nv_state_t");
    flush_log();
    let sizing = f.size_bars();
    let mem_bars: Vec<_> = sizing.bars.iter().filter(|b| !b.io && b.size != 0).copied().collect();
    for b in &sizing.bars {
        nvlog!("[nvrm]   BAR{} {} {}{} base {:#x} size {:#x} ({} MiB){}\n", b.index,
               if b.io { "io" } else { "mem" }, if b.is64 { "64" } else { "32" },
               if b.prefetchable { " pref" } else { "" }, b.base, b.size, b.size >> 20,
               if b.base == 0 { " UNASSIGNED" } else { "" });
    }
    let size = unsafe { glue::lnv_state_size() } as usize;
    let layout = match core::alloc::Layout::from_size_align(size, 16) {
        Ok(l) => l,
        Err(_) => return "layout",
    };
    let nv = unsafe { alloc::alloc::alloc_zeroed(layout) } as *mut NvState;
    if nv.is_null() {
        leave(S_BARS, false, opts);
        return "nv_state_t allocation failed";
    }
    unsafe {
        glue::lnv_state_init(nv, core::ptr::null_mut(), handle, 0, f.bus, f.dev, f.func,
                             f.vendor_id(), f.device_id(), f.read16(0x2C), f.read16(0x2E), 0);
    }
    // nv-pci.c: memory BARs in order -> REGS, FB, IMEM.
    let slots = [NV_GPU_BAR_INDEX_REGS, NV_GPU_BAR_INDEX_FB, NV_GPU_BAR_INDEX_IMEM];
    for (slot, b) in slots.iter().zip(mem_bars.iter()) {
        unsafe { glue::lnv_state_set_bar(nv, *slot, b.index as u32, b.base, b.size) };
    }
    if mem_bars.len() < 3 || mem_bars.iter().take(3).any(|b| b.base == 0) {
        nvlog!("[nvrm]   WARNING: expected 3 assigned memory BARs (regs, fb, imem); got {}\n", mem_bars.len());
    }
    let (fb, fw, fh, fp) = crate::platform::boot_fb();
    match mem_bars.get(1) {
        Some(b1) if fb >= b1.base && fb < b1.base + b1.size => {
            let off = fb - b1.base;
            nvlog!("[nvrm]   GOP framebuffer {:#x} ({}x{}, pitch {}) = BAR1+{:#x}: {}\n", fb, fw, fh, fp, off,
                   if off == 0 { "at BAR1 base, RM keeps a BAR1 console mapping when BAR1 goes virtual (console should survive)" }
                   else { "NOT at BAR1 base: RM may not preserve it; the screen may go dark after GSP init (the stick log keeps everything)" });
        }
        _ => nvlog!("[nvrm]   GOP framebuffer {:#x} is not in BAR1 (console is not on this GPU, or BARs moved)\n", fb),
    }
    if !leave(S_BARS, true, opts) {
        return "stopped";
    }

    // ---- locks
    enter(S_LOCKS, "rm_init_event_locks");
    let ok = unsafe { rm::rm_init_event_locks(core::ptr::null_mut(), nv) };
    if !leave(S_LOCKS, ok, opts) {
        return if ok { "stopped" } else { "rm_init_event_locks failed" };
    }

    let fake = f.vendor_id() != crate::PCI_VENDOR_NVIDIA;
    // ---- supported
    enter(S_SUPPORTED, "rm_is_supported_device (reads PMC_BOOT_0/42 through BAR0)");
    flush_log();
    let st = if fake {
        nvlog!("[nvrm]   rehearsal (not an NVIDIA function): rm_is_supported_device skipped\n");
        NV_OK
    } else {
        unsafe { rm::rm_is_supported_device(core::ptr::null_mut(), nv) }
    };
    nvlog!("[nvrm]   rm_is_supported_device -> {:#x}\n", st);
    if !leave(S_SUPPORTED, st == NV_OK, opts) {
        return if st == NV_OK { "stopped" } else { "GPU not supported by RM" };
    }

    // ---- private state
    enter(S_PRIVATE, "rm_init_private_state + rm_set_rm_firmware_requested");
    let ok = fake || unsafe { rm::rm_init_private_state(core::ptr::null_mut(), nv) };
    if ok && !fake {
        unsafe { rm::rm_set_rm_firmware_requested(core::ptr::null_mut(), nv) };
        unsafe { glue::lnv_state_set_queue(nv, crate::work::device_queue_cookie()) };
    }
    if !leave(S_PRIVATE, ok, opts) {
        return if ok { "stopped" } else { "rm_init_private_state failed" };
    }

    // ---- root
    enter(S_ROOT, "waiting for the real root filesystem (firmware lives there)");
    let deadline = sched::monotonic_ns() + opts.root_timeout_s * 1_000_000_000;
    let mut next_note = sched::monotonic_ns() + 10_000_000_000;
    while !drivers::firmware::root_ready() {
        if sched::monotonic_ns() > deadline {
            nvlog!("[nvrm]   root not mounted after {} s (nvrm.roottimeout)\n", opts.root_timeout_s);
            leave(S_ROOT, false, opts);
            return "no root filesystem";
        }
        if sched::monotonic_ns() > next_note {
            nvlog!("[nvrm]   still waiting for the root filesystem...\n");
            next_note += 10_000_000_000;
        }
        sched::kthread::msleep(100);
    }
    if !leave(S_ROOT, true, opts) {
        return "stopped";
    }

    // ---- firmware
    let img = gsp_image_for(f.device_id());
    enter(S_FIRMWARE, "preload the GSP image");
    let ok = crate::fw::preload(img);
    if !leave(S_FIRMWARE, ok, opts) {
        return if ok { "stopped" } else { "GSP firmware missing (see scripts/nvidia-test-stick.sh)" };
    }

    // ---- PCI command
    enter(S_PCICMD, "enable memory + I/O decode and bus mastering");
    flush_log();
    let before = f.command();
    f.set_command(before | drivers::pci::CMD_MEMORY | drivers::pci::CMD_IO | drivers::pci::CMD_BUS_MASTER);
    nvlog!("[nvrm]   COMMAND {:#06x} -> {:#06x}\n", before, f.command());
    if !leave(S_PCICMD, true, opts) {
        return "stopped";
    }

    // ---- MSI + ISR + bottom half
    enter(S_MSI, "MSI vector + ISR + bottom-half queue");
    unsafe {
        if let Some(q) = WorkQueue::new("nvrm-bh") {
            BH_QUEUE = Some(q);
        }
        BH_WORK = Some(Box::leak(Box::new(Work::new(nv_bh, nv as usize))));
    }
    let enable_msi = read_reg(nv, "EnableMSI");
    nvlog!("[nvrm]   registry EnableMSI = {:?}\n", enable_msi);
    let mut msi_ok = false;
    if opts.nomsi {
        nvlog!("[nvrm]   nvrm.nomsi=1: no interrupt source (RM will poll; bisection mode)\n");
        msi_ok = true;
    } else if enable_msi.unwrap_or(1) == 1 {
        match irq::request_msi(f, nv_isr, nv as usize) {
            Ok(h) => {
                let v = h.vector().unwrap_or(0);
                unsafe {
                    glue::lnv_state_or_flags(nv, glue::NV_FLAG_USES_MSI);
                    glue::lnv_state_set_interrupt_line(nv, v as u32);
                }
                nvlog!("[nvrm]   MSI enabled: vector {:#x} -> BSP; COMMAND {:#06x}\n", v, f.command());
                *MSI.lock() = Some(h);
                msi_ok = true;
            }
            Err(e) => nvlog!("[nvrm]   request_msi failed: {:?} (no INTx routing on x86_64)\n", e),
        }
    } else {
        nvlog!("[nvrm]   EnableMSI=0 and INTx routing is unavailable\n");
    }
    if !leave(S_MSI, msi_ok, opts) {
        return if msi_ok { "stopped" } else { "no interrupt source" };
    }

    // ---- rm_init_adapter (GSP boot)
    enter(S_ADAPTER, "rm_init_adapter: VBIOS/FWSEC, firmware, WPR/radix3, booter, GSP-RM INIT_DONE");
    nvlog!("[nvrm]   (if the screen stops updating now, the stick log has the rest)\n");
    flush_log();
    let t0 = sched::monotonic_ns();
    let ok = if fake {
        rehearse_adapter(opts)
    } else {
        unsafe { rm::rm_init_adapter(core::ptr::null_mut(), nv) }
    };
    let ms = now_ms(t0);
    if ok {
        nvlog!("[nvrm] +{}ms ==== rm_init_adapter OK in {} ms: GSP-RM is up (INIT_DONE) ====\n", crate::ms(), ms);
        let name = unsafe { rm::rm_get_device_name(f.device_id(), f.read16(0x2C), f.read16(0x2E)) };
        if !name.is_null() {
            let s = unsafe { core::ffi::CStr::from_ptr(name) }.to_str().unwrap_or("?");
            nvlog!("[nvrm]   device name: {}\n", s);
        }
        let (b1, l1) = {
            let (mut a, mut b) = (0u64, 0u64);
            unsafe { glue::lnv_state_bar(nv, NV_GPU_BAR_INDEX_FB, &mut a, &mut b) };
            (a, b)
        };
        nvlog!("[nvrm]   BAR1 {:#x} size {:#x}; if you can read this line on screen, the GOP console survived GSP init\n", b1, l1);
    } else {
        nvlog!("[nvrm] +{}ms ==== rm_init_adapter FAILED after {} ms ====\n", crate::ms(), ms);
        crate::trace::dump_last();
        if let Some(h) = MSI.lock().take() {
            irq::free_irq(h);
        }
    }
    leave(S_ADAPTER, ok, opts);
    if ok { "rm_init_adapter OK" } else { "rm_init_adapter failed" }
}
