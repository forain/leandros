//! The old kernel's final kexec log, handed to the next kernel.
//!
//! Everything `reboot(KEXEC)` prints after the kernel log's final flush
//! (xHCI halt, driver hooks, the per-function device resets, the CPU stop)
//! used to reach the serial port only; a machine without one (the laptop)
//! lost it. Now, from the start of `kernel_kexec` until just before the
//! purgatory jump, every kernel console line is also copied into a 16 KiB
//! buffer that `kexec_file_load` allocated with the image (outside every
//! destination range, so neither the copy nor the new kernel's image
//! overwrites it).
//!
//! Hand-over: its `(phys, size)` goes in the boot info built at load time,
//! a private multiboot2 tag on x86_64 and `/chosen/leandros,kexec-prevlog`
//! on aarch64 (`boot::kexec_prevlog`). Neither touches the command line, so
//! nothing accumulates across generations or under `--reuse-cmdline`. The
//! range stays plain RAM in the map: the next kernel keeps it out of its
//! allocator ([`reserve_early`], before `mm` starts), prints it as
//! `[prevkexec] ...` lines once memory is up ([`replay`]) and frees it.
//!
//! Buffer layout: magic u64, text length u32, capacity u32, then the text.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Buffer size (an order-2 buddy block).
pub const SIZE: usize = 16 * 1024;
pub const ORDER: usize = 2;
const MAGIC: u64 = u64::from_le_bytes(*b"LOSPRVLG");
const HDR: usize = 16;
const CAP: usize = SIZE - HDR;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static VA: AtomicUsize = AtomicUsize::new(0);
static POS: AtomicUsize = AtomicUsize::new(0);

// ── the old kernel ──────────────────────────────────────────────────────────

/// Start capturing into the buffer at `phys` (0 = no buffer).
pub fn start(phys: u64) {
    if phys == 0 { return; }
    let va = mm::phys_to_virt(phys as usize);
    unsafe {
        core::ptr::write_volatile(va as *mut u64, MAGIC);
        core::ptr::write_volatile((va + 8) as *mut u32, 0);
        core::ptr::write_volatile((va + 12) as *mut u32, CAP as u32);
    }
    POS.store(0, Ordering::Relaxed);
    VA.store(va, Ordering::Release);
    ACTIVE.store(true, Ordering::Release);
}

/// Stop without handing anything over (the kexec failed before shutdown).
pub fn cancel() { ACTIVE.store(false, Ordering::Release); }

/// Copy console bytes while capturing. Lock-free (any CPU, any context);
/// bytes past the capacity are dropped.
#[inline]
pub fn tee(b: &[u8]) {
    if !ACTIVE.load(Ordering::Relaxed) || b.is_empty() { return; }
    let va = VA.load(Ordering::Acquire);
    let p = POS.fetch_add(b.len(), Ordering::Relaxed);
    if p >= CAP { return; }
    let n = b.len().min(CAP - p);
    unsafe {
        core::ptr::copy_nonoverlapping(b.as_ptr(), (va + HDR + p) as *mut u8, n);
        // Monotonic enough: `seal` writes the final value.
        core::ptr::write_volatile((va + 8) as *mut u32, (p + n) as u32);
    }
}

/// Last thing before the purgatory jump (one CPU left): stop, write the
/// final length and (aarch64) clean the buffer to the point of coherency so
/// the next kernel reads it whatever the caches did in between.
pub fn seal() {
    if !ACTIVE.swap(false, Ordering::AcqRel) { return; }
    let va = VA.load(Ordering::Acquire);
    let len = POS.load(Ordering::Relaxed).min(CAP);
    unsafe { core::ptr::write_volatile((va + 8) as *mut u32, len as u32); }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        let mut p = va;
        while p < va + SIZE {
            core::arch::asm!("dc cvac, {}", in(reg) p, options(nostack));
            p += 64;
        }
        core::arch::asm!("dsb sy", options(nostack));
    }
}

// ── the new kernel ──────────────────────────────────────────────────────────

/// The range kept out of the allocator by [`reserve_early`] (start, end).
static HELD: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
/// The range lies in RAM this kernel manages, so it can be freed.
static FREEABLE: AtomicBool = AtomicBool::new(false);

