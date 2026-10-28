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
/// Save PCI config space of every function after the devices quiesced,
/// clobber scratch-safe registers, restore before they resume and verify
/// every saved dword (lane s3pci). Inert without this flag.
pub const PM_PCI: u32 = 2;
/// With `PM_PCI`: restore unconditionally (COMMAND=0 pulse, all registers)
/// and also clobber MSI data / MSI-X entry 0.
pub const PM_PCI_DEEP: u32 = 4;
/// Real S3 (lane s3core): save PCI config without clobbering, restore everything unconditionally.
pub const PM_S3: u32 = 8;

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

static PCI_SAVED: spin::Mutex<Option<alloc::vec::Vec<drivers::pci_pm::FnState>>> = spin::Mutex::new(None);

fn pci_suspend(f: u32) -> Result<String, String> {
    if f & (PM_PCI | PM_S3) == 0 { return Ok(String::from("off")); }
    let st = drivers::pci_pm::save_all();
    let stuck = if f & PM_PCI != 0 { drivers::pci_pm::clobber(&st, f & PM_PCI_DEEP != 0) } else { 0 };
    let r = alloc::format!("saved {} function(s), {} MSI-X dword-entries, clobbered {} function(s)",
        drivers::pci_pm::count(&st), drivers::pci_pm::msix_entries(&st) , stuck);
    *PCI_SAVED.lock() = Some(st);
    Ok(r)
}

fn pci_resume(f: u32) -> Result<String, String> {
    let Some(st) = PCI_SAVED.lock().take() else { return Ok(String::from("off")) };
    let dead = drivers::pci_pm::restore_all(&st, f & (PM_PCI_DEEP | PM_S3) != 0, |ms| sched::kthread::msleep(ms as _));
    let (n, bad) = drivers::pci_pm::verify(&st);
    for b in bad.iter().take(40) { crate::serial_print_str("[PM] pci mismatch: "); crate::serial_print_str(b); crate::serial_print_str("\n"); }
    if !dead.is_empty() { return Err(alloc::format!("{} function(s) not responding after restore", dead.len())); }
    if !bad.is_empty() { return Err(alloc::format!("{} of {} dwords mismatch", bad.len(), n)); }
    Ok(alloc::format!("restored, {} dwords verified", n))
}

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
    // Last: runs after every driver quiesced, restores before any resumes.
    PmDev { name: "pci", suspend: pci_suspend, resume: pci_resume },
];

static BUSY: AtomicBool = AtomicBool::new(false);

pub(crate) fn line(out: &mut String, s: &str) {
    crate::serial_print_str("[PM] ");
    crate::serial_print_str(s);
    crate::serial_print_str("\n");
    out.push_str(s);
    out.push('\n');
}

/// Run every device suspend hook in order; stops at the first failure and
/// returns (hooks that succeeded, all ok). Used by the real S3 path.
pub(crate) fn devs_suspend(flags: u32, out: &mut String) -> (usize, bool) {
    let mut done = 0usize;
    for d in DEVS {
        let t = sched::monotonic_ns();
        match (d.suspend)(flags) {
            Ok(s) => { line(out, &alloc::format!("suspend {}: ok in {} us: {}", d.name, (sched::monotonic_ns() - t) / 1000, s)); done += 1; }
            Err(e) => { line(out, &alloc::format!("suspend {}: FAILED: {}", d.name, e)); return (done, false); }
        }
    }
    (done, true)
}

/// Resume the first `done` hooks in reverse order.
pub(crate) fn devs_resume(done: usize, flags: u32, out: &mut String) -> bool {
    let mut ok = true;
    for d in DEVS[..done].iter().rev() {
        let t = sched::monotonic_ns();
        match (d.resume)(flags) {
            Ok(s) => line(out, &alloc::format!("resume {}: ok in {} us: {}", d.name, (sched::monotonic_ns() - t) / 1000, s)),
            Err(e) => { line(out, &alloc::format!("resume {}: FAILED: {}", d.name, e)); ok = false; }
        }
    }
    ok
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
    if req.hold_ms > 60_000 || req.flags & !(PM_XHCI_HALT | PM_PCI | PM_PCI_DEEP) != 0 { return -22; }
    let (ok, rep) = freeze_cycle(req.flags, req.hold_ms);
    if req.buf != 0 && req.buf_len > 0 {
        let mut v: Vec<u8> = rep.into_bytes();
        v.truncate(req.buf_len as usize - 1);
        v.push(0);
        if unsafe { sched::uaccess::copy_to_user(req.buf as usize, v.as_ptr(), v.len()) } != 0 { return -14; }
    }
    if ok { 0 } else { -5 }
}

