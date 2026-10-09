//! Host tests for the GPU-UAPI logic the kernel runs. The modules under test
//! are included verbatim, so these exercise exactly the kernel's code.

extern crate alloc;

#[path = "../../src/syncobj_core.rs"]
pub mod syncobj_core;

#[path = "../../nvidia/src/uapi_vm.rs"]
pub mod uapi_vm;

#[path = "../../src/sync_file_core.rs"]
pub mod sync_file_core;

#[cfg(test)]
mod syncobj_tests {
    use super::syncobj_core::*;
    use std::cell::RefCell;
    use std::collections::BTreeSet;

    /// A fence oracle: the set of signalled ids.
    struct Done(RefCell<BTreeSet<u64>>);
    impl Done {
        fn new() -> Self { Done(RefCell::new(BTreeSet::new())) }
        fn signal(&self, f: u64) { self.0.borrow_mut().insert(f); }
        fn f(&self) -> impl Fn(u64) -> bool + '_ { move |x| self.0.borrow().contains(&x) }
    }

    #[test]
    fn binary_lifecycle() {
        let d = Done::new();
        let mut p = Payload::null();
        assert!(p.is_null());
        assert_eq!(p.wait_state(0, 0, &d.f()), WaitState::Invalid);
        assert_eq!(p.wait_state(0, WAIT_FOR_SUBMIT, &d.f()), WaitState::Pending);
        p.replace(Fence::One(7));
        assert_eq!(p.wait_state(0, 0, &d.f()), WaitState::Pending);
        d.signal(7);
        assert_eq!(p.wait_state(0, 0, &d.f()), WaitState::Signaled);
        p.reset();
        assert_eq!(p.wait_state(0, 0, &d.f()), WaitState::Invalid);
        p.replace(Fence::Signaled);
        assert_eq!(p.wait_state(0, 0, &d.f()), WaitState::Signaled);
        assert_eq!(Payload::signaled().wait_state(0, 0, &d.f()), WaitState::Signaled);
        // A binary payload queries as 0 and is never collected.
        p.gc(&d.f());
        assert_eq!(p.query(&d.f()), 0);
        assert!(!p.is_null());
    }

    #[test]
    fn timeline_points_and_query() {
        let d = Done::new();
        let mut p = Payload::null();
        p.add_point(1, Fence::One(101));
        p.add_point(2, Fence::One(102));
        p.add_point(5, Fence::One(105));
        assert_eq!(p.last_point(), 5);
        assert_eq!(p.query(&d.f()), 0);
        // Out-of-order retirement: point 2 done, 1 not -> still 0.
        d.signal(102);
        assert_eq!(p.query(&d.f()), 0);
        assert_eq!(p.wait_state(2, 0, &d.f()), WaitState::Pending);
        d.signal(101);
        assert_eq!(p.query(&d.f()), 2);
        assert_eq!(p.wait_state(2, 0, &d.f()), WaitState::Signaled);
        // Point 3 is satisfied by point 5's link (the first >= 3).
        assert_eq!(p.wait_state(3, 0, &d.f()), WaitState::Pending);
        d.signal(105);
        assert_eq!(p.wait_state(3, 0, &d.f()), WaitState::Signaled);
        assert_eq!(p.query(&d.f()), 5);
        // Beyond the last point.
        assert_eq!(p.wait_state(6, 0, &d.f()), WaitState::Invalid);
        assert_eq!(p.wait_state(6, WAIT_FOR_SUBMIT, &d.f()), WaitState::Pending);
        assert_eq!(p.wait_state(6, WAIT_AVAILABLE, &d.f()), WaitState::Pending);
        p.add_point(6, Fence::One(106));
        // Available but not signalled.
        assert_eq!(p.wait_state(6, WAIT_AVAILABLE, &d.f()), WaitState::Signaled);
        assert_eq!(p.wait_state(6, WAIT_FOR_SUBMIT, &d.f()), WaitState::Pending);
    }

    #[test]
    fn timeline_gc_keeps_answers() {
        let d = Done::new();
        let mut p = Payload::null();
        for i in 1..=40u64 {
            p.add_point(i, Fence::One(1000 + i));
        }
        for i in 1..=30u64 {
            d.signal(1000 + i);
        }
        let q = p.query(&d.f());
        p.gc(&d.f());
        assert_eq!(p.len(), 10);
        assert_eq!(p.base(), 30);
        assert_eq!(p.query(&d.f()), q);
        assert_eq!(p.wait_state(12, 0, &d.f()), WaitState::Signaled);
        assert_eq!(p.lookup(12, &d.f()), Lookup::Ready(Fence::Signaled));
        assert_eq!(p.last_point(), 40);
        // Everything signalled and collected: head is still a signalled fence.
        for i in 31..=40u64 {
            d.signal(1000 + i);
        }
        p.gc(&d.f());
        assert_eq!(p.len(), 0);
        assert_eq!(p.head(), Some(Fence::Signaled));
        assert_eq!(p.wait_state(0, 0, &d.f()), WaitState::Signaled);
        assert_eq!(p.query(&d.f()), 40);
    }