/// Before `mm::init_with_map`: keep the previous kernel's buffer out of the
/// buddy allocator. Only a page-aligned range of plausible size that lies
/// wholly inside one Available region is accepted (anything else would be
/// unmapped or not ours to free).
/// Is `[s, e)` covered by the union of Available regions (adjacent regions
/// count as one)? Walks forward from `s` through regions that touch.
fn in_available_union(bi: &boot::BootInfo, s: u64, e: u64) -> bool {
    let mut cur = s;
    while cur < e {
        match bi.memory_regions().iter().find(|r| r.kind == boot::MemoryType::Available
            && cur >= r.base && cur < r.base + r.length) {
            Some(r) => cur = r.base + r.length,
            None => return false,
        }
    }
    true
}

pub fn reserve_early(bi: &boot::BootInfo) {
    let Some((p, s)) = boot::kexec_prevlog() else { return };
    let ok = p & 0xfff == 0 && s >= 4096 && s <= 64 * 1024 && s & 0xfff == 0
        && in_available_union(bi, p, p + s);
    crate::serial_print_str("[early] kexec prevlog: ");
    crate::serial_print_hex(p as usize);
    crate::serial_print_str(", ");
    crate::serial_print_hex(s as usize);
    if !ok {
        crate::serial_print_str(" B: not in usable RAM, ignored\n");
        for r in bi.memory_regions().iter().filter(|r| r.base < p + s && p < r.base + r.length) {
            crate::serial_print_str(&alloc::format!("[early]   overlapping map entry {:#x}-{:#x} kind {}\n",
                r.base, r.base + r.length, r.kind as u32));
        }
        return;
    }
    crate::serial_print_str(" B reserved until read\n");
    mm::buddy::reserve_range(p as usize, (p + s) as usize);
    HELD[0].store(p, Ordering::Relaxed);
    HELD[1].store(p + s, Ordering::Relaxed);
    FREEABLE.store(true, Ordering::Relaxed);
}

/// After `mm` is up: print the previous kernel's lines as `[prevkexec] ...`
/// (console + kernel log, so they reach the persistent log), then give the
/// pages to the allocator.
pub fn replay() {
    if !FREEABLE.swap(false, Ordering::AcqRel) { return; }
    let (s, e) = (HELD[0].load(Ordering::Relaxed) as usize, HELD[1].load(Ordering::Relaxed) as usize);
    let va = mm::phys_to_virt(s);
    let magic = unsafe { core::ptr::read_volatile(va as *const u64) };
    let len = unsafe { core::ptr::read_volatile((va + 8) as *const u32) } as usize;
    if magic != MAGIC {
        crate::serial_print_str("[prevkexec] no log in the handed-over buffer (bad magic)\n");
    } else {
        let len = len.min(e - s - HDR);
        let text = unsafe { core::slice::from_raw_parts((va + HDR) as *const u8, len) };
        let lines = text.split(|&b| b == b'\n').filter(|l| !l.is_empty()).count();
        print_line(alloc::format!("{} bytes, {} lines from the previous kernel's kexec:", len, lines).as_bytes());
        for l in text.split(|&b| b == b'\n') {
            if !l.is_empty() { print_line(l); }
        }
        print_line(b"end");
    }
    let pages = mm::buddy::release_reserved(s, e);
    crate::serial_print_str(&alloc::format!("[prevkexec] buffer {:#x}-{:#x} freed ({} pages)\n", s, e, pages));
}

/// One `[prevkexec] ` line, non-printable bytes as `?`, split at 200 bytes.
fn print_line(l: &[u8]) {
    for chunk in l.chunks(200) {
        let mut b = [0u8; 200 + 13];
        b[..12].copy_from_slice(b"[prevkexec] ");
        let mut n = 12;
        for &c in chunk {
            b[n] = if c == b'\t' || (0x20..0x7f).contains(&c) { c } else { b'?' };
            n += 1;
        }
        b[n] = b'\n';
        n += 1;
        if let Ok(s) = core::str::from_utf8(&b[..n]) { crate::serial_print_str(s); }
    }
}
