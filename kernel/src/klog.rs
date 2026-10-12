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
//!   48  boot id u64 (random per boot; 0 on images from older kernels)
//!
//! Boot id (lane klogfix): a sparse re-flash zeroes only the header page of
//! each slot, so journal pages of the PREVIOUS flash survive in the data area
//! and can carry a higher sequence/length than the new boot's. Every header
//! and journal page therefore carries the boot's random id (outside the CRC,
//! so older readers still work) and a reader only accepts journals whose id
//! equals that of the slot's header page. A zeroed/invalid header page means
//! the slot is skipped entirely (the kernel always writes the header when it
//! claims the slot, before any journal exists).
//!
//! Journal pages (lane readfirst): the periodic flush does NOT rewrite the
//! header page. On a cheap stick an in-place write far from the data being
//! appended is exactly the write that costs an allocation-unit merge (~1 s,
//! nvrm-boot16), and it was a second command per flush. Instead the flush
//! appends one page right after the data, in the SAME write command as the
//! data: a copy of the header with magic "LEANLOGJ", at data offset
//! `roundup(length, 4096)` (slot offset 4096 + that). The next flush
//! overwrites it with data and puts a new one after the new end. A reader
//! takes, of the header page and every valid journal page of the same boot
//! that sits exactly at `roundup(its length)`, the one with the highest flush
//! sequence. The header page itself is written when the slot is claimed, at
//! the first dump, on poweroff/reboot and on panic, so a reader that does
//! not know journals (an older `read-usb-log.sh`) still finds a valid, if
//! older, log. The data area stops one page short of the slot for the
//! journal page.
//!
//! The data is written before (or with) the header, so a header always
//! describes bytes that are already on the medium. `scripts/read-usb-log.sh`
//! reads it back.
//!
//! Flush points: at registration (everything printed so far), periodically
//! from a kernel task when there are new bytes (adaptive, see [`flusher`]), on
//! `sync`, on hung-task reports ([`request_flush`]), on poweroff/reboot, and
//! on panic (best effort, see [`flush_panic`]).
//!
//! Locking and context (lane readfirst): the ring is lock-free and the state
//! lock (a spinlock) is never held across block I/O on the periodic/sync
//! path. That path ([`flush_now`]) snapshots what to write under the lock,
//! marks the state busy, drops the lock and writes in a context that may
//! sleep (IRQs masked like a syscall, no lock held: the USB driver parks
//! between polls and queues behind waiting reads like any write), then
//! commits the new position under the lock. Poweroff and panic use the old
//! synchronous path with their own buffer: poweroff first waits (bounded)
//! for a running periodic flush; panic does not wait and writes over it.

use alloc::boxed::Box;
use alloc::vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
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
static BOOT_ID: AtomicU64 = AtomicU64::new(0);
/// Flush interval forced by `klog.flush=<s>` (ns), 0 = adaptive.
static FORCED_INTERVAL_NS: AtomicU64 = AtomicU64::new(0);

fn raw_entropy() -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        let t = unsafe { core::arch::x86_64::_rdtsc() };
        let has = core::arch::x86_64::__cpuid(1).ecx & (1 << 30) != 0;
        if has {
            let mut r = 0u64;
            if unsafe { core::arch::x86_64::_rdrand64_step(&mut r) } == 1 { return t ^ r.rotate_left(17); }
        }
        t
    }
    #[cfg(target_arch = "aarch64")]
    {
        let v: u64;
        unsafe { core::arch::asm!("mrs {}, cntvct_el0", out(reg) v) };
        v
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    { 0 }
}

fn make_boot_id(seed: u64) -> u64 {
    let mut x = raw_entropy() ^ sched::monotonic_ns().rotate_left(32) ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    x.max(1)
}
const MAGIC: &[u8; 8] = b"LEANLOG1";
const JMAGIC: &[u8; 8] = b"LEANLOGJ";

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
    /// Synchronous path's buffer (registration, poweroff, panic).
    sbuf: Box<[u8]>,
    /// Periodic path's buffers, lent to the flush in progress (`None` then).
    job: Option<JobBufs>,
    failed: bool,
}