    #[test]
    fn lookup_takes_the_whole_prefix() {
        let d = Done::new();
        let mut p = Payload::null();
        p.add_point(1, Fence::One(11));
        p.add_point(2, Fence::One(12));
        p.add_point(3, Fence::One(13));
        // A transfer of point 2 must stand for points 1 and 2.
        assert_eq!(p.lookup(2, &d.f()), Lookup::Ready(Fence::All(vec![11, 12])));
        d.signal(11);
        assert_eq!(p.lookup(2, &d.f()), Lookup::Ready(Fence::One(12)));
        d.signal(12);
        assert_eq!(p.lookup(2, &d.f()), Lookup::Ready(Fence::Signaled));
        // Point 0 = the head (binary view of a timeline).
        assert_eq!(p.lookup(0, &d.f()), Lookup::Ready(Fence::One(13)));
        // A transferred All fence into a binary payload.
        let mut b = Payload::null();
        if let Lookup::Ready(f) = p.lookup(3, &d.f()) { b.replace(f); }
        assert_eq!(b.wait_state(0, 0, &d.f()), WaitState::Pending);
        d.signal(13);
        assert_eq!(b.wait_state(0, 0, &d.f()), WaitState::Signaled);
    }

    #[test]
    fn cpu_signal_and_out_of_order_add() {
        let d = Done::new();
        let mut p = Payload::null();
        p.add_point(4, Fence::Signaled);
        assert_eq!(p.query(&d.f()), 4);
        p.add_point(2, Fence::One(52)); // late, lower point
        assert_eq!(p.query(&d.f()), 0);
        d.signal(52);
        assert_eq!(p.query(&d.f()), 4);
        // Re-signalling an existing point replaces its fence.
        p.add_point(4, Fence::One(54));
        assert_eq!(p.query(&d.f()), 2);
    }

    #[test]
    fn fence_helpers() {
        let d = Done::new();
        assert_eq!(Fence::from_ids(vec![]), Fence::Signaled);
        assert_eq!(Fence::from_ids(vec![0, 3, 3]), Fence::One(3));
        assert_eq!(Fence::from_ids(vec![5, 3]), Fence::All(vec![3, 5]));
        d.signal(3);
        assert_eq!(Fence::All(vec![3, 5]).pending(&d.f()), vec![5]);
        assert_eq!(Fence::All(vec![3, 5]).as_single(&d.f()), 5);
        assert_eq!(Fence::One(3).as_single(&d.f()), 3);
        assert_eq!(Fence::Signaled.as_single(&d.f()), 0);
    }
}

#[cfg(test)]
mod vm_tests {
    use super::uapi_vm::*;

    /// Records RM calls; mappings/regions are ids.
    #[derive(Default)]
    struct Fake {
        next: u32,
        live: Vec<(u32, u64, u64, Backing, Option<u32>)>,
        regions: Vec<(u32, u64, u64)>,
        log: Vec<String>,
        fail_map_at: Option<u64>,
    }

    impl VmOps for Fake {
        type Map = u32;
        type Sparse = u32;
        fn map(&mut self, start: u64, len: u64, b: Backing, sparse: Option<&u32>) -> Result<u32, i32> {
            if self.fail_map_at == Some(start) { return Err(12); }
            self.next += 1;
            self.live.push((self.next, start, len, b, sparse.copied()));
            self.log.push(format!("map {:#x}+{:#x} bo{}@{:#x} k{} s{:?}", start, len, b.bo, b.offset, b.kind, sparse));
            Ok(self.next)
        }
        fn unmap(&mut self, m: u32, sparse: Option<&u32>) {
            let i = self.live.iter().position(|e| e.0 == m).expect("unmap of a live mapping");
            assert_eq!(self.live[i].4, sparse.copied(), "unmap with the sparse parent it was mapped in");
            let e = self.live.remove(i);
            self.log.push(format!("unmap {:#x}+{:#x}", e.1, e.2));
        }
        fn sparse_new(&mut self, start: u64, len: u64) -> Result<u32, i32> {
            self.next += 1;
            self.regions.push((self.next, start, len));
            Ok(self.next)
        }
        fn sparse_free(&mut self, s: u32) {
            let i = self.regions.iter().position(|e| e.0 == s).unwrap();
            self.regions.remove(i);
        }
    }

