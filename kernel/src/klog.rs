//! Persistent kernel log on a raw partition of the boot disk.
//!
//! Debugging real hardware means reading the log off the machine afterwards,
//! so everything that reaches the console (kernel diagnostics and userspace
//! console output; they all end in `serial_write_byte*`) is also copied into
//! a lock-free RAM ring, and a flusher appends it to a dedicated log
//! partition of the boot disk. The partition has no filesystem:
//!
//!   type   MBR 0xDA, or GPT type GUID 042B7470-1302-4E61-9347-A0130B276754
//!   size   64 MiB written by `scripts/write-usb.sh`, at the very end
//!   slots  4 slots of `min(16 MiB, size / 4)`; boot N uses slot (N-1) % 4
//!   slot   4096-byte header page, then the plain-text log
//!
//! Header (little endian, rest of the page zero):
//!    0  magic "LEANLOG1"      8  boot counter u32     12  flush sequence u32
//!   16  data length u64      24  slot size u64        32  CRC32 of the data
//!   36  flags u32 (1 = slot full, 2 = panic, 4 = ring overrun lost bytes)
//!   40  CRC32 of header bytes 0..40
//!
//! The data is written before the header, so a header always describes bytes
//! that are already on the medium. `scripts/read-usb-log.sh` reads it back.
//!
//! Flush points: at registration (everything printed so far), every `FLUSH_INTERVAL_NS` (10 s) from
//! a kernel task when there are new bytes, on `sync`, on poweroff/reboot, and
//! on panic (best effort, see [`flush_panic`]).
//!
//! Locking: the ring is lock-free. The state lock is only ever taken with
//! `try_lock` (periodic, sync) or a bounded spin (shutdown, panic); no block
//! I/O runs under any lock another path could be waiting on forever. The
//! flusher masks IRQs around each flush so the USB driver's IoWindow rules
//! apply (spin with `irq_window`, never yield).

use alloc::boxed::Box;
use alloc::vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use spin::Mutex;

const RING: usize = 2 << 20;
static mut RING_BUF: [u8; RING] = [0; RING];
static HEAD: AtomicUsize = AtomicUsize::new(0);

const SLOTS: u64 = 4;
const PAGE: usize = 4096;
const MAX_SLOT: u64 = 16 << 20;
const CHUNK: usize = 256 << 10;
const FLAG_FULL: u32 = 1;
const FLAG_PANIC: u32 = 2;
const FLAG_LOST: u32 = 4;
const MAGIC: &[u8; 8] = b"LEANLOG1";

/// Record one console byte. Called from every console write path, in any
/// context (interrupts, panic): one atomic add and one store.
#[inline]
pub fn push(b: u8) {
    WRITERS.fetch_add(1, Ordering::Acquire);
    let i = HEAD.fetch_add(1, Ordering::Relaxed);
    unsafe { *(core::ptr::addr_of_mut!(RING_BUF) as *mut u8).add(i % RING) = b; }
    WRITERS.fetch_sub(1, Ordering::Release);
}

/// Record a whole line (or write) as ONE contiguous run of the ring (lane
/// loginhang): bytes pushed one at a time from two CPUs interleaved byte by
/// byte in the stick log (nvrm-boot10.log's garbled `[nvuapi]` pairs). Any
/// context: one atomic add for the reservation, then the copy.
pub fn push_bytes(bytes: &[u8]) {
    if bytes.is_empty() { return; }
    WRITERS.fetch_add(1, Ordering::Acquire);
    let i = HEAD.fetch_add(bytes.len(), Ordering::Relaxed);
    let base = core::ptr::addr_of_mut!(RING_BUF) as *mut u8;
    for (k, &b) in bytes.iter().enumerate() {
        unsafe { *base.add((i + k) % RING) = b; }
    }
    WRITERS.fetch_sub(1, Ordering::Release);
}