struct JobBufs { wbuf: Box<[u8]>, tail: Box<[u8]> }

static STATE: Mutex<Option<State>> = Mutex::new(None);
static PANICKED: AtomicBool = AtomicBool::new(false);
/// Bumped by every synchronous flush: a periodic flush that sees it change
/// stops writing and does not commit (the synchronous one wrote a superset).
static SYNC_GEN: AtomicU32 = AtomicU32::new(0);
/// A periodic flush is writing (its buffers are out of `State`).
static BUSY: AtomicBool = AtomicBool::new(false);
/// Somebody asked for a flush soon (hung-task report).
static FLUSH_REQ: AtomicBool = AtomicBool::new(false);
/// Duration of the last periodic flush (ns), for the I/O summary.
pub static LAST_FLUSH_NS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, PartialEq)]
enum Mode { Final, Panic }

fn lock_spin() -> Option<spin::MutexGuard<'static, Option<State>>> {
    for _ in 0..20_000_000u32 {
        if let Some(g) = STATE.try_lock() { return Some(g); }
        core::hint::spin_loop();
    }
    None
}

fn header_valid(h: &[u8]) -> Option<u32> {
    if &h[..8] != MAGIC { return None; }
    let stored = u32::from_le_bytes(h[40..44].try_into().unwrap());
    if crc_update(0, &h[..40]) != stored { return None; }
    Some(u32::from_le_bytes(h[8..12].try_into().unwrap()))
}

/// Fill a header (or journal) page.
fn fill_header(h: &mut [u8], magic: &[u8; 8], boot: u32, seq: u32, written: u64, slot_size: u64, crc: u32, flags: u32) {
    h.fill(0);
    h[..8].copy_from_slice(magic);
    h[8..12].copy_from_slice(&boot.to_le_bytes());
    h[12..16].copy_from_slice(&seq.to_le_bytes());
    h[16..24].copy_from_slice(&written.to_le_bytes());
    h[24..32].copy_from_slice(&slot_size.to_le_bytes());
    h[32..36].copy_from_slice(&crc.to_le_bytes());
    h[36..40].copy_from_slice(&flags.to_le_bytes());
    let c = crc_update(0, &h[..40]);
    h[40..44].copy_from_slice(&c.to_le_bytes());
    h[48..56].copy_from_slice(&BOOT_ID.load(Ordering::Relaxed).to_le_bytes());
}

fn write_header(st: &mut State, panic: bool) -> bool {
    let (boot, seq, written, ssz, crc, flags) = (st.boot, st.seq, st.written, st.slot_size, st.crc, st.flags);
    fill_header(&mut st.hdr, MAGIC, boot, seq, written, ssz, crc, flags);
    drivers::blkdev::raw_write(st.idx, st.slot_off, &st.hdr, panic)
}

fn report_failure(st: &mut State, ok: bool) {
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

/// Synchronous flush with the state locked: data, then the header page.
fn flush_locked(st: &mut State, mode: Mode) {
    SYNC_GEN.fetch_add(1, Ordering::AcqRel);
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
        st.sbuf[..tl].copy_from_slice(&st.tail[..tl]);
        for i in 0..n {
            st.sbuf[tl + i] = unsafe { *(core::ptr::addr_of!(RING_BUF) as *const u8).add((pos + i) % RING) };
        }
        let total = tl + n;
        let padded = (total + PAGE - 1) & !(PAGE - 1);
        st.sbuf[total..padded].fill(0);
        let off = st.slot_off + PAGE as u64 + (st.written - tl as u64);
        if !drivers::blkdev::raw_write(st.idx, off, &st.sbuf[..padded], panic) {
            ok = false;
            break;
        }
        let new_crc = crc_update(st.crc, &st.sbuf[tl..total]);
        let nt = total % PAGE;
        st.tail[..nt].copy_from_slice(&st.sbuf[total - nt..total]);
        st.tail_len = nt;
        st.crc = new_crc;
        st.written += n as u64;
        pos += n;
    }
    if panic { st.flags |= FLAG_PANIC; }
    st.pos = pos;
    st.seq = st.seq.wrapping_add(1);
    ok &= write_header(st, panic);
    report_failure(st, ok);
}