// ---------------------------------------------------------------------------
// /sys/power and /sys/class/rtc (lane s3sysfs). Everything here is inert
// unless `pm.s3=1`; `freeze` needs only a `pm_test` level of freezer/devices.
// ---------------------------------------------------------------------------
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8};
use vfs_server::sysclass::Ops;

const LEVELS: [&str; 6] = ["none", "core", "processors", "platform", "devices", "freezer"];
const LV_CORE: u8 = 1;
const LV_DEVICES: u8 = 4;
const LV_FREEZER: u8 = 5;
/// Hold time of a sysfs-triggered freeze-only cycle.
const SYSFS_HOLD_MS: u32 = 500;

static PM_TEST: AtomicU8 = AtomicU8::new(0);
static S3_ENABLED: AtomicBool = AtomicBool::new(false);
static S3_REQUIRE_RTC: AtomicBool = AtomicBool::new(true);
/// True once the platform sleep path exists (set by lane s3core).
static S3_VALID: AtomicBool = AtomicBool::new(false);
static WAKE_COUNT: AtomicU32 = AtomicU32::new(0);
/// Armed wake alarm (Unix seconds), 0 = none.
static ALARM: AtomicU64 = AtomicU64::new(0);

/// Parse `pm.s3`, `pm.s3_require_rtc`, `pm.test` and register the sysfs ops.
pub fn init() {
    let cmd = crate::boot_cmdline().unwrap_or("");
    for t in cmd.split_ascii_whitespace() {
        match t {
            "pm.s3=1" => S3_ENABLED.store(true, Ordering::Relaxed),
            "pm.s3_require_rtc=0" => S3_REQUIRE_RTC.store(false, Ordering::Relaxed),
            _ => if let Some(l) = t.strip_prefix("pm.test=") {
                if let Some(i) = LEVELS.iter().position(|x| *x == l) { PM_TEST.store(i as u8, Ordering::Relaxed); }
            },
        }
    }
    #[cfg(target_arch = "x86_64")]
    S3_VALID.store(crate::pm_s3::valid(), Ordering::Relaxed);
    vfs_server::sysclass::set_sysclass_ops(&PM_OPS);
}

static PM_OPS: Ops = Ops {
    devices: |class, idx, out| match class {
        "rtc" => if idx == 0 && has_rtc() { copy(out, "rtc0") } else { None },
        _ => drivers::power_sysfs::device(class, idx, out),
    },
    attrs: |class, dev, idx, out| match class {
        "power" => copy(out, *["state", "mem_sleep", "pm_test", "wakeup_count"].get(idx)?),
        "rtc" => copy(out, *["wakealarm", "since_epoch"].get(idx)?),
        _ => drivers::power_sysfs::attr_name(class, dev, idx, out),
    },
    read: |class, dev, attr, out| match class {
        "power" => power_read(attr, out),
        "rtc" => rtc_read(attr, out),
        _ => drivers::power_sysfs::read(class, dev, attr, out),
    },
    writable: |class, dev, attr| match class {
        "power" => attr != "" && ["state", "mem_sleep", "pm_test", "wakeup_count"].contains(&attr),
        "rtc" => attr == "wakealarm",
        _ => drivers::power_sysfs::writable(class, dev, attr),
    },
    write: |class, dev, attr, data| match class {
        "power" => power_write(attr, data),
        "rtc" => rtc_write(data),
        _ => drivers::power_sysfs::write(class, dev, attr, data),
    },
};

fn copy(out: &mut [u8], s: &str) -> Option<usize> {
    let b = s.as_bytes();
    if b.len() > out.len() { return None; }
    out[..b.len()].copy_from_slice(b);
    Some(b.len())
}

fn has_rtc() -> bool { cfg!(target_arch = "x86_64") }

fn s3_ok() -> bool { S3_ENABLED.load(Ordering::Relaxed) && S3_VALID.load(Ordering::Relaxed) }

fn power_read(attr: &str, out: &mut [u8]) -> Option<usize> {
    let s = match attr {
        // freeze (devices-only cycle) is always offered; mem only when S3 is real.
        "state" => String::from(if s3_ok() { "freeze mem\n" } else { "freeze\n" }),
        "mem_sleep" => String::from(if s3_ok() { "[deep]\n" } else { "\n" }),
        "pm_test" => {
            let cur = PM_TEST.load(Ordering::Relaxed) as usize;
            let mut s = String::new();
            for (i, l) in LEVELS.iter().enumerate() {
                if i > 0 { s.push(' '); }
                if i == cur { s.push('['); s.push_str(l); s.push(']'); } else { s.push_str(l); }
            }
            s.push('\n');
            s
        }
        "wakeup_count" => alloc::format!("{}\n", WAKE_COUNT.load(Ordering::Relaxed)),
        _ => return None,
    };
    copy(out, &s)
}

