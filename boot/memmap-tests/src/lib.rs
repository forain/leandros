//! Host tests for the kernel's memory-map sanitizer. The module under test is
//! included verbatim, so these exercise exactly the code the kernel runs.

#[path = "../../src/memmap.rs"]
pub mod memmap;

#[cfg(test)]
mod tests {
    use super::memmap::*;
    use std::collections::BTreeSet;

    const PAGE: u64 = 4096;
    const GIB: u64 = 1 << 30;

    fn r(base: u64, length: u64, kind: MemoryType) -> MemoryRegion { MemoryRegion { base, length, kind } }
    use MemoryType::*;

    fn run(input: &[MemoryRegion]) -> (Vec<MemoryRegion>, Stats) {
        let mut scratch = input.to_vec();
        let mut bounds = vec![0u64; 2 * input.len()];
        let mut out = vec![MemoryRegion::EMPTY; 2 * input.len() + 1];
        let st = sanitize(&mut scratch, &mut bounds, &mut out);
        out.truncate(st.output);
        (out, st)
    }

    fn prio(k: MemoryType) -> u8 {
        match k { Available => 0, AcpiReclaimable => 1, AcpiNvs => 2, Reserved => 3, BadMemory => 4 }
    }

    /// Independent reference: the kind at byte `p` = most restrictive input
    /// entry covering it (wrapping entries clamped like the sanitizer does).
    fn kind_at(input: &[MemoryRegion], p: u64) -> Option<MemoryType> {
        let mut k: Option<MemoryType> = None;
        for e in input {
            if e.length == 0 { continue; }
            let end = e.base.saturating_add(e.length);
            let end = if e.base.checked_add(e.length).is_none() { u64::MAX } else { end };
            if p >= e.base && p < end {
                k = Some(match k { Some(o) if prio(o) >= prio(e.kind) => o, _ => e.kind });
            }
        }
        k
    }

    /// Reference usable bytes: elementary intervals via a BTreeSet, merged
    /// runs of Available, page-aligned inward, clipped at MAX_PHYS.
    fn reference_usable(input: &[MemoryRegion]) -> u64 {
        let mut b = BTreeSet::new();
        for e in input {
            if e.length == 0 { continue; }
            b.insert(e.base);
            b.insert(e.base.checked_add(e.length).unwrap_or(u64::MAX));
        }
        let b: Vec<u64> = b.into_iter().collect();
        let mut runs: Vec<(u64, u64)> = Vec::new();
        for w in b.windows(2) {
            if kind_at(input, w[0]) == Some(Available) {
                match runs.last_mut() {
                    Some(last) if last.1 == w[0] => last.1 = w[1],
                    _ => runs.push((w[0], w[1])),
                }
            }
        }
        runs.iter().map(|&(s, e)| {
            let s = (s + PAGE - 1) & !(PAGE - 1);
            let e = (e & !(PAGE - 1)).min(MAX_PHYS);
            e.saturating_sub(s)
        }).sum()
    }

    fn check_invariants(input: &[MemoryRegion], out: &[MemoryRegion]) {
        for w in out.windows(2) {
            assert!(w[0].base + w[0].length <= w[1].base, "overlap/unsorted: {:x?} {:x?}", w[0], w[1]);
        }
        for o in out {
            assert!(o.length > 0, "zero-length output {:x?}", o);
            assert!(o.base.checked_add(o.length).is_some(), "wrapping output {:x?}", o);
            if o.kind == Available {
                assert_eq!(o.base % PAGE, 0, "unaligned usable {:x?}", o);
                assert_eq!(o.length % PAGE, 0, "unaligned usable {:x?}", o);
                assert!(o.base + o.length <= MAX_PHYS);
                // Every byte of a usable output must be usable in the input:
                // sample the ends, the middle and each input boundary inside.
                let mut pts = vec![o.base, o.base + o.length - 1, o.base + o.length / 2];
                for e in input {
                    for p in [e.base, e.base.wrapping_add(e.length), e.base.wrapping_add(e.length).wrapping_sub(1)] {
                        if p >= o.base && p < o.base + o.length { pts.push(p); }
                    }
                }
                for p in pts {
                    assert_eq!(kind_at(input, p), Some(Available), "usable output {:x?} covers {:#x} which is {:?}", o, p, kind_at(input, p));
                }
            }
        }
        // Total usable matches the reference exactly.
        let got: u64 = out.iter().filter(|o| o.kind == Available).map(|o| o.length).sum();
        assert_eq!(got, reference_usable(input), "usable bytes differ from reference");
        // Adjacent same-kind entries are merged.
        for w in out.windows(2) {
            assert!(!(w[0].kind == w[1].kind && w[0].base + w[0].length == w[1].base), "unmerged {:x?} {:x?}", w[0], w[1]);
        }
    }