/// What a periodic flush writes, taken from `State` under its lock.
struct Job {
    bufs: JobBufs,
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
    head: usize,
    tail_len: usize,
    gen: u32,
}

/// Periodic / sync flush: snapshot under the lock, write unlocked (may
/// sleep), commit under the lock. Skipped when another one is running.
fn flush_job() {
    extern "C" {
        fn arch_interrupt_save() -> usize;
        fn arch_interrupt_restore(flags: usize);
    }
    if PANICKED.load(Ordering::Acquire) { return; }
    // Phase 1: snapshot.
    let mut job = {
        let mut g = match STATE.try_lock() { Some(g) => g, None => return };
        let st = match g.as_mut() { Some(s) => s, None => return };
        let head = stable_head();
        if head == st.pos { return; }
        let bufs = match st.job.take() { Some(b) => b, None => return };
        BUSY.store(true, Ordering::Release);
        let mut pos = st.pos;
        if head - pos > RING {
            pos = head - RING;
            st.flags |= FLAG_LOST;
        }
        st.seq = st.seq.wrapping_add(1);
        let mut job = Job {
            bufs, idx: st.idx, slot_off: st.slot_off, cap: st.cap, slot_size: st.slot_size, boot: st.boot,
            seq: st.seq, written: st.written, crc: st.crc, flags: st.flags, pos, head,
            tail_len: st.tail_len, gen: SYNC_GEN.load(Ordering::Acquire),
        };
        job.bufs.tail[..st.tail_len].copy_from_slice(&st.tail[..st.tail_len]);
        job
    };
    // Phase 2: write, no lock held. IRQs masked so the block layer treats
    // this task like a syscall that may sleep (it parks between polls).
    let mut ok = true;
    let mut aborted = false;
    while job.pos < job.head {
        let room = job.cap - job.written;
        if room == 0 {
            job.flags |= FLAG_FULL;
            job.pos = job.head;
            break;
        }
        let n = (job.head - job.pos).min(CHUNK).min(room as usize);
        let tl = job.tail_len;
        let JobBufs { wbuf, tail } = &mut job.bufs;
        wbuf[..tl].copy_from_slice(&tail[..tl]);
        for i in 0..n {
            wbuf[tl + i] = unsafe { *(core::ptr::addr_of!(RING_BUF) as *const u8).add((job.pos + i) % RING) };
        }
        let total = tl + n;
        let padded = (total + PAGE - 1) & !(PAGE - 1);
        wbuf[total..padded].fill(0);
        let new_crc = crc_update(job.crc, &wbuf[tl..total]);
        let new_written = job.written + n as u64;
        let last = job.pos + n >= job.head || new_written >= job.cap;
        let mut len = padded;
        if last {
            // The journal page rides in the same command, right after the data.
            if new_written >= job.cap { job.flags |= FLAG_FULL; }
            fill_header(&mut wbuf[padded..padded + PAGE], JMAGIC, job.boot, job.seq, new_written,
                        job.slot_size, new_crc, job.flags);
            len += PAGE;
        }
        if SYNC_GEN.load(Ordering::Acquire) != job.gen || PANICKED.load(Ordering::Acquire) {
            aborted = true;
            break;
        }
        let off = job.slot_off + PAGE as u64 + (job.written - tl as u64);
        let f = unsafe { arch_interrupt_save() };
        let w = drivers::blkdev::raw_write(job.idx, off, &wbuf[..len], false);
        unsafe { arch_interrupt_restore(f) };
        if !w {
            ok = false;
            break;
        }
        let nt = total % PAGE;
        tail[..nt].copy_from_slice(&wbuf[total - nt..total]);
        job.tail_len = nt;
        job.crc = new_crc;
        job.written = new_written;
        job.pos += n;
    }
    // Phase 3: commit.
    let mut g = match lock_spin() { Some(g) => g, None => { BUSY.store(false, Ordering::Release); return; } };
    if let Some(st) = g.as_mut() {
        if !aborted && SYNC_GEN.load(Ordering::Acquire) == job.gen {
            st.pos = job.pos;
            st.written = job.written;
            st.crc = job.crc;
            st.flags = job.flags;
            st.tail_len = job.tail_len;
            st.tail[..job.tail_len].copy_from_slice(&job.bufs.tail[..job.tail_len]);
            report_failure(st, ok);
        }
        st.job = Some(job.bufs);
    }
    BUSY.store(false, Ordering::Release);
}