fn power_write(attr: &str, data: &[u8]) -> i32 {
    let Ok(s) = core::str::from_utf8(data) else { return -22 };
    let s = s.trim();
    match attr {
        "state" => match s {
            "freeze" => suspend_enter(false),
            "mem" => suspend_enter(true),
            _ => -22,
        },
        "mem_sleep" => if s3_ok() && s == "deep" { 0 } else { -22 },
        "pm_test" => match LEVELS.iter().position(|l| *l == s) {
            Some(i) => { PM_TEST.store(i as u8, Ordering::Relaxed); 0 }
            None => -22,
        },
        "wakeup_count" => match s.parse::<u32>() {
            Ok(v) if v == WAKE_COUNT.load(Ordering::Relaxed) => 0,
            Ok(_) => -16, // EBUSY: a wake event arrived since it was read
            Err(_) => -22,
        },
        _ => -2,
    }
}

/// `state` write. `freeze` runs the devices-only cycle; `mem` additionally
/// needs `pm.s3=1`, an armed alarm (`pm.s3_require_rtc`) at level `none`, and
/// the platform path (lane s3core), which does not exist yet: -ENOSYS.
fn suspend_enter(mem: bool) -> i32 {
    let lv = PM_TEST.load(Ordering::Relaxed);
    if mem {
        if !S3_ENABLED.load(Ordering::Relaxed) { return -22; }
        if lv == 0 && S3_REQUIRE_RTC.load(Ordering::Relaxed) {
            let a = ALARM.load(Ordering::Relaxed);
            let now = rtc_now().unwrap_or(0);
            if a == 0 || a <= now || a - now > 300 { return -22; }
        }
    } else if lv != LV_DEVICES && lv != LV_FREEZER {
        return -22; // plain freeze is only offered as a test level for now
    }
    if lv == LV_FREEZER {
        let mut o = String::new();
        line(&mut o, "pm_test=freezer: freeze and thaw only (no freezer yet), nothing to do");
        return 0;
    }
    #[cfg(target_arch = "x86_64")]
    if lv == 0 || lv == LV_CORE {
        if !S3_VALID.load(Ordering::Relaxed) { return -22; }
        return crate::pm_s3::suspend(lv == LV_CORE);
    }
    if lv != LV_DEVICES { return -38; } // platform/processors (and any level on aarch64): ENOSYS
    let (ok, _) = freeze_cycle(0, SYSFS_HOLD_MS);
    if ok { 0 } else { -5 }
}

fn rtc_now() -> Option<u64> {
    #[cfg(target_arch = "x86_64")] { arch_x86_64::rtc::epoch_secs() }
    #[cfg(not(target_arch = "x86_64"))] { None }
}

fn rtc_read(attr: &str, out: &mut [u8]) -> Option<usize> {
    match attr {
        "wakealarm" => {
            let a = ALARM.load(Ordering::Relaxed);
            // A fired alarm reads back empty, like Linux.
            if a != 0 && rtc_now().map_or(false, |n| a > n) { copy(out, &alloc::format!("{}\n", a)) } else { copy(out, "\n") }
        }
        "since_epoch" => copy(out, &alloc::format!("{}\n", rtc_now()?)),
        _ => None,
    }
}

fn rtc_write(data: &[u8]) -> i32 {
    #[cfg(target_arch = "x86_64")]
    {
        let Ok(s) = core::str::from_utf8(data) else { return -22 };
        let s = s.trim();
        if s.is_empty() || s == "0" {
            arch_x86_64::rtc::clear_alarm();
            ALARM.store(0, Ordering::Relaxed);
            return 0;
        }
        let Some(now) = arch_x86_64::rtc::epoch_secs() else { return -19 };
        let target = match s.strip_prefix('+') {
            Some(n) => match n.parse::<u64>() { Ok(n) => now + n, Err(_) => return -22 },
            None => match s.parse::<u64>() { Ok(t) => t, Err(_) => return -22 },
        };
        match arch_x86_64::rtc::set_alarm(target) {
            Ok((sc, mi, hr, b)) => {
                ALARM.store(target, Ordering::Relaxed);
                crate::serial_print_str(&alloc::format!("[PM] rtc alarm armed for {} (cmos sec={:#04x} min={:#04x} hour={:#04x} regB={:#04x})\n", target, sc, mi, hr, b));
                0
            }
            Err(e) => e,
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    { let _ = data; -19 }
}
