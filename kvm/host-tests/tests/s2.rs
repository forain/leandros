use kvm::s2::*;
use std::cell::RefCell;
use std::rc::Rc;

const BASE: u64 = 0x8000_0000;
const FRAMES: usize = 4096;

#[derive(Default)]
struct Pool { mem: Vec<u64>, used: Vec<bool> }

#[derive(Clone)]
struct VecAlloc(Rc<RefCell<Pool>>);

impl VecAlloc {
    fn new() -> Self {
        let p = Pool { mem: vec![0u64; FRAMES * 512], used: vec![false; FRAMES] };
        VecAlloc(Rc::new(RefCell::new(p)))
    }
    fn live(&self) -> usize { self.0.borrow().used.iter().filter(|u| **u).count() }
}

impl FrameAlloc for VecAlloc {
    fn alloc_zeroed(&mut self) -> Option<(u64, *mut u64)> { self.alloc_zeroed_pages(1) }
    fn free(&mut self, pa: u64) { self.free_pages(pa, 1) }
    fn ptr(&self, pa: u64) -> *mut u64 {
        let idx = ((pa - BASE) / 4096) as usize;
        let mut p = self.0.borrow_mut();
        unsafe { p.mem.as_mut_ptr().add(idx * 512) }
    }
    fn alloc_zeroed_pages(&mut self, n: usize) -> Option<(u64, *mut u64)> {
        let mut p = self.0.borrow_mut();
        let mut i = 0;
        while i + n <= FRAMES {
            if p.used[i..i + n].iter().all(|u| !*u) {
                for u in &mut p.used[i..i + n] { *u = true; }
                for w in &mut p.mem[i * 512..(i + n) * 512] { *w = 0; }
                let ptr = unsafe { p.mem.as_mut_ptr().add(i * 512) };
                return Some((BASE + i as u64 * 4096, ptr));
            }
            i += n; // keeps n-alignment
        }
        None
    }
    fn free_pages(&mut self, pa: u64, n: usize) {
        let idx = ((pa - BASE) / 4096) as usize;
        let mut p = self.0.borrow_mut();
        for u in &mut p.used[idx..idx + n] { assert!(*u, "double free"); *u = false; }
    }
}

fn mk(bits: u32) -> (Stage2<VecAlloc>, VecAlloc) {
    let a = VecAlloc::new();
    let s = Stage2::new(a.clone(), bits).ok().unwrap();
    (s, a)
}

#[test]
fn geometry_roots() {
    let g = |b| Geometry::new(b).unwrap();
    assert_eq!((g(32).start_level, g(32).root_pages, g(32).sl0()), (2, 4, 0));
    assert_eq!((g(34).start_level, g(34).root_pages), (2, 16));
    assert_eq!((g(35).start_level, g(35).root_pages, g(35).sl0()), (1, 1, 1));
    assert_eq!((g(39).start_level, g(39).root_pages), (1, 1));
    assert_eq!((g(40).start_level, g(40).root_pages), (1, 2));
    assert_eq!((g(43).start_level, g(43).root_pages), (1, 16));
    assert_eq!((g(44).start_level, g(44).root_pages, g(44).sl0()), (0, 1, 2));
    assert_eq!((g(48).start_level, g(48).root_pages), (0, 1));
    assert_eq!(Geometry::new(31), Err(S2Err::BadIpaBits));
    assert_eq!(Geometry::new(49), Err(S2Err::BadIpaBits));
}

#[test]
fn vtcr_40bit() {
    let (s, _) = mk(40);
    let v = s.vtcr(2); // PARange 40 bits
    assert_eq!(v & 0x3f, 24); // T0SZ
    assert_eq!((v >> 6) & 3, 1); // SL0 = level 1
    assert_eq!((v >> 8) & 3, 1);
    assert_eq!((v >> 10) & 3, 1);
    assert_eq!((v >> 12) & 3, 3);
    assert_eq!((v >> 14) & 3, 0);
    assert_eq!((v >> 16) & 7, 2);
    assert_eq!((v >> 19) & 1, 0);
    assert_eq!(v >> 31 & 1, 1);
    assert_eq!(s.vtcr_with(2, true) >> 19 & 1, 1);
}

#[test]
fn root_alignment_and_vttbr() {
    let (s, _) = mk(40);
    assert_eq!(s.vttbr_baddr() % 8192, 0);
    assert_eq!(s.vttbr(0x1234) >> 48, 0x1234);
    assert_eq!(s.vttbr(7) & 0xFFFF_FFFF_FFFF, s.vttbr_baddr());
}

