//! Periodic I/O summary (lane fsio, 2026-10-09).
//!
//! A kernel task that wakes every `PERIOD_NS` and, only when something
//! happened in the window, prints
//!
//!   [usbms] disk0 5000 ms: rd ... | wr ... | busy N% | lockwait max ... | lat ...
//!   [fslock] F2FS_MOUNTS 5000 ms: N acquisitions, W waited (avg A ms, max M ms),
//!            held total T ms (max H ms); klog flush L ms
//!
//! The first says what the USB stick itself is doing (is the device the
//! bottleneck? are writes slow?), the second whether filesystem callers queue
//! behind each other and for how long the filesystem lock is held. Both are
//! printed from task context with no lock held: on the PC every byte on the
//! 115200-baud UART costs ~90 us with IRQs masked.
//!
//! The task also kicks the console outbox (`console_drain_outbox`): a drain
//! is now bounded to what was queued when it started (see `main.rs`), so a
//! line queued behind a busy drainer goes out here at the latest.

const PERIOD_NS: u64 = 5_000_000_000;
const KICK_NS: u64 = 1_000_000_000;

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

fn task() -> ! {
    extern "C" {
        fn arch_interrupt_save() -> usize;
        fn arch_interrupt_restore(flags: usize);
    }
    let mut last = sched::monotonic_ns();
    loop {
        let mut next = sched::monotonic_ns();
        // Five console kicks per summary window.
        for _ in 0..(PERIOD_NS / KICK_NS) {
            next += KICK_NS;
            sleep_until(next);
            crate::console_drain_outbox();
            // Commit f2fs changes older than its checkpoint spacing (lane fsio:
            // checkpoints are no longer forced every 16 mutating requests).
            let f = unsafe { arch_interrupt_save() };
            f2fs_server::writeback_tick();
            unsafe { arch_interrupt_restore(f) };
        }
        let now = sched::monotonic_ns();
        let window = now.saturating_sub(last);
        last = now;
        let f = unsafe { arch_interrupt_save() };
        drivers::usb_storage::stats_report(window);
        let w = f2fs_server::mount_lock_window();
        unsafe { arch_interrupt_restore(f) };
        if w.waits != 0 || w.hold_max_ns >= 100_000_000 {
            let mut s = alloc::string::String::new();
            let _ = core::fmt::Write::write_fmt(&mut s, format_args!(
                "[fslock] F2FS_MOUNTS {} ms: {} acquisitions, {} waited (avg {} ms, max {} ms), held total {} ms (max {} ms); last klog flush {} ms\n",
                window / 1_000_000, w.acquires, w.waits,
                w.wait_ns / w.waits.max(1) / 1_000_000, w.wait_max_ns / 1_000_000,
                w.hold_ns / 1_000_000, w.hold_max_ns / 1_000_000,
                crate::klog::LAST_FLUSH_NS.load(core::sync::atomic::Ordering::Relaxed) / 1_000_000));
            crate::serial_print_str(&s);
        }
    }
}

/// Start the summary task (kernel init, task context).
pub fn init() {
    if sched::spawn(task, 0).is_none() {
        crate::serial_print_str("[IOSTAT] could not start the I/O summary task\n");
    }
}