/// Pushers between their reservation and the end of their copy. The flusher
/// snapshots `HEAD` while this is zero (briefly retried), so it does not write
/// out a reserved run that is still being filled.
static WRITERS: AtomicUsize = AtomicUsize::new(0);

fn stable_head() -> usize {
    for _ in 0..100_000 {
        if WRITERS.load(Ordering::Acquire) == 0 {
            let h = HEAD.load(Ordering::Acquire);
            if WRITERS.load(Ordering::Acquire) == 0 { return h; }
        }
        core::hint::spin_loop();
    }
    HEAD.load(Ordering::Acquire)
}

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 { c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 }; k += 1; }
        t[i] = c;
        i += 1;
    }
    t
}
static CRC_T: [u32; 256] = crc_table();

/// Incremental CRC-32 (zlib); `crc` is the running value (start 0).
fn crc_update(crc: u32, data: &[u8]) -> u32 {
    let mut c = !crc;
    for &b in data { c = CRC_T[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8); }
    !c
}

struct State {
    idx: usize,
    slot_off: u64,
    cap: u64,
    slot_size: u64,
    boot: u32,
    seq: u32,
    written: u64,
    crc: u32,
    flags: u32,
    pos: usize,
    tail_len: usize,
    tail: Box<[u8]>,
    hdr: Box<[u8]>,
    wbuf: Box<[u8]>,
    failed: bool,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static PANICKED: AtomicBool = AtomicBool::new(false);
/// Duration of the last periodic flush (ns), for the I/O summary.
pub static LAST_FLUSH_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

#[derive(Clone, Copy, PartialEq)]
enum Mode { Normal, Final, Panic }

fn lock(mode: Mode) -> Option<spin::MutexGuard<'static, Option<State>>> {
    match mode {
        Mode::Normal => STATE.try_lock(),
        _ => {
            for _ in 0..20_000_000u32 {
                if let Some(g) = STATE.try_lock() { return Some(g); }
                core::hint::spin_loop();
            }
            None
        }
    }
}

fn header_valid(h: &[u8]) -> Option<u32> {
    if &h[..8] != MAGIC { return None; }
    let stored = u32::from_le_bytes(h[40..44].try_into().unwrap());
    if crc_update(0, &h[..40]) != stored { return None; }
    Some(u32::from_le_bytes(h[8..12].try_into().unwrap()))
}

fn write_header(st: &mut State, panic: bool) -> bool {
    let h = &mut st.hdr;
    h.fill(0);
    h[..8].copy_from_slice(MAGIC);
    h[8..12].copy_from_slice(&st.boot.to_le_bytes());
    h[12..16].copy_from_slice(&st.seq.to_le_bytes());
    h[16..24].copy_from_slice(&st.written.to_le_bytes());
    h[24..32].copy_from_slice(&st.slot_size.to_le_bytes());
    h[32..36].copy_from_slice(&st.crc.to_le_bytes());
    h[36..40].copy_from_slice(&st.flags.to_le_bytes());
    let c = crc_update(0, &h[..40]);
    h[40..44].copy_from_slice(&c.to_le_bytes());
    drivers::blkdev::raw_write(st.idx, st.slot_off, &st.hdr, panic)
}