    const K0: u64 = 1 << 39;
    const K1: u64 = 1 << 40;

    fn vm() -> VmState<u32, u32> {
        VmState::new(0x10_0000, 1 << 48, K0, K1)
    }

    fn b(bo: u32, off: u64) -> Backing { Backing { bo, offset: off, kind: 0 } }

    /// Every live RM mapping matches the bookkeeping, and nothing overlaps.
    fn consistent(v: &VmState<u32, u32>, f: &Fake) {
        assert_eq!(v.maps.len(), f.live.len());
        let mut prev_end = 0;
        for (s, m) in &v.maps {
            assert!(*s >= prev_end, "overlap at {:#x}", s);
            prev_end = m.end;
            let e = f.live.iter().find(|e| e.0 == m.tok).expect("RM mapping");
            assert_eq!((e.1, e.2, e.3), (*s, m.end - s, m.back));
        }
    }

    #[test]
    fn map_unmap_simple() {
        let (mut v, mut f) = (vm(), Fake::default());
        v.op_map(&mut f, 0x4000_0000, 0x10000, b(1, 0)).unwrap();
        v.op_map(&mut f, 0x4001_0000, 0x10000, b(2, 0x2000)).unwrap();
        consistent(&v, &f);
        v.op_unmap(&mut f, 0x4000_0000, 0x20000).unwrap();
        assert!(v.maps.is_empty() && f.live.is_empty());
    }

    #[test]
    fn partial_unmap_splits() {
        let (mut v, mut f) = (vm(), Fake::default());
        v.op_map(&mut f, 0x10_0000, 0x10000, b(7, 0x1000)).unwrap();
        // Punch a hole in the middle.
        v.op_unmap(&mut f, 0x10_4000, 0x2000).unwrap();
        consistent(&v, &f);
        let pieces: Vec<_> = v.maps.iter().map(|(s, m)| (*s, m.end, m.back.offset)).collect();
        assert_eq!(pieces, vec![(0x10_0000, 0x10_4000, 0x1000), (0x10_6000, 0x11_0000, 0x7000)]);
        // Trim the head of the second piece.
        v.op_unmap(&mut f, 0x10_6000, 0x1000).unwrap();
        consistent(&v, &f);
        assert_eq!(v.maps.get(&0x10_7000).unwrap().back.offset, 0x8000);
    }

    #[test]
    fn map_replaces_overlap() {
        let (mut v, mut f) = (vm(), Fake::default());
        v.op_map(&mut f, 0x20_0000, 0x8000, b(1, 0)).unwrap();
        v.op_map(&mut f, 0x20_4000, 0x8000, b(2, 0)).unwrap();
        consistent(&v, &f);
        let starts: Vec<_> = v.maps.keys().copied().collect();
        assert_eq!(starts, vec![0x20_0000, 0x20_4000]);
        assert_eq!(v.maps[&0x20_0000].end, 0x20_4000);
        assert_eq!(v.maps[&0x20_4000].back.bo, 2);
    }

    #[test]
    fn validation() {
        let (mut v, mut f) = (vm(), Fake::default());
        assert_eq!(v.op_map(&mut f, 0x20_0001, 0x1000, b(1, 0)), Err(EINVAL));
        assert_eq!(v.op_map(&mut f, 0x20_0000, 0, b(1, 0)), Err(EINVAL));
        assert_eq!(v.op_map(&mut f, 0x20_0000, 0x1000, b(1, 0x10)), Err(EINVAL));
        assert_eq!(v.op_map(&mut f, 0x1000, 0x1000, b(1, 0)), Err(EINVAL)); // below lo
        assert_eq!(v.op_map(&mut f, K0 - 0x1000, 0x2000, b(1, 0)), Err(EINVAL)); // into the kernel window
        assert_eq!(v.op_map(&mut f, K0 + 0x10000, 0x1000, b(1, 0)), Err(EINVAL));
        assert!(f.live.is_empty());
        // Unmapping nothing is fine.
        v.op_unmap(&mut f, 0x30_0000, 0x1000).unwrap();
    }