    #[test]
    fn simple_overlap_reserved_wins() {
        let input = [r(0x10_0000, 0x100_0000, Available), r(0x80_0000, 0x1000, Reserved)];
        let (out, st) = run(&input);
        check_invariants(&input, &out);
        assert_eq!(st.conflicts, 1);
        assert_eq!(out.len(), 3);
        assert_eq!((out[0].base, out[0].length, out[0].kind), (0x10_0000, 0x70_0000, Available));
        assert_eq!((out[1].base, out[1].length, out[1].kind), (0x80_0000, 0x1000, Reserved));
        assert_eq!((out[2].base, out[2].kind), (0x80_1000, Available));
    }

    #[test]
    fn zero_wrap_unaligned_unsorted() {
        let input = [
            r(0x2000_0000, 0x1000_0000, Available),
            r(0x100_0000, 0, Available),                 // zero length
            r(0x1234, 0x10_0000, Available),             // unaligned
            r(u64::MAX - 0x1fff, 0x10_0000, Reserved),   // wraps
            r(0x40_0000_0000, 0x1_0000_0000, Available), // 256 GiB+ (<MAX_PHYS)
            r(MAX_PHYS - 0x1000, 0x10000, Available),    // straddles MAX_PHYS
        ];
        let (out, st) = run(&input);
        check_invariants(&input, &out);
        assert!(st.unsorted);
        assert_eq!(st.zero_len, 1);
        assert_eq!(st.wrapped, 1);
        assert!(st.unaligned >= 1);
        assert!(st.dropped_usable >= 1);
        let u: Vec<_> = out.iter().filter(|o| o.kind == Available).collect();
        assert_eq!(u[0].base, 0x2000);
        assert_eq!(u[0].base + u[0].length, 0x10_1000);
        assert_eq!(u.last().unwrap().base + u.last().unwrap().length, MAX_PHYS);
    }

    /// A copy of the HP EliteBook 840 G5's Fast-Boot-ON map (kinds collapsed
    /// the way `limine_kind` does), from the stick's klog of 2026-10-16.
    fn hp_fastboot_on() -> Vec<MemoryRegion> {
        let raw: &[(u64, u64, MemoryType)] = &[
            (0x0, 0x1000, Reserved), (0x1000, 0x17000, Available), (0x18000, 0x40000, Reserved),
            (0x58000, 0x1000, Reserved), (0x59000, 0x33000, Available), (0x8C000, 0x12000, Reserved),
            (0x9E000, 0x1000, Reserved), (0x9F000, 0x1000, Reserved), (0xA0000, 0x60000, Reserved),
            (0x100000, 0xB00000, Available), (0xC00000, 0x422000, Reserved), (0x1022000, 0x3EFDE000, Available),
            (0x40000000, 0x400000, Reserved), (0x40400000, 0x5757E000, Available), (0x9797E000, 0x1000, Reserved),
            (0x9797F000, 0x379000, Available), (0x97CF8000, 0x1000, Reserved), (0x97CF9000, 0x86ED000, Available),
            (0xA03E6000, 0x20000, Reserved), (0xA0406000, 0x1720000, Available), (0xA1B26000, 0x37C7000, Reserved),
            (0xA52ED000, 0x1000, AcpiNvs), (0xA52EE000, 0x1000, Reserved), (0xA52EF000, 0xE7000, Reserved),
            (0xA53D6000, 0x5000, Available), (0xA53DB000, 0xA000, Reserved), (0xA53E5000, 0x4AAD000, Available),
            (0xA9E92000, 0x1680000, Reserved), (0xAB512000, 0x7AD000, Reserved), (0xABCBF000, 0x3A00000, Available),
            (0xAF6BF000, 0x602000, Reserved), (0xAFCC1000, 0x35F000, Available), (0xB0020000, 0x1D1F000, Reserved),
            (0xB1D3F000, 0x77000, Available), (0xB1DB6000, 0x20000, Reserved), (0xB1DD6000, 0x1D000, Available),
            (0xB1DF3000, 0x35A6000, Reserved), (0xB5399000, 0x300000, Available), (0xB5699000, 0x1502000, Reserved),
            (0xB6B9B000, 0x7E4000, Reserved), (0xB737F000, 0x600000, Reserved), (0xB797F000, 0x200000, AcpiNvs),
            (0xB7B7F000, 0x80000, AcpiReclaimable), (0xB7BFF000, 0x1000, Reserved), (0xB7C00000, 0x6C00000, Reserved),
            (0xC0000000, 0x300000, Reserved), (0xF0000000, 0x8000000, Reserved), (0xFE010000, 0x1000, Reserved),
            (0xFED20000, 0x60000, Reserved), (0x100000000, 0x33F800000, Available),
        ];
        raw.iter().map(|&(b, l, k)| r(b, l, k)).collect()
    }

