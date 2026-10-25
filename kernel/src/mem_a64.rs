//! aarch64 bodies of the kernel's memcpy/memset/memmove: eight bytes per
//! access in the bulk, a byte head/tail around it.
//!
//! These were byte loops (one load and one store per byte), which is the whole
//! cost of the console blit, page copies and user copies.
//!
//! Constraints that shape this file:
//! - The kernel is built softfloat and saves no vector state on EL0 entry, so
//!   nothing here may touch FP/SIMD registers (no q-register copies).
//! - Every load and store is naturally aligned. The destination may be the
//!   framebuffer (Device / Normal-NC on real hardware and HVF), where an
//!   unaligned access faults. When source and destination disagree mod 8 the
//!   destination is aligned and the source is read as aligned doublewords and
//!   merged with shifts; the extra bytes read lie in the same aligned
//!   doubleword as wanted bytes, so never on another page.
//! - Only single-register `ldr`/`str` with immediate offsets, no `ldp`/`stp`
//!   and no writeback addressing: those forms report no instruction syndrome
//!   (ISV=0), which a hypervisor cannot emulate if the access ever traps.
//! - Head/tail bytes go through volatile accesses so LLVM cannot turn the
//!   loops back into memcpy/memset calls (these ARE those symbols).
//!
//! Tested on the host by kernel/memtest (native on Apple silicon): all lengths
//! 0..=80, every misalignment, overlap at every distance in both directions.

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};

/// Below this many bytes the alignment work does not pay.
const SMALL: usize = 16;

#[inline(always)]
unsafe fn bytes_fwd(d: *mut u8, s: *const u8, n: usize) {
    let mut i = 0;
    while i < n {
        write_volatile(d.add(i), read_volatile(s.add(i)));
        i += 1;
    }
}

#[inline(always)]
unsafe fn bytes_bwd(d: *mut u8, s: *const u8, n: usize) {
    let mut i = n;
    while i > 0 {
        i -= 1;
        write_volatile(d.add(i), read_volatile(s.add(i)));
    }
}

/// Copy `w` doublewords upward. `d` must be 8-aligned. `s` may have any
/// alignment; it is read only through aligned doublewords. Safe for
/// overlapping ranges with `d < s`.
#[inline(always)]
unsafe fn words_fwd(d: *mut u8, s: *const u8, w: usize) {
    if w == 0 {
        return;
    }
    let sh = ((s as usize) & 7) * 8;
    if sh == 0 {
        asm!(
            "2:",
            "cmp {w}, #4",
            "b.lo 3f",
            "ldr {a}, [{s}]",
            "ldr {b}, [{s}, #8]",
            "ldr {c}, [{s}, #16]",
            "ldr {e}, [{s}, #24]",
            "str {a}, [{d}]",
            "str {b}, [{d}, #8]",
            "str {c}, [{d}, #16]",
            "str {e}, [{d}, #24]",
            "add {s}, {s}, #32",
            "add {d}, {d}, #32",
            "sub {w}, {w}, #4",
            "b 2b",
            "3:",
            "cbz {w}, 4f",
            "ldr {a}, [{s}]",
            "str {a}, [{d}]",
            "add {s}, {s}, #8",
            "add {d}, {d}, #8",
            "sub {w}, {w}, #1",
            "b 3b",
            "4:",
            w = inout(reg) w => _,
            s = inout(reg) s => _,
            d = inout(reg) d => _,
            a = out(reg) _, b = out(reg) _, c = out(reg) _, e = out(reg) _,
            options(nostack),
        );
    } else {
        // out = (lo >> sh) | (hi << (64 - sh)), little-endian.
        asm!(
            "ldr {lo}, [{s}]",
            "2:",
            "cmp {w}, #2",
            "b.lo 3f",
            "ldr {hi}, [{s}, #8]",
            "ldr {h2}, [{s}, #16]",
            "lsr {t}, {lo}, {sh}",
            "lsl {u}, {hi}, {rsh}",
            "orr {t}, {t}, {u}",
            "lsr {u}, {hi}, {sh}",
            "lsl {lo}, {h2}, {rsh}",
            "orr {u}, {u}, {lo}",
            "str {t}, [{d}]",
            "str {u}, [{d}, #8]",
            "mov {lo}, {h2}",
            "add {s}, {s}, #16",
            "add {d}, {d}, #16",
            "sub {w}, {w}, #2",
            "b 2b",
            "3:",
            "cbz {w}, 4f",
            "ldr {hi}, [{s}, #8]",
            "lsr {t}, {lo}, {sh}",
            "lsl {u}, {hi}, {rsh}",
            "orr {t}, {t}, {u}",
            "str {t}, [{d}]",
            "4:",
            w = inout(reg) w => _,
            s = inout(reg) (s as usize & !7) => _,
            d = inout(reg) d => _,
            sh = in(reg) sh,
            rsh = in(reg) 64 - sh,
            lo = out(reg) _, hi = out(reg) _, h2 = out(reg) _,
            t = out(reg) _, u = out(reg) _,
            options(nostack),
        );
    }
}