#[test]
fn descriptor_bits_normal() {
    let (mut s, _) = mk(40);
    s.map(0x4000_0000, 0x9000_1000, Perm::RW, Attr::Normal).unwrap();
    let d = s.walk_desc(0x4000_0000).unwrap();
    assert_eq!(d & 3, 3);
    assert_eq!((d >> 2) & 0xF, 0b1111);
    assert_eq!((d >> 6) & 3, 3);
    assert_eq!((d >> 8) & 3, 3);
    assert_eq!((d >> 10) & 1, 1);
    assert_eq!(d & DESC_XN, DESC_XN);
    assert_eq!(d & 0x0000_FFFF_FFFF_F000, 0x9000_1000);
    assert_eq!(s.walk(0x4000_0123), Some((0x9000_1123, Perm::RW)));
}

#[test]
fn descriptor_bits_device_ro_exec() {
    let (mut s, _) = mk(40);
    s.map(0x0900_0000, 0x0900_0000, Perm::RW, Attr::Device).unwrap();
    let d = s.walk_desc(0x0900_0000).unwrap();
    assert_eq!((d >> 2) & 0xF, 0b0001);
    assert_eq!((d >> 8) & 3, 0);
    s.map(0x5000_0000, 0xA000_0000, Perm::RO, Attr::Normal).unwrap();
    assert_eq!(s.walk(0x5000_0000).unwrap().1, Perm::RO);
    s.map(0x5000_1000, 0xA000_1000, Perm::RX, Attr::Normal).unwrap();
    let d = s.walk_desc(0x5000_1000).unwrap();
    assert_eq!(d & DESC_XN, 0);
    assert_eq!((d >> 6) & 3, 1);
    s.map(0x5000_2000, 0xA000_2000, Perm::RWX, Attr::Normal).unwrap();
    assert_eq!(s.walk(0x5000_2000).unwrap().1, Perm::RWX);
}

#[test]
fn map_errors() {
    let (mut s, _) = mk(40);
    let w = Perm { read: false, write: true, exec: false };
    assert_eq!(s.map(0x1001, 0x2000, Perm::RW, Attr::Normal), Err(S2Err::Unaligned));
    assert_eq!(s.map(0x1000, 0x2001, Perm::RW, Attr::Normal), Err(S2Err::Unaligned));
    assert_eq!(s.map(1 << 40, 0x2000, Perm::RW, Attr::Normal), Err(S2Err::IpaRange));
    assert_eq!(s.map(0x1000, 1 << 48, Perm::RW, Attr::Normal), Err(S2Err::PaRange));
    assert_eq!(s.map(0x1000, 0x2000, w, Attr::Normal), Err(S2Err::BadPerm));
    assert_eq!(s.map(0x1000, 0x2000, Perm::RWX, Attr::Device), Err(S2Err::BadPerm));
    s.map(0x1000, 0x2000, Perm::RW, Attr::Normal).unwrap();
    assert_eq!(s.map(0x1000, 0x3000, Perm::RW, Attr::Normal), Err(S2Err::Exists));
    // same pa: permission update
    s.map(0x1000, 0x2000, Perm::RO, Attr::Normal).unwrap();
    assert_eq!(s.walk(0x1000).unwrap().1, Perm::RO);
    assert_eq!(s.leaf_count(), 1);
}

#[test]
fn roots_32_40_44_48_bit() {
    for (bits, top) in [(32u32, (1u64 << 32) - 4096), (40, (1 << 40) - 4096),
                        (44, (1 << 44) - 4096), (48, (1 << 48) - 4096)] {
        let (mut s, _) = mk(bits);
        s.map(0, 0x8000_0000, Perm::RW, Attr::Normal).unwrap();
        s.map(top, 0x8000_1000, Perm::RW, Attr::Normal).unwrap();
        s.map(top / 2 & !0xFFF, 0x8000_2000, Perm::RX, Attr::Normal).unwrap();
        assert_eq!(s.walk(0).unwrap().0, 0x8000_0000, "{bits}");
        assert_eq!(s.walk(top).unwrap().0, 0x8000_1000, "{bits}");
        assert_eq!(s.walk(top / 2 & !0xFFF).unwrap().1, Perm::RX, "{bits}");
        assert_eq!(s.walk(0x1000), None);
        assert_eq!(s.map(1u64 << bits, 0x8000_0000, Perm::RW, Attr::Normal), Err(S2Err::IpaRange));
        assert_eq!(s.walk(1u64 << bits), None);
    }
}

#[test]
fn concatenated_root_second_half() {
    // 40-bit: IPAs >= 2^39 land in the second concatenated root page.
    let (mut s, _) = mk(40);
    s.map(1 << 39, 0x8000_0000, Perm::RW, Attr::Normal).unwrap();
    assert_eq!(s.walk(1 << 39).unwrap().0, 0x8000_0000);
    assert_eq!(s.walk(0), None);
}