fn flush_locked(st: &mut State, mode: Mode) {
    let panic = mode == Mode::Panic;
    let head = if panic { HEAD.load(Ordering::Acquire) } else { stable_head() };
    let mut pos = st.pos;
    if head == pos && !panic { return; }
    if head - pos > RING {
        pos = head - RING;
        st.flags |= FLAG_LOST;
    }
    let mut ok = true;
    while pos < head {
        let room = st.cap - st.written;
        if room == 0 {
            st.flags |= FLAG_FULL;
            pos = head;
            break;
        }
        let n = (head - pos).min(CHUNK).min(room as usize);
        let tl = st.tail_len;
        st.wbuf[..tl].copy_from_slice(&st.tail[..tl]);
        for i in 0..n {
            st.wbuf[tl + i] = unsafe { *(core::ptr::addr_of!(RING_BUF) as *const u8).add((pos + i) % RING) };
        }
        let total = tl + n;
        let padded = (total + PAGE - 1) & !(PAGE - 1);
        st.wbuf[total..padded].fill(0);
        let off = st.slot_off + PAGE as u64 + (st.written - tl as u64);
        if !drivers::blkdev::raw_write(st.idx, off, &st.wbuf[..padded], panic) {
            ok = false;
            break;
        }
        let new_crc = crc_update(st.crc, &st.wbuf[tl..total]);
        let nt = total % PAGE;
        st.tail[..nt].copy_from_slice(&st.wbuf[total - nt..total]);
        st.tail_len = nt;
        st.crc = new_crc;
        st.written += n as u64;
        pos += n;
    }
    if panic { st.flags |= FLAG_PANIC; }
    st.pos = pos;
    st.seq = st.seq.wrapping_add(1);
    ok &= write_header(st, panic);
    if !ok {
        // Say so once per failure episode (the message itself goes to the
        // ring and so triggers one more attempt, which then stays quiet).
        if !st.failed {
            st.failed = true;
            crate::serial_print_str("[KLOG] write to the log partition failed; will retry\n");
        }
    } else {
        st.failed = false;
    }
}

fn flush(mode: Mode) {
    let mut g = match lock(mode) { Some(g) => g, None => return };
    if let Some(st) = g.as_mut() { flush_locked(st, mode); }
}

/// Flush now if the state is free (periodic flusher, `sync`).
pub fn flush_now() { flush(Mode::Normal); }

/// Final flush before poweroff/reboot/halt.
pub fn flush_final() { flush(Mode::Final); }

/// Best-effort flush from the panic handler.
///
/// Panic-safe parts: the ring is lock-free; the registry lock and the USB
/// disk lock are only `try_lock`ed (a held lock means the write is skipped,
/// never waited for); nothing allocates. NOT guaranteed: the USB write
/// polls the xHCI controller without tick-driven deadlines (no IoWindow), so
/// a stalled controller can still hang the panicking CPU after the panic text
/// has already reached serial and the screen. If the disk was mid-request on
/// another CPU, the panic message is lost to the stick; the periodic flush
/// (every 10 s) then bounds what is missing.
pub fn flush_panic() {
    if PANICKED.swap(true, Ordering::SeqCst) { return; }
    flush(Mode::Panic);
}

/// Period of the background flush. Every flush is a data write plus an
/// in-place header rewrite, 1-2.6 s of device time on the USB 2.0 stick that
/// also holds root, so it is kept sparse; panic, sync, shutdown and hang
/// reports flush immediately regardless.
const FLUSH_INTERVAL_NS: u64 = 10_000_000_000;