/// Copy `w` doublewords downward, ending just below `de` / `se`. `de` must be
/// 8-aligned; `se` may have any alignment. Safe for overlap with `de > se`.
#[inline(always)]
unsafe fn words_bwd(de: *mut u8, se: *const u8, w: usize) {
    if w == 0 {
        return;
    }
    let sh = ((se as usize) & 7) * 8;
    if sh == 0 {
        asm!(
            "2:",
            "cmp {w}, #4",
            "b.lo 3f",
            "ldr {a}, [{s}, #-8]",
            "ldr {b}, [{s}, #-16]",
            "ldr {c}, [{s}, #-24]",
            "ldr {e}, [{s}, #-32]",
            "str {a}, [{d}, #-8]",
            "str {b}, [{d}, #-16]",
            "str {c}, [{d}, #-24]",
            "str {e}, [{d}, #-32]",
            "sub {s}, {s}, #32",
            "sub {d}, {d}, #32",
            "sub {w}, {w}, #4",
            "b 2b",
            "3:",
            "cbz {w}, 4f",
            "ldr {a}, [{s}, #-8]",
            "str {a}, [{d}, #-8]",
            "sub {s}, {s}, #8",
            "sub {d}, {d}, #8",
            "sub {w}, {w}, #1",
            "b 3b",
            "4:",
            w = inout(reg) w => _,
            s = inout(reg) se => _,
            d = inout(reg) de => _,
            a = out(reg) _, b = out(reg) _, c = out(reg) _, e = out(reg) _,
            options(nostack),
        );
    } else {
        // `s` is se rounded down; `hi` holds the doubleword at s, `lo` the one
        // below it; the eight bytes ending at se are (lo >> sh) | (hi << (64-sh)).
        asm!(
            "ldr {hi}, [{s}]",
            "2:",
            "cbz {w}, 4f",
            "ldr {lo}, [{s}, #-8]",
            "lsr {t}, {lo}, {sh}",
            "lsl {u}, {hi}, {rsh}",
            "orr {t}, {t}, {u}",
            "str {t}, [{d}, #-8]",
            "mov {hi}, {lo}",
            "sub {s}, {s}, #8",
            "sub {d}, {d}, #8",
            "sub {w}, {w}, #1",
            "b 2b",
            "4:",
            w = inout(reg) w => _,
            s = inout(reg) (se as usize & !7) => _,
            d = inout(reg) de => _,
            sh = in(reg) sh,
            rsh = in(reg) 64 - sh,
            lo = out(reg) _, hi = out(reg) _,
            t = out(reg) _, u = out(reg) _,
            options(nostack),
        );
    }
}

#[inline(always)]
pub unsafe fn fill(dest: *mut u8, val: u8, count: usize) -> *mut u8 {
    if count < SMALL {
        for i in 0..count {
            write_volatile(dest.add(i), val);
        }
        return dest;
    }
    let head = (dest as usize).wrapping_neg() & 7;
    for i in 0..head {
        write_volatile(dest.add(i), val);
    }
    let d = dest.add(head);
    let n = count - head;
    let pat = (val as u64).wrapping_mul(0x0101_0101_0101_0101);
    asm!(
        "2:",
        "cmp {w}, #4",
        "b.lo 3f",
        "str {p}, [{d}]",
        "str {p}, [{d}, #8]",
        "str {p}, [{d}, #16]",
        "str {p}, [{d}, #24]",
        "add {d}, {d}, #32",
        "sub {w}, {w}, #4",
        "b 2b",
        "3:",
        "cbz {w}, 4f",
        "str {p}, [{d}]",
        "add {d}, {d}, #8",
        "sub {w}, {w}, #1",
        "b 3b",
        "4:",
        w = inout(reg) n >> 3 => _,
        d = inout(reg) d => _,
        p = in(reg) pat,
        options(nostack),
    );
    let t = d.add(n & !7);
    for i in 0..(n & 7) {
        write_volatile(t.add(i), val);
    }
    dest
}

#[inline(always)]
pub unsafe fn copy_forward(dest: *mut u8, src: *const u8, count: usize) -> *mut u8 {
    if count < SMALL {
        bytes_fwd(dest, src, count);
        return dest;
    }
    let head = (dest as usize).wrapping_neg() & 7;
    bytes_fwd(dest, src, head);
    let (d, s, n) = (dest.add(head), src.add(head), count - head);
    words_fwd(d, s, n >> 3);
    let done = n & !7;
    bytes_fwd(d.add(done), s.add(done), n & 7);
    dest
}

#[inline(always)]
pub unsafe fn copy_overlapping(dest: *mut u8, src: *const u8, count: usize) -> *mut u8 {
    // Forward is safe whenever dest is below src or the ranges are disjoint:
    // every chunk is loaded before it is stored and every store lands below
    // every byte still to be read.
    if (dest as usize).wrapping_sub(src as usize) >= count {
        return copy_forward(dest, src, count);
    }
    if count < SMALL {
        bytes_bwd(dest, src, count);
        return dest;
    }
    // dest overlaps above src: top-down. Tail bytes until the destination end
    // is 8-aligned, whole doublewords, then the low head bytes.
    let tail = (dest as usize + count) & 7;
    let n = count - tail;
    bytes_bwd(dest.add(n), src.add(n), tail);
    words_bwd(dest.add(n), src.add(n), n >> 3);
    bytes_bwd(dest, src, n & 7);
    dest
}
