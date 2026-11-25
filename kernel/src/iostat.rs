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
    let mut pc_last = mm::pagecache::stats();
    loop {
        let mut next = sched::monotonic_ns();
        // Five console kicks per summary window.
        for _ in 0..(PERIOD_NS / KICK_NS) {
            next += KICK_NS;
            sleep_until(next);
            uart_rx_report();
            crate::console_drain_outbox();
            drivers::framebuffer::fb_flush_deferred();
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
        isoch_report(window);
        let w = f2fs_server::mount_lock_window();
        unsafe { arch_interrupt_restore(f) };
        let (cps, fsyncs, shared, throttled, fsync_pid) = f2fs_server::checkpoint_window();
        let locked_io = f2fs_server::locked_io_window();
        let (fetches, fshared, stale, gave_up, abandoned, fetch_max_ms) = f2fs_server::noio_window();
        let (batched, miss_extra) = f2fs_server::noio_batch_window();
        if w.waits != 0 || w.hold_max_ns >= 100_000_000 || cps != 0 || locked_io != 0 || fetches != 0 {
            let mut s = alloc::string::String::new();
            let _ = core::fmt::Write::write_fmt(&mut s, format_args!(
                "[fslock] F2FS_MOUNTS {} ms: {} acquisitions, {} waited (avg {} ms, max {} ms), held total {} ms (max {} ms); {} block requests under it, {} reads for it unlocked (max {} ms, {} shared, {} stale, {} gave up, {} abandoned, {} batched into an earlier pass of {} extra misses); {} checkpoints, {} fsyncs ({} shared, last pid {}), {} throttled; last klog flush {} ms\n",
                window / 1_000_000, w.acquires, w.waits,
                w.wait_ns / w.waits.max(1) / 1_000_000, w.wait_max_ns / 1_000_000,
                w.hold_ns / 1_000_000, w.hold_max_ns / 1_000_000, locked_io,
                fetches, fetch_max_ms, fshared, stale, gave_up, abandoned, batched, miss_extra, cps, fsyncs, shared, fsync_pid, throttled,
                crate::klog::LAST_FLUSH_NS.load(core::sync::atomic::Ordering::Relaxed) / 1_000_000));
            crate::stat_line(s.as_bytes());
        }
        // Who fsyncs what, and what the checkpoints wrote (lane fsyncbatch).
        if let Some(s) = f2fs_server::fsync_report(window) {
            crate::stat_line(s.as_bytes());
        }
        pcache_report(window, &mut pc_last);
    }
}

/// `[pcache]` line for the window, only when the page cache did something:
/// pages served from it (hits), read for a miss (misses), read ahead beyond
/// what was asked (ra), evicted (by the clock or the allocator's emergency
/// path), and what it holds now.
fn pcache_report(window: u64, last: &mut mm::pagecache::Stats) {
    let st = mm::pagecache::stats();
    let d = |now: usize, before: usize| now.saturating_sub(before);
    let hits = d(st.hits, last.hits);
    let misses = d(st.misses, last.misses);
    let ra = d(st.readahead, last.readahead);
    let evict = d(st.evicted + st.emergency, last.evicted + last.emergency);
    let waits = d(st.waits, last.waits);
    let bypass = d(st.bypass, last.bypass);
    let wb = d(st.written_back, last.written_back);
    *last = st;
    if hits == 0 && misses == 0 && evict == 0 && wb == 0 { return; }
    let mut s = alloc::string::String::new();
    let _ = core::fmt::Write::write_fmt(&mut s, format_args!(
        "[pcache] {} s: hits {} misses {} ra {} evict {} cached {} MiB (dirty {} KiB, written back {}, fill waits {}, uncached reads {}, cap {} MiB, free {} MiB)\n",
        (window + 500_000_000) / 1_000_000_000, hits, misses, ra, evict, st.cached * 4 / 1024,
        st.dirty * 4, wb, waits, bypass, st.cap * 4 / 1024, mm::buddy::free_pages() * 4 / 1024));
    crate::stat_line(s.as_bytes());
}

/// Start the summary task (kernel init, task context).
pub fn init() {
    if sched::spawn(task, 0).map(|p| sched::set_comm(p, b"kiostat")).is_none() {
        crate::serial_print_str("[IOSTAT] could not start the I/O summary task\n");
    }
}

/// One `[uart]` line whenever the console UART lost input since the last
/// window (RX overruns / framing errors). Printed on the console itself: it
/// is rare, and it is the one place a user typing at the serial port sees it.
fn uart_rx_report() {
    #[cfg(target_arch = "aarch64")]
    {
        use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
        static SEEN: AtomicU64 = AtomicU64::new(0);
        // One-shot GIC/PL011 snapshot the tick took while RX data sat
        // un-taken by the interrupt (openbugs #101).
        arch_aarch64::gic::uart_irq_diag_report();
        let oe = arch_aarch64::uart::UART_RX_OVERRUNS.load(Relaxed);
        let er = arch_aarch64::uart::UART_RX_ERRORS.load(Relaxed);
        // `irqs` proves the RX interrupt fires (printed once when the first
        // one arrives, then with every loss); `tick` counts bytes only the
        // cpu0 tick fallback drained; `rescued` bytes a TX wait pulled out.
        static IRQ_SEEN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
        let irqs = arch_aarch64::uart::UART_RX_IRQS.load(Relaxed);
        let first_irq = irqs != 0 && !IRQ_SEEN.swap(true, Relaxed);
        let (txgap, late) = arch_aarch64::uart::irqoff_stats_us();
        if oe + er != SEEN.swap(oe + er, Relaxed) || first_irq {
            crate::serial_print_str(&alloc::format!(
                "[uart] rx overruns {} errors {} throttled {} irqs {} tick {} rescued {} txgap_us {} irqlate_us {}\n", oe, er,
                evdev_server::SERIAL_RX_THROTTLED.load(Relaxed), irqs,
                arch_aarch64::uart::UART_RX_TICK_BYTES.load(Relaxed),
                arch_aarch64::uart::UART_RX_RESCUED.load(Relaxed), txgap, late));
        }
    }
}