/// Flush now unless a flush is already running (periodic flusher, `sync`,
/// the RM's log hook). Never holds the state lock across the disk write.
pub fn flush_now() { flush_job(); }

/// Ask the flusher for a flush within about a second (hung-task reports:
/// they must reach the stick even when the periodic flush is backed off).
pub fn request_flush() { FLUSH_REQ.store(true, Ordering::Release); }

/// Final flush before poweroff/reboot/halt: waits (bounded) for a running
/// periodic flush, then writes data and the header page synchronously.
pub fn flush_final() {
    let t0 = sched::monotonic_ns();
    while BUSY.load(Ordering::Acquire) && sched::monotonic_ns().saturating_sub(t0) < 10_000_000_000 {
        if sched::irqctx::can_sleep() && sched::current_pid() != 0 {
            drivers::usb_storage::sleep_until(sched::monotonic_ns() + 1_000_000);
        } else {
            core::hint::spin_loop();
        }
    }
    let mut g = match lock_spin() { Some(g) => g, None => return };
    if let Some(st) = g.as_mut() { flush_locked(st, Mode::Final); }
}

/// Best-effort flush from the panic handler.
///
/// Panic-safe parts: the ring is lock-free; the registry lock and the USB
/// disk lock are only `try_lock`ed (a held lock means the write is skipped,
/// never waited for); nothing allocates; a periodic flush in progress is not
/// waited for (its own buffers are untouched, it stops at its next write).
/// NOT guaranteed: the USB write polls the xHCI controller without
/// tick-driven deadlines (no IoWindow), so a stalled controller can still
/// hang the panicking CPU after the panic text has already reached serial and
/// the screen. If the disk was mid-request on another CPU, the panic message
/// is lost to the stick; the periodic flush then bounds what is missing.
pub fn flush_panic() {
    if PANICKED.swap(true, Ordering::SeqCst) { return; }
    let mut g = match lock_spin() { Some(g) => g, None => return };
    if let Some(st) = g.as_mut() { flush_locked(st, Mode::Panic); }
}

/// Adaptive cadence (lane fsio, lane readfirst). The next periodic flush is
/// due `factor` x the estimated flush cost later, between `MIN_INTERVAL_NS`
/// and a ceiling. On fast media that is every 2 s. On a slow stick (a flush
/// estimated at `SLOW_FLUSH_NS` or more: nvrm-boot16 measured 1-3.3 s per
/// flush) the factor is 50 and the ceiling 5 min, ~2% of the device's time
/// instead of ~10%. The estimate is the last cost or 3/4 of the previous
/// estimate, whichever is larger, so one cheap flush does not snap the
/// cadence back. sync, poweroff, panic and hung-task reports flush
/// regardless.
const MIN_INTERVAL_NS: u64 = 2_000_000_000;
const FAST_MAX_INTERVAL_NS: u64 = 30_000_000_000;
const FAST_COST_FACTOR: u64 = 10;
const SLOW_FLUSH_NS: u64 = 250_000_000;
const SLOW_MAX_INTERVAL_NS: u64 = 300_000_000_000;
const SLOW_COST_FACTOR: u64 = 50;
/// How often the flusher looks at the clock and `FLUSH_REQ`.
const TICK_NS: u64 = 1_000_000_000;

