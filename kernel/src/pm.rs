//! System sleep framework (Linux kernel/power/suspend.c + drivers/base/power).
//!
//! Step 1 of suspend-to-RAM: the device half only. A *freeze-only cycle*
//! (Linux `pm_test=devices`) runs every device's suspend hook in suspend
//! order, holds the machine quiesced for a moment, then runs the resume
//! hooks in the reverse order — no platform sleep, no CPU state loss. It
//! proves the hooks are reversible on the real hardware before any ACPI S3
//! entry is attempted (see docs in artifacts/notes/lane-lapsusp-2026-10-27.md).
//!
//! Order (children before parents, consumers before providers, as Linux's
//! dpm_list does it in reverse probe order): input (i8042), display (i915
//! planes), network (e1000e), block layer (gate + drain + cache flush, covers
//! NVMe, USB storage and virtio-blk), and last the USB host controllers. A
//! failing suspend hook aborts the cycle: the hooks already run are resumed
//! in reverse, like `dpm_suspend`'s error path.
//!
//! Trigger: reboot(2) with the private command `LEANDROS_REBOOT_CMD_PM_TEST`
//! and `arg` pointing at a `PmTestReq` (flags, hold time, report buffer);
//! root only. `userland/suspendtest` drives it.
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

/// `PmTestReq.flags`: really halt the xHCI controllers (USBCMD.R/S = 0)
/// instead of only masking their interrupts. Off by default: on the laptop
/// the root file system is on a USB stick.
pub const PM_XHCI_HALT: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PmTestReq {
    pub flags: u32,
    pub hold_ms: u32,
    pub buf: u64,
    pub buf_len: u64,
}

type Hook = fn(u32) -> Result<String, String>;