/// `[usbisoch]` line for a window in which isochronous TDs completed.
fn isoch_report(window_ns: u64) {
    use core::sync::atomic::{AtomicU64, Ordering};
    static LAST: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
    let s = usb::xhci::isoch::stats();
    let now = [s.tds, s.underrun, s.missed, s.silence, s.errors];
    let mut d = [0u64; 5];
    for i in 0..5 { d[i] = now[i] - LAST[i].swap(now[i], Ordering::Relaxed); }
    if d[0] == 0 && d[1] == 0 && d[2] == 0 { return; }
    let line = alloc::format!(
        "[usbisoch] {} ms: {} TDs, isoch_underrun {} (total {}), isoch_missed {} (total {}), silence {}, errors {}\n",
        window_ns / 1_000_000, d[0], d[1], now[1], d[2], now[2], d[3], d[4]);
    crate::serial_print_str(&line);
}

/// `[tx_dropped, user_tx_dropped]` for /proc/sysstat's `# uartdrop` line.
#[no_mangle]
pub extern "C" fn kernel_uart_tx_drops(out: *mut u64) {
    if out.is_null() { return; }
    use core::sync::atomic::Ordering::Relaxed;
    #[cfg(target_arch = "aarch64")]
    let v = [arch_aarch64::uart::UART_TX_DROPPED.load(Relaxed), arch_aarch64::uart::USER_TX_DROPPED.load(Relaxed)];
    #[cfg(target_arch = "x86_64")]
    let v = [arch_x86_64::UART_TX_DROPPED.load(Relaxed), arch_x86_64::USER_TX_DROPPED.load(Relaxed)];
    unsafe { core::ptr::copy_nonoverlapping(v.as_ptr(), out, 2); }
}

/// `[txgap_us, irqlate_us, rescued, overruns]` for /proc/sysstat's `# uart`
/// line (servers/vfs `gen_sysstat`). Zeros where there is no PL011 console.
#[no_mangle]
pub extern "C" fn kernel_uart_rx_stats(out: *mut u64) {
    if out.is_null() { return; }
    #[allow(unused_mut)]
    let mut v = [0u64; 4];
    #[cfg(target_arch = "aarch64")]
    {
        use core::sync::atomic::Ordering::Relaxed;
        let (g, l) = arch_aarch64::uart::irqoff_stats_us();
        v = [g, l, arch_aarch64::uart::UART_RX_RESCUED.load(Relaxed),
             arch_aarch64::uart::UART_RX_OVERRUNS.load(Relaxed)];
    }
    unsafe { core::ptr::copy_nonoverlapping(v.as_ptr(), out, 4); }
}

/// Appends the virtio-gpu / DRM half of `/proc/pollfds` to `buf` (cap `cap`);
/// returns the bytes written. Called by servers/vfs `gen_pollfds`.
#[no_mangle]
pub extern "C" fn kernel_pollfds_gpu(buf: *mut u8, cap: usize) -> usize {
    let mut s = alloc::string::String::new();
    drivers::drm_device_interface::pollfds_text(&mut s);
    let n = s.len().min(cap);
    unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), buf, n); }
    n
}

/// `/proc/drm/fence_stats`: virtio-gpu fence completion-path counters and
/// latency histograms (lock losses, hook latency, head-of-line hold).
/// Returns the bytes written. Called by servers/vfs.
#[no_mangle]
pub extern "C" fn kernel_drm_fence_stats(buf: *mut u8, cap: usize) -> usize {
    let mut s = alloc::string::String::new();
    drivers::virtio_gpu::fence_stats_text(&mut s);
    let n = s.len().min(cap);
    unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), buf, n); }
    n
}

/// `/proc/leandros_hvs`: Pi 5 HVS flip-path counters (`drivers::hvs_plane::stats`). Returns the
/// bytes written. Called by servers/vfs.
#[no_mangle]
pub extern "C" fn kernel_hvs_stats(buf: *mut u8, cap: usize) -> usize {
    let mut s = alloc::string::String::new();
    drivers::hvs_stats_text(&mut s);
    let n = s.len().min(cap);
    unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), buf, n); }
    n
}

/// `/proc/leandros_hvs` write `reset`: zero the counters.
#[no_mangle]
pub extern "C" fn kernel_hvs_stats_reset() {
    drivers::hvs_stats_reset();
}

/// `/proc/drm/fence_hold`: state of the fence-retirement hold (test knob).
#[no_mangle]
pub extern "C" fn kernel_drm_fence_hold_get() -> u32 {
    drivers::virtio_gpu::fence_hold_active() as u32
}

/// `/proc/drm/fence_hold` write: nonzero holds fence retirement (auto-released
/// after 2 s), zero releases it.
#[no_mangle]
pub extern "C" fn kernel_drm_fence_hold_set(on: u32) {
    drivers::virtio_gpu::fence_hold_set(on);
}