fn next_interval(cost_est: u64) -> u64 {
    let f = FORCED_INTERVAL_NS.load(Ordering::Relaxed);
    if f != 0 { return f; }
    if cost_est >= SLOW_FLUSH_NS {
        cost_est.saturating_mul(SLOW_COST_FACTOR).clamp(MIN_INTERVAL_NS, SLOW_MAX_INTERVAL_NS)
    } else {
        cost_est.saturating_mul(FAST_COST_FACTOR).clamp(MIN_INTERVAL_NS, FAST_MAX_INTERVAL_NS)
    }
}

fn flusher() -> ! {
    // Test hook: `klog.testpanic` on the kernel command line panics the
    // kernel from this task a few seconds after the log starts, to check that
    // the panic message reaches the log partition.
    let test_panic = crate::boot_cmdline()
        .map_or(false, |c| c.split_ascii_whitespace().any(|t| t == "klog.testpanic"));
    let mut ticks = 0u32;
    let mut cost_est = 0u64;
    let mut next_due = sched::monotonic_ns().saturating_add(MIN_INTERVAL_NS);
    loop {
        ticks += 1;
        if test_panic && ticks == 4 {
            panic!("klog.testpanic: deliberate kernel panic to test the log partition");
        }
        drivers::usb_storage::sleep_until(sched::monotonic_ns().saturating_add(TICK_NS));
        let t0 = sched::monotonic_ns();
        let req = FLUSH_REQ.swap(false, Ordering::AcqRel);
        if t0 < next_due && !req { continue; }
        flush_now();
        let t1 = sched::monotonic_ns();
        let cost = t1.saturating_sub(t0);
        cost_est = cost.max(cost_est / 4 * 3);
        next_due = t1 + next_interval(cost_est);
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
    BOOT_ID.store(make_boot_id(last as u64), Ordering::Relaxed);
    if let Some(sec) = crate::boot_cmdline().and_then(|c| c.split_ascii_whitespace()
        .find_map(|t| t.strip_prefix("klog.flush=")))
        .and_then(|v| v.parse::<u64>().ok()).filter(|v| (1..=300).contains(v)) {
        FORCED_INTERVAL_NS.store(sec * 1_000_000_000, Ordering::Relaxed);
    }
    let slot = (boot as u64 - 1) % SLOTS;
    let mut st = State {
        idx,
        slot_off: slot * slot_size,
        // Header page in front, one page at the end kept for the journal.
        cap: slot_size - 2 * PAGE as u64,
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
        sbuf: vec![0u8; CHUNK + PAGE].into_boxed_slice(),
        job: Some(JobBufs {
            wbuf: vec![0u8; CHUNK + 2 * PAGE].into_boxed_slice(),
            tail: vec![0u8; PAGE].into_boxed_slice(),
        }),
        failed: false,
    };
    let mut msg = alloc::string::String::new();
    let _ = core::fmt::Write::write_fmt(&mut msg, format_args!(
        "[KLOG] logging to {} slot {} (boot {}), {} KiB per slot\n",
        drivers::blkdev::dev_path(idx), slot, boot, slot_size >> 10));
    let fi = FORCED_INTERVAL_NS.load(Ordering::Relaxed);
    if fi != 0 {
        let _ = core::fmt::Write::write_fmt(&mut msg, format_args!("[KLOG] flush interval forced to {} s (klog.flush)\n", fi / 1_000_000_000));
    } else {
        msg.push_str("[KLOG] flush interval adaptive (2 s .. 5 min)\n");
    }
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
