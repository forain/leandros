//! Hung-task detector for the filesystem and USB disk path (lane loginhang,
//! 2026-10-10).
//!
//! WHY. nvrm-boot10.log: after a greeter login on the PC every later login
//! froze right after the password and the on-stick klog simply stopped — the
//! only evidence was an absence. The filesystem lock had been leaked by a
//! thread reaped mid-I/O (see `sched::ksync::klocks_held`), so nothing that
//! touched the disk could ever print or persist a word about it.
//!
//! WHAT. A kernel thread that never takes `F2FS_MOUNTS`, a disk lock or any
//! sleeping lock — it only reads atomics and try-locks the run queue — wakes
//! every second and reports, when
//!   * a task has slept >= 5 s waiting for `F2FS_MOUNTS`, or the lock has been
//!     held >= 5 s, or
//!   * a USB mass-storage command has been in flight >= 5 s,
//! the waiter, the holder (pid, executable, scheduler state, whether the pid
//! still exists at all — a holder that is gone is a LEAKED lock), what the
//! holder is doing (f2fs operation, checkpoint write-back progress) and the
//! in-flight command (op, LBA, size, age, issuing pid). The report goes to
//! VT 1's text console AND serial (`console_print_urgent`: written past the
//! staged outbox so a saturated UART cannot drop it) and into the klog ring,
//! every 10 s while the condition lasts, so it can be photographed off the
//! screen even when the stick is dead.

use alloc::string::String;
use core::fmt::Write;

const PERIOD_NS: u64 = 1_000_000_000;
const STUCK_NS: u64 = 5_000_000_000;
const REPEAT_NS: u64 = 10_000_000_000;

fn sleep_until(deadline: u64) {
    while sched::monotonic_ns() < deadline {
        sched::block_on_poll_prepare_masked(deadline, sched::POLL_MASK_TIMED_ONLY);
        if sched::monotonic_ns() >= deadline {
            sched::block_on_poll_cancel();
        } else {
            sched::block_on_poll_commit();
        }
    }
}

fn secs(ns: u64) -> (u64, u64) { (ns / 1_000_000_000, (ns / 100_000_000) % 10) }

/// ` pid N (/bin/x) state S` — or ` pid N GONE` when no such task exists.
fn describe_pid(s: &mut String, pid: u32) {
    let _ = write!(s, "pid {}", pid);
    match sched::task_brief(pid) {
        Some(b) => {
            let mut exe = [0u8; 96];
            if let Some(n) = sched::exe_path_try(b.tgid, &mut exe) {
                let _ = write!(s, " ({})", core::str::from_utf8(&exe[..n]).unwrap_or("?"));
            }
            let _ = write!(s, " state {}", b.state as char);
            if let Some(c) = b.on_cpu { let _ = write!(s, " on cpu{}", c); }
            if let Some(ch) = b.blocked_on { let _ = write!(s, " wchan {:#x}", ch); }
            let _ = write!(s, " klocks {}", sched::ksync::klocks_held(pid));
        }
        None => { let _ = write!(s, " GONE (no such task: the lock is LEAKED)"); }
    }
}

fn check(last_report: &mut u64) {
    let now = sched::monotonic_ns();
    let fs = f2fs_server::lock_snapshot();
    let fs_stuck = fs.oldest_wait_ns >= STUCK_NS || fs.hold_ns >= STUCK_NS;
    let cmds = drivers::usb_storage::inflight(now);
    let usb_stuck = cmds.iter().any(|c| c.age_ns >= STUCK_NS);
    if !fs_stuck && !usb_stuck { return; }
    if *last_report != 0 && now.saturating_sub(*last_report) < REPEAT_NS { return; }
    *last_report = now;

    let mut s = String::new();
    if fs_stuck {
        let (hs, hd) = secs(fs.hold_ns);
        let _ = write!(s, "\n[HUNG] F2FS_MOUNTS: {} waiter(s)", fs.waiters);
        if fs.oldest_pid != 0 {
            let (ws, wd) = secs(fs.oldest_wait_ns);
            let _ = write!(s, ", oldest asleep {}.{} s: ", ws, wd);
            describe_pid(&mut s, fs.oldest_pid);
        }
        s.push('\n');
        if fs.owner_pid != 0 {
            let _ = write!(s, "[HUNG] F2FS_MOUNTS holder (held {}.{} s): ", hs, hd);
            describe_pid(&mut s, fs.owner_pid);
            let (os, od) = secs(fs.op_age_ns);
            let _ = write!(s, "; doing: {}", fs.activity);
            if fs.wb_total != 0 {
                let _ = write!(s, " ({} of {} blocks written, {} remaining)",
                               fs.wb_done, fs.wb_total, fs.wb_total.saturating_sub(fs.wb_done));
            }
            let _ = write!(s, " for {}.{} s\n", os, od);
        } else {
            s.push_str("[HUNG] F2FS_MOUNTS is free now\n");
        }
    }
    for c in &cmds {
        let (as_, ad) = secs(c.age_ns);
        let _ = write!(s, "[HUNG] usbms disk{}: {} lba {} {} KiB in flight {}.{} s, issued by ",
                       c.disk, c.op, c.lba, c.bytes / 1024, as_, ad);
        describe_pid(&mut s, c.pid);
        s.push('\n');
    }
    if !usb_stuck {
        for c in drivers::usb_storage::lock_owners() {
            let _ = write!(s, "[HUNG] usbms disk{} lock: ", c.0);
            describe_pid(&mut s, c.1);
            let _ = write!(s, ", {} waiter(s)\n", c.2);
        }
    }
    crate::console_print_urgent(&s);
}

fn task() -> ! {
    let mut last_report = 0u64;
    let mut next = sched::monotonic_ns();
    loop {
        next += PERIOD_NS;
        sleep_until(next);
        check(&mut last_report);
    }
}

/// Start the detector (kernel init, task context).
pub fn init() {
    if sched::spawn(task, 0).is_none() {
        crate::serial_print_str("[HUNG] could not start the hung-task detector\n");
    }
}
