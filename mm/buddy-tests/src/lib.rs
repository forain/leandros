//! Host test: the kernel's buddy allocator (included verbatim) initialised
//! from a sanitized, deliberately hostile memory map -- 400 unsorted,
//! overlapping, partly unaligned entries, RAM out past 64 GiB -- then drained
//! and refilled. Checks that init neither faults nor corrupts its lists, that
//! it never hands out a page the map does not call usable, and that every
//! usable page is accounted for.

use std::sync::atomic::{AtomicUsize, Ordering};

#[path = "../../src/buddy.rs"]
pub mod buddy;

/// Base of the host mapping standing in for physical address 0.
static HHDM: AtomicUsize = AtomicUsize::new(0);
pub fn phys_to_virt(p: usize) -> usize { HHDM.load(Ordering::Relaxed) + p }

pub mod pagecache {
    pub fn emergency_reclaim(_pages: usize) -> usize { 0 }
}

#[no_mangle]
pub extern "C" fn serial_write_byte_direct(b: u8) {
    use std::io::Write;
    let _ = std::io::stderr().write_all(&[b]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use boot::memmap::*;
    use boot::{MemoryRegion, MemoryType::*};

    const PAGE: u64 = 4096;
    const GIB: u64 = 1 << 30;
    const SPAN: usize = 80 << 30;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 }
        fn below(&mut self, n: u64) -> u64 { self.next() % n }
    }

    fn r(base: u64, length: u64, kind: boot::MemoryType) -> MemoryRegion { MemoryRegion { base, length, kind } }

    fn hostile_map() -> Vec<MemoryRegion> {
        let mut g = Rng(0x5eed_f00d);
        let mut v = Vec::new();
        // ~200 small usable fragments below 4 GiB with holes between them.
        let mut addr = 0x1000u64;
        while v.len() < 200 {
            let len = (1 + g.below(256)) * PAGE + if g.below(6) == 0 { g.below(PAGE) } else { 0 };
            v.push(r(addr, len, Available));
            addr += len + if g.below(2) == 0 { (1 + g.below(32)) * PAGE } else { 0 };
        }
        // Big ranges above 4 GiB (one ending past 64 GiB), split oddly.
        v.push(r(4 * GIB, 30 * GIB, Available));
        v.push(r(34 * GIB, 31 * GIB + 0x123000, Available));
        v.push(r(65 * GIB + 0x200000, 3 * GIB, Available));
        // ~200 reserved/ACPI/bad punches anywhere, many overlapping usable.
        let kinds = [Reserved, AcpiNvs, AcpiReclaimable, BadMemory];
        while v.len() < 400 {
            let base = g.below(70 * GIB) & !0xFFF | if g.below(5) == 0 { g.below(PAGE) } else { 0 };
            let len = if g.below(12) == 0 { 0 } else { (1 + g.below(64)) * PAGE };
            v.push(r(base, len, kinds[g.below(4) as usize]));
        }
        v.push(r(u64::MAX - 0x5000, 0x10000, Reserved)); // wraps
        for i in (1..v.len()).rev() { let j = g.below(i as u64 + 1) as usize; v.swap(i, j); }
        v
    }

    #[test]
    fn buddy_on_sanitized_hostile_map() {
        // Lazily committed host memory standing in for 80 GiB of physical space.
        let layout = std::alloc::Layout::from_size_align(SPAN, 1 << 21).unwrap();
        let base = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!base.is_null(), "could not reserve the simulated physical span");
        HHDM.store(base as usize, Ordering::Relaxed);

        let input = hostile_map();
        assert!(input.len() >= 400);
        let mut scratch = input.clone();
        let mut bounds = vec![0u64; 2 * input.len()];
        let mut out = vec![MemoryRegion::EMPTY; 2 * input.len()];
        let st = sanitize(&mut scratch, &mut bounds, &mut out);
        out.truncate(st.output);
        eprintln!("sanitize: {:?}", st);
        assert!(st.unsorted && st.overlaps > 0 && st.wrapped == 1);
        assert!(out.iter().all(|e| e.base + e.length <= SPAN as u64 || e.kind != Available));

        // Expected free pages: usable pages at or above 1 MiB (LOW_RESERVED_END),
        // minus the free-head bitmap the allocator carves for itself.
        let usable = |p: u64| out.iter().any(|e| e.kind == Available && p >= e.base && p + PAGE <= e.base + e.length);
        let expect_pages: u64 = out.iter().filter(|e| e.kind == Available).map(|e| {
            let s = e.base.max(1 << 20);
            let end = e.base + e.length;
            if end > s { (end - s) / PAGE } else { 0 }
        }).sum();

        buddy::set_verbose(true);
        buddy::init_from_map(&out);
        buddy::init_site_tags();
        buddy::set_verbose(false);
        let total = buddy::total_pages() as u64;
        let top = out.iter().filter(|e| e.kind == Available).map(|e| e.base + e.length).max().unwrap();
        let bitmap_pages = (((top / PAGE).div_ceil(64) * 8 + PAGE - 1) / PAGE) as u64;
        assert_eq!(total, expect_pages - bitmap_pages, "pages handed to the buddy");

        // Drain everything at order 0 in chunks (via larger orders first so it
        // is quick), checking every block lies wholly inside usable RAM.
        let mut got: Vec<(usize, usize)> = Vec::new();
        for order in (0..buddy::MAX_ORDER).rev() {
            while let Some(a) = buddy::try_alloc(order) {
                let a64 = a as u64;
                let n = 1u64 << order;
                assert_eq!(a64 % (PAGE << order), 0, "misaligned block {:#x} order {}", a, order);
                // Check first, last and a middle page of the block.
                for p in [a64, a64 + (n - 1) * PAGE, a64 + (n / 2) * PAGE] {
                    assert!(usable(p), "allocated non-usable page {:#x} (block {:#x} order {})", p, a, order);
                }
                assert!(a64 >= 1 << 20, "allocated below 1 MiB: {:#x}", a);
                got.push((a, order));
            }
        }
        let alloc_pages: u64 = got.iter().map(|&(_, o)| 1u64 << o).sum();
        // init_site_tags took one block; everything else must have come back.
        assert_eq!(buddy::free_pages(), 0);
        let tag_bytes = buddy::phys_end() as u64 / PAGE;
        let tag_pages = tag_bytes.div_ceil(PAGE).next_power_of_two();
        assert_eq!(alloc_pages + tag_pages, total, "drained {} + tags {} of {}", alloc_pages, tag_pages, total);
        // No two allocations overlap.
        got.sort();
        for w in got.windows(2) {
            assert!(w[0].0 + (4096usize << w[0].1) <= w[1].0, "overlapping blocks {:x?}", w);
        }
        // Give it all back; it must coalesce without tripping a check.
        for &(a, o) in got.iter().rev() { buddy::free(a, o); }
        assert_eq!(buddy::free_pages() as u64, alloc_pages);
        assert_eq!(buddy::bad_frees(), 0);
        // And it can satisfy a 1 GiB block again (the big ranges re-merged).
        assert!(buddy::try_alloc(buddy::MAX_ORDER - 1).is_some());
    }
}