    #[test]
    fn hp_fast_boot_on_map_is_preserved() {
        let input = hp_fastboot_on();
        assert_eq!(input.len(), 50);
        let (out, st) = run(&input);
        check_invariants(&input, &out);
        assert!(!st.unsorted);
        assert_eq!(st.overlaps, 0);
        assert_eq!(st.unaligned, 0);
        let usable_in: u64 = input.iter().filter(|e| e.kind == Available).map(|e| e.length).sum();
        let usable_out: u64 = out.iter().filter(|e| e.kind == Available).map(|e| e.length).sum();
        assert_eq!(usable_in, usable_out);
        // Adjacent reserved entries merge; usable ones never do here.
        assert_eq!(out.iter().filter(|e| e.kind == Available).count(),
                   input.iter().filter(|e| e.kind == Available).count());
        assert!(out.len() < input.len());
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 }
        fn below(&mut self, n: u64) -> u64 { self.next() % n }
    }

    /// A full-init firmware map, made worse: the HP map fragmented into many
    /// more entries, shuffled, with overlaps, duplicates, zero-length and
    /// unaligned entries and RAM above 4 GiB out to ~70 GiB.
    fn pathological(seed: u64, n: usize) -> Vec<MemoryRegion> {
        let mut g = Rng(seed);
        let mut v = Vec::new();
        // Base: 0..70 GiB of usable split into n/2 pieces, with holes.
        let mut addr = 0x1000u64;
        while v.len() < n / 2 {
            let len = (1 + g.below(512)) * PAGE + if g.below(8) == 0 { g.below(PAGE) } else { 0 };
            let gap = if g.below(3) == 0 { (1 + g.below(64)) * PAGE } else { 0 };
            v.push(r(addr, len, Available));
            addr += len + gap;
            if g.below(40) == 0 { addr += g.below(8) * GIB; } // jump past 4 GiB etc.
        }
        v.push(r(4 * GIB, 66 * GIB, Available)); // the big DIMM range
        let kinds = [Reserved, AcpiNvs, AcpiReclaimable, BadMemory, Available];
        while v.len() < n {
            let base = g.below(72 * GIB) & !0xFFF;
            let base = if g.below(6) == 0 { base | g.below(PAGE) } else { base };
            let len = match g.below(10) {
                0 => 0,
                1 => u64::MAX - g.below(1 << 20),          // wraps
                2 => g.below(4 * GIB),
                _ => (1 + g.below(64)) * PAGE,
            };
            v.push(r(base, len, kinds[g.below(kinds.len() as u64) as usize]));
            if g.below(10) == 0 { let d = *v.last().unwrap(); v.push(d); } // duplicate
        }
        // Shuffle.
        for i in (1..v.len()).rev() { let j = g.below(i as u64 + 1) as usize; v.swap(i, j); }
        v
    }

    #[test]
    fn pathological_maps() {
        for seed in 1..=25u64 {
            for &n in &[64usize, 300, 400, 1024] {
                let input = pathological(seed * 7919 + n as u64, n);
                let (out, st) = run(&input);
                check_invariants(&input, &out);
                assert_eq!(st.out_truncated, 0);
                assert!(st.overlaps > 0, "the generator should overlap entries");
            }
        }
    }

    #[test]
    fn output_capacity_overflow_is_counted_not_fatal() {
        // 300 disjoint usable/reserved alternations into a 50-entry buffer.
        let mut input = Vec::new();
        for i in 0..300u64 {
            input.push(r(0x10_0000 + i * 0x2000, 0x1000, Available));
            input.push(r(0x10_1000 + i * 0x2000, 0x1000, Reserved));
        }
        let mut scratch = input.clone();
        let mut bounds = vec![0u64; 2 * input.len()];
        let mut out = vec![MemoryRegion::EMPTY; 50];
        let st = sanitize(&mut scratch, &mut bounds, &mut out);
        assert_eq!(st.output, 50);
        assert!(st.out_truncated > 0);
        for w in out.windows(2) { assert!(w[0].base + w[0].length <= w[1].base); }
    }

    #[test]
    fn empty_and_all_zero() {
        let (out, st) = run(&[]);
        assert!(out.is_empty());
        assert_eq!(st.output, 0);
        let (out, st) = run(&[r(0x1000, 0, Available), r(0, 0, Reserved)]);
        assert!(out.is_empty());
        assert_eq!(st.zero_len, 2);
    }
}