fn flusher() -> ! {
    extern "C" {
        fn arch_interrupt_save() -> usize;
        fn arch_interrupt_restore(flags: usize);
    }
    // Test hook: `klog.testpanic` on the kernel command line panics the
    // kernel from this task ~10 s after the log starts, to check that the
    // panic message reaches the log partition.
    let test_panic = crate::boot_cmdline()
        .map_or(false, |c| c.split_ascii_whitespace().any(|t| t == "klog.testpanic"));
    let mut rounds = 0u32;
    // Adaptive cadence (lane fsio). Every flush rewrites the slot header and
    // the partial tail page in place, i.e. at least two small writes. On the
    // PC's USB stick a small write appears to cost ~0.5 s (nvrm-boot7.log),
    // so a flush every 2 s kept the disk busy more than half the time and
    // every filesystem read queued behind it. The next flush is due
    // `FLUSH_COST_FACTOR` x the last flush's duration later (2 s .. 30 s):
    // unchanged on fast media, ~10% duty on a slow stick. sync, poweroff and
    // panic still flush immediately.
    const MIN_INTERVAL_NS: u64 = 2_000_000_000;
    const MAX_INTERVAL_NS: u64 = 30_000_000_000;
    const FLUSH_COST_FACTOR: u64 = 10;
    let mut next_due = 0u64;
    loop {
        rounds += 1;
        if test_panic && rounds == 2 {
            panic!("klog.testpanic: deliberate kernel panic to test the log partition");
        }
        let deadline = sched::monotonic_ns().saturating_add(MIN_INTERVAL_NS);
        while sched::monotonic_ns() < deadline {
            sched::block_on_poll_prepare_masked(deadline, sched::POLL_MASK_TIMED_ONLY);
            if sched::monotonic_ns() >= deadline {
                sched::block_on_poll_cancel();
            } else {
                sched::block_on_poll_commit();
            }
        }
        // Mask IRQs for the flush: the block layer then treats this task like
        // a syscall (preemption off, poll loops spin) instead of yielding in
        // the middle of a USB transfer.
        let t0 = sched::monotonic_ns();
        if t0 < next_due { continue; }
        let f = unsafe { arch_interrupt_save() };
        flush_now();
        unsafe { arch_interrupt_restore(f) };
        let t1 = sched::monotonic_ns();
        let cost = t1.saturating_sub(t0);
        next_due = t1 + cost.saturating_mul(FLUSH_COST_FACTOR).clamp(MIN_INTERVAL_NS, MAX_INTERVAL_NS)
            .saturating_sub(MIN_INTERVAL_NS);
        LAST_FLUSH_NS.store(cost, Ordering::Relaxed);
    }
}

/// Find the log partition on the boot disk, claim this boot's slot, dump the
/// log so far and start the periodic flusher. Task context, after the block
/// devices (USB included) are registered.
pub fn init() {
    let idx = match drivers::blkdev::find_log_partition(crate::boot_disk()) {
        Some(i) => i,
        None => {
            crate::serial_print_str("[KLOG] no log partition on the boot disk; log is RAM only\n");
            return;
        }
    };
    let (ss, bytes) = match drivers::blkdev::raw_geometry(idx) { Some(g) => g, None => return };
    let slot_size = (bytes / SLOTS).min(MAX_SLOT) & !(PAGE as u64 - 1);
    if ss as usize > PAGE || slot_size < 16 * PAGE as u64 {
        crate::serial_print_str("[KLOG] log partition too small or odd sectors; log is RAM only\n");
        return;
    }
    let mut hdr = vec![0u8; PAGE].into_boxed_slice();
    let mut last = 0u32;
    for s in 0..SLOTS {
        if drivers::blkdev::raw_read(idx, s * slot_size, &mut hdr) {
            if let Some(b) = header_valid(&hdr) { last = last.max(b); }
        }
    }
    let boot = last.wrapping_add(1).max(1);
    let slot = (boot as u64 - 1) % SLOTS;
    let mut st = State {
        idx,
        slot_off: slot * slot_size,
        cap: slot_size - PAGE as u64,
        slot_size,
        boot,
        seq: 0,
        written: 0,
        crc: 0,
        flags: 0,
        pos: 0,
        tail_len: 0,
        tail: vec![0u8; PAGE].into_boxed_slice(),
        hdr,
        wbuf: vec![0u8; CHUNK + PAGE].into_boxed_slice(),
        failed: false,
    };
    let mut msg = alloc::string::String::new();
    let _ = core::fmt::Write::write_fmt(&mut msg, format_args!(
        "[KLOG] logging to {} slot {} (boot {}), {} KiB per slot\n",
        drivers::blkdev::dev_path(idx), slot, boot, slot_size >> 10));
    crate::serial_print_str(&msg);
    // Claim the slot (len 0 invalidates the previous contents), then dump
    // everything printed so far.
    if !write_header(&mut st, false) {
        crate::serial_print_str("[KLOG] cannot write the log partition; log is RAM only\n");
        return;
    }
    flush_locked(&mut st, Mode::Final);
    *STATE.lock() = Some(st);
    if sched::spawn(flusher, 0).is_none() {
        crate::serial_print_str("[KLOG] could not start the flusher task\n");
    }
}