    #[test]
    fn sparse_regions() {
        let (mut v, mut f) = (vm(), Fake::default());
        v.op_sparse_map(&mut f, 0x100_0000, 0x10_0000).unwrap();
        // Overlapping region refused; a map crossing its boundary refused.
        assert_eq!(v.op_sparse_map(&mut f, 0x10F_0000, 0x2_0000), Err(EINVAL));
        assert_eq!(v.op_map(&mut f, 0x10F_0000, 0x2_0000, b(1, 0)), Err(EINVAL));
        // A bind inside lives in the region.
        v.op_map(&mut f, 0x100_0000, 0x1_0000, b(3, 0)).unwrap();
        assert_eq!(f.live[0].4, Some(f.regions[0].0));
        // Partial unbind inside the region re-binds the rest in the region.
        v.op_unmap(&mut f, 0x100_8000, 0x8000).unwrap();
        consistent(&v, &f);
        assert!(f.live.iter().all(|e| e.4 == Some(f.regions[0].0)));
        // NVK's va_free: plain UNMAP of the range, then UNMAP|SPARSE.
        v.op_unmap(&mut f, 0x100_0000, 0x10_0000).unwrap();
        v.op_sparse_unmap(&mut f, 0x100_0000, 0x10_0000).unwrap();
        assert!(f.regions.is_empty() && f.live.is_empty());
        // Destroying a region that is not there.
        assert_eq!(v.op_sparse_unmap(&mut f, 0x100_0000, 0x10_0000), Err(ENOENT));
    }

    #[test]
    fn sparse_unmap_drops_leftovers_and_region_over_maps_refused() {
        let (mut v, mut f) = (vm(), Fake::default());
        v.op_map(&mut f, 0x200_0000, 0x1000, b(1, 0)).unwrap();
        assert_eq!(v.op_sparse_map(&mut f, 0x200_0000, 0x10000), Err(EINVAL));
        v.op_unmap(&mut f, 0x200_0000, 0x1000).unwrap();
        v.op_sparse_map(&mut f, 0x200_0000, 0x10000).unwrap();
        v.op_map(&mut f, 0x200_4000, 0x1000, b(1, 0)).unwrap();
        v.op_sparse_unmap(&mut f, 0x200_0000, 0x10000).unwrap();
        assert!(v.maps.is_empty() && f.live.is_empty() && f.regions.is_empty());
    }

    #[test]
    fn forget_bo_and_clear() {
        let (mut v, mut f) = (vm(), Fake::default());
        v.op_map(&mut f, 0x30_0000, 0x1000, b(1, 0)).unwrap();
        v.op_map(&mut f, 0x30_1000, 0x1000, b(2, 0)).unwrap();
        v.op_map(&mut f, 0x30_2000, 0x1000, b(1, 0x1000)).unwrap();
        v.forget_bo(&mut f, 1);
        consistent(&v, &f);
        assert_eq!(v.maps.len(), 1);
        v.op_sparse_map(&mut f, 0x400_0000, 0x10000).unwrap();
        v.clear(&mut f);
        assert!(v.maps.is_empty() && v.regions.is_empty() && f.live.is_empty() && f.regions.is_empty());
    }

    #[test]
    fn failed_remap_reports_error() {
        let (mut v, mut f) = (vm(), Fake::default());
        v.op_map(&mut f, 0x50_0000, 0x4000, b(1, 0)).unwrap();
        f.fail_map_at = Some(0x50_0000);
        assert_eq!(v.op_unmap(&mut f, 0x50_2000, 0x1000), Err(12));
        consistent(&v, &f);
    }

    #[test]
    fn pseudo_random_ops_stay_consistent() {
        let (mut v, mut f) = (vm(), Fake::default());
        let mut x: u64 = 0x1234_5678;
        let mut rnd = || { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x };
        for _ in 0..2000 {
            let start = 0x100_0000 + (rnd() % 64) * 0x1000;
            let len = (1 + rnd() % 8) * 0x1000;
            if rnd() % 3 == 0 {
                v.op_unmap(&mut f, start, len).unwrap();
            } else {
                v.op_map(&mut f, start, len, b((rnd() % 4) as u32, (rnd() % 4) * 0x1000)).unwrap();
            }
            consistent(&v, &f);
        }
        // Page-granular truth check: every page maps to bo/offset of the
        // mapping that covers it.
        for (s, m) in &v.maps {
            let mut a = *s;
            while a < m.end {
                assert_eq!(v.maps.range(..=a).next_back().unwrap().0, s);
                a += 0x1000;
            }
        }
    }
}

#[cfg(test)]
mod sync_file_tests {
    use super::sync_file_core::*;
    use super::syncobj_core::*;
    use std::cell::RefCell;
    use std::collections::BTreeSet;