struct PmDev { name: &'static str, suspend: Hook, resume: Hook }

fn none(_: u32) -> Result<String, String> { Ok(String::from("not present")) }

#[cfg(target_arch = "x86_64")]
fn ps2_suspend(_: u32) -> Result<String, String> {
    match arch_x86_64::keyboard::pm_suspend() {
        Ok(None) => Ok(String::from("no i8042")),
        Ok(Some(c)) => Ok(alloc::format!("keyboard + AUX ports disabled (CTR {:#04x})", c)),
        Err(e) => Err(String::from(e)),
    }
}
#[cfg(target_arch = "x86_64")]
fn ps2_resume(_: u32) -> Result<String, String> {
    match arch_x86_64::keyboard::pm_resume() {
        Ok(None) => Ok(String::from("nothing saved")),
        Ok(Some(aux)) => Ok(alloc::format!("keyboard{} port(s) enabled", if aux { " + AUX" } else { "" })),
        Err(e) => Err(String::from(e)),
    }
}

fn i915_suspend(_: u32) -> Result<String, String> { drivers::i915::pm_suspend() }
fn i915_resume(_: u32) -> Result<String, String> { drivers::i915::pm_resume() }
fn e1000e_suspend(_: u32) -> Result<String, String> { drivers::e1000e::pm_suspend() }
fn e1000e_resume(_: u32) -> Result<String, String> { drivers::e1000e::pm_resume() }
fn blk_suspend(_: u32) -> Result<String, String> { drivers::blkdev::pm_freeze(5_000) }
fn blk_resume(_: u32) -> Result<String, String> { drivers::blkdev::pm_thaw() }
fn xhci_suspend(f: u32) -> Result<String, String> { drivers::usb_hcd::pm_suspend(f & PM_XHCI_HALT != 0) }
fn xhci_resume(_: u32) -> Result<String, String> { drivers::usb_hcd::pm_resume() }

/// Suspend order; resume runs it backwards.
static DEVS: &[PmDev] = &[
    #[cfg(target_arch = "x86_64")]
    PmDev { name: "i8042", suspend: ps2_suspend, resume: ps2_resume },
    #[cfg(not(target_arch = "x86_64"))]
    PmDev { name: "i8042", suspend: none, resume: none },
    PmDev { name: "i915", suspend: i915_suspend, resume: i915_resume },
    PmDev { name: "e1000e", suspend: e1000e_suspend, resume: e1000e_resume },
    PmDev { name: "block", suspend: blk_suspend, resume: blk_resume },
    PmDev { name: "xhci", suspend: xhci_suspend, resume: xhci_resume },
];

static BUSY: AtomicBool = AtomicBool::new(false);

fn line(out: &mut String, s: &str) {
    crate::serial_print_str("[PM] ");
    crate::serial_print_str(s);
    crate::serial_print_str("\n");
    out.push_str(s);
    out.push('\n');
}

/// One freeze-only cycle. Returns (all hooks succeeded, report). Nothing may
/// block on the disks between the block hook's suspend and resume, so lines
/// go to the console (the kernel log buffers them in memory; its writer
/// waits at the block gate) and to the report string.
pub fn freeze_cycle(flags: u32, hold_ms: u32) -> (bool, String) {
    let mut out = String::new();
    if BUSY.swap(true, Ordering::AcqRel) {
        line(&mut out, "busy: a cycle is already running");
        return (false, out);
    }
    let t0 = sched::monotonic_ns();
    line(&mut out, &alloc::format!("freeze-only cycle: {} device class(es), hold {} ms, flags {:#x}", DEVS.len(), hold_ms, flags));
    let mut done = 0usize;
    let mut ok = true;
    for d in DEVS {
        let t = sched::monotonic_ns();
        match (d.suspend)(flags) {
            Ok(s) => { line(&mut out, &alloc::format!("suspend {}: ok in {} us: {}", d.name, (sched::monotonic_ns() - t) / 1000, s)); done += 1; }
            Err(e) => { line(&mut out, &alloc::format!("suspend {}: FAILED: {} (aborting, resuming {} device(s))", d.name, e, done)); ok = false; break; }
        }
    }
    if ok {
        line(&mut out, &alloc::format!("all devices quiesced after {} us", (sched::monotonic_ns() - t0) / 1000));
        let until = sched::monotonic_ns() + hold_ms as u64 * 1_000_000;
        while sched::monotonic_ns() < until { sched::kthread::msleep(5); }
    }
    for d in DEVS[..done].iter().rev() {
        let t = sched::monotonic_ns();
        match (d.resume)(flags) {
            Ok(s) => line(&mut out, &alloc::format!("resume {}: ok in {} us: {}", d.name, (sched::monotonic_ns() - t) / 1000, s)),
            Err(e) => { line(&mut out, &alloc::format!("resume {}: FAILED: {}", d.name, e)); ok = false; }
        }
    }
    line(&mut out, &alloc::format!("cycle {} in {} us", if ok { "complete" } else { "FAILED" }, (sched::monotonic_ns() - t0) / 1000));
    BUSY.store(false, Ordering::Release);
    (ok, out)
}

/// reboot(2) `LEANDROS_REBOOT_CMD_PM_TEST`: `arg` is a user `PmTestReq`.
/// Returns 0 when every hook succeeded, -EIO otherwise, -EFAULT/-EINVAL on
/// a bad request; the report is copied (NUL-terminated, truncated) to `buf`.
pub fn sys_pm_test(arg: usize) -> isize {
    let mut req = PmTestReq { flags: 0, hold_ms: 0, buf: 0, buf_len: 0 };
    let n = core::mem::size_of::<PmTestReq>();
    if unsafe { sched::uaccess::copy_from_user(&mut req as *mut _ as *mut u8, arg, n) } != 0 { return -14; }
    if req.hold_ms > 60_000 || req.flags & !PM_XHCI_HALT != 0 { return -22; }
    let (ok, rep) = freeze_cycle(req.flags, req.hold_ms);
    if req.buf != 0 && req.buf_len > 0 {
        let mut v: Vec<u8> = rep.into_bytes();
        v.truncate(req.buf_len as usize - 1);
        v.push(0);
        if unsafe { sched::uaccess::copy_to_user(req.buf as usize, v.as_ptr(), v.len()) } != 0 { return -14; }
    }
    if ok { 0 } else { -5 }
}