#[test]
fn unmap_reports_and_frees_tables() {
    let (mut s, a) = mk(40);
    let base_live = a.live(); // root pages
    assert_eq!(base_live, 2);
    for i in 0..8u64 { s.map(0x4000_0000 + i * 4096, 0x9000_0000 + i * 4096, Perm::RW, Attr::Normal).unwrap(); }
    assert_eq!(a.live(), base_live + 2); // L2 + L3
    let mut got = vec![];
    let n = s.unmap(0x4000_0000, 8 * 4096, |pa| got.push(pa));
    assert_eq!(n, 8);
    got.sort();
    assert_eq!(got, (0..8).map(|i| 0x9000_0000 + i * 4096).collect::<Vec<_>>());
    assert_eq!(a.live(), base_live, "empty tables freed");
    assert_eq!(s.leaf_count(), 0);
    assert_eq!(s.walk(0x4000_0000), None);
}

#[test]
fn unmap_partial_keeps_neighbours() {
    let (mut s, a) = mk(40);
    for i in 0..4u64 { s.map(0x4000_0000 + i * 4096, 0x9000_0000 + i * 4096, Perm::RW, Attr::Normal).unwrap(); }
    let live = a.live();
    let mut got = vec![];
    assert_eq!(s.unmap(0x4000_1000, 2 * 4096, |pa| got.push(pa)), 2);
    assert_eq!(got.len(), 2);
    assert!(s.walk(0x4000_0000).is_some() && s.walk(0x4000_3000).is_some());
    assert!(s.walk(0x4000_1000).is_none() && s.walk(0x4000_2000).is_none());
    assert_eq!(a.live(), live, "tables still in use");
    // unmapping holes is a no-op
    assert_eq!(s.unmap(0x4000_1000, 2 * 4096, |_| panic!()), 0);
    assert_eq!(s.leaf_count(), 2);
}

#[test]
fn unmap_huge_sparse_range_is_fast_and_clamped() {
    let (mut s, _) = mk(48);
    s.map(0x10_0000_0000, 0x8000_0000, Perm::RW, Attr::Normal).unwrap();
    s.map((1 << 48) - 4096, 0x8000_1000, Perm::RW, Attr::Normal).unwrap();
    let mut n = 0;
    assert_eq!(s.unmap(0, u64::MAX, |_| n += 1), 2);
    assert_eq!(n, 2);
    assert_eq!(s.leaf_count(), 0);
}

#[test]
fn unmap_zero_len_and_unaligned() {
    let (mut s, _) = mk(40);
    s.map(0x1000, 0x8000_0000, Perm::RW, Attr::Normal).unwrap();
    assert_eq!(s.unmap(0x1000, 0, |_| panic!()), 0);
    assert_eq!(s.unmap(0x1001, 4096, |_| panic!()), 0);
    // unaligned length rounds up
    assert_eq!(s.unmap(0x1000, 1, |_| ()), 1);
}

#[test]
fn protect_grants_exec_lazily() {
    let (mut s, _) = mk(40);
    s.map(0x4000_0000, 0x9000_0000, Perm::RW, Attr::Normal).unwrap();
    assert!(s.protect(0x4000_0000, Perm::RX));
    assert_eq!(s.walk(0x4000_0000).unwrap().1, Perm::RX);
    assert!(!s.protect(0x4000_1000, Perm::RX), "unmapped");
    s.map(0x0900_0000, 0x0900_0000, Perm::RW, Attr::Device).unwrap();
    assert!(!s.protect(0x0900_0000, Perm::RX), "no exec on device");
    assert!(!s.protect(0x4000_0000, Perm { read: false, write: true, exec: false }));
}

#[test]
fn drop_frees_everything() {
    let a = VecAlloc::new();
    {
        let mut s = Stage2::new(a.clone(), 44).ok().unwrap();
        for i in 0..1000u64 { s.map(i * 0x20_0000, 0x8000_0000, Perm::RW, Attr::Normal).unwrap(); }
        assert!(a.live() > 1000);
    }
    assert_eq!(a.live(), 0);
}

#[test]
fn unmap_all_unpins() {
    let (mut s, a) = mk(40);
    for i in 0..600u64 { s.map(i * 4096, 0x8000_0000 + i * 4096, Perm::RW, Attr::Normal).unwrap(); }
    let mut n = 0;
    assert_eq!(s.unmap_all(|_| n += 1), 600);
    assert_eq!(n, 600);
    assert_eq!(a.live(), 2);
}

#[test]
fn nomem_is_reported_and_leaves_state_usable() {
    let a = VecAlloc::new();
    let mut s = Stage2::new(a.clone(), 40).ok().unwrap();
    // exhaust
    let mut hog = a.clone();
    while hog.alloc_zeroed().is_some() {}
    assert_eq!(s.map(0x4000_0000, 0x9000_0000, Perm::RW, Attr::Normal), Err(S2Err::NoMem));
    assert_eq!(s.walk(0x4000_0000), None);
}