    struct Done(RefCell<BTreeSet<u64>>);
    impl Done {
        fn new() -> Self { Done(RefCell::new(BTreeSet::new())) }
        fn signal(&self, f: u64) { self.0.borrow_mut().insert(f); }
        fn f(&self) -> impl Fn(u64) -> bool + '_ { move |x| self.0.borrow().contains(&x) }
    }

    #[test]
    fn flags() {
        assert!(!resv_flags_valid(0));
        assert!(resv_flags_valid(DMA_BUF_SYNC_READ));
        assert!(resv_flags_valid(DMA_BUF_SYNC_WRITE));
        assert!(resv_flags_valid(DMA_BUF_SYNC_RW));
        assert!(!resv_flags_valid(4));
        assert!(!resv_flags_valid(DMA_BUF_SYNC_READ | 8));
    }

    #[test]
    fn resv_reader_waits_for_writers_writer_for_all() {
        let d = Done::new();
        let mut r = Resv::new();
        assert_eq!(r.export(DMA_BUF_SYNC_READ, &d.f()), Fence::Signaled);
        assert_eq!(r.export(DMA_BUF_SYNC_RW, &d.f()), Fence::Signaled);
        r.add(&Fence::One(10), DMA_BUF_SYNC_WRITE, &d.f());
        r.add(&Fence::One(20), DMA_BUF_SYNC_READ, &d.f());
        r.add(&Fence::All(vec![21, 22]), DMA_BUF_SYNC_READ, &d.f());
        assert_eq!(r.export(DMA_BUF_SYNC_READ, &d.f()), Fence::One(10));
        assert_eq!(r.export(DMA_BUF_SYNC_WRITE, &d.f()), Fence::All(vec![10, 20, 21, 22]));
        assert_eq!(r.export(DMA_BUF_SYNC_RW, &d.f()), Fence::All(vec![10, 20, 21, 22]));
        d.signal(10);
        assert_eq!(r.export(DMA_BUF_SYNC_READ, &d.f()), Fence::Signaled);
        assert_eq!(r.export(DMA_BUF_SYNC_WRITE, &d.f()), Fence::All(vec![20, 21, 22]));
        d.signal(20); d.signal(21); d.signal(22);
        r.gc(&d.f());
        assert!(r.is_empty());
    }

    #[test]
    fn resv_import_skips_signalled_and_dedups() {
        let d = Done::new();
        d.signal(5);
        let mut r = Resv::new();
        r.add(&Fence::One(5), DMA_BUF_SYNC_WRITE, &d.f());
        r.add(&Fence::Signaled, DMA_BUF_SYNC_WRITE, &d.f());
        assert!(r.is_empty());
        r.add(&Fence::One(6), DMA_BUF_SYNC_RW, &d.f());
        r.add(&Fence::One(6), DMA_BUF_SYNC_WRITE, &d.f());
        assert_eq!(r.write, vec![6]);
        assert!(r.read.is_empty());
    }

    #[test]
    fn merge_flattens() {
        let d = Done::new();
        d.signal(1);
        assert_eq!(merge(&Fence::Signaled, &Fence::Signaled, &d.f()), Fence::Signaled);
        assert_eq!(merge(&Fence::One(1), &Fence::One(2), &d.f()), Fence::One(2));
        assert_eq!(merge(&Fence::One(3), &Fence::All(vec![2, 3, 4]), &d.f()), Fence::All(vec![2, 3, 4]));
    }

    #[test]
    fn syncobj_export_semantics() {
        let d = Done::new();
        let mut p = Payload::null();
        assert_eq!(syncobj_export(p.links(), p.base(), &d.f()), None);
        p.replace(Fence::Signaled);
        assert_eq!(syncobj_export(p.links(), p.base(), &d.f()), Some(Fence::Signaled));
        p.replace(Fence::One(9));
        assert_eq!(syncobj_export(p.links(), p.base(), &d.f()), Some(Fence::One(9)));
        d.signal(9);
        assert_eq!(syncobj_export(p.links(), p.base(), &d.f()), Some(Fence::Signaled));
        // timeline: every pending point
        let mut t = Payload::null();
        t.add_point(1, Fence::One(11));
        t.add_point(2, Fence::One(12));
        assert_eq!(syncobj_export(t.links(), t.base(), &d.f()), Some(Fence::All(vec![11, 12])));
        d.signal(11); d.signal(12);
        t.gc(&d.f());
        assert_eq!(syncobj_export(t.links(), t.base(), &d.f()), Some(Fence::Signaled));
    }
}
