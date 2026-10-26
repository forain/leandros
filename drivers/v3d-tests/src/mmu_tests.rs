use crate::mock::*;
use crate::v3d::mmu::*;
use crate::v3d::regs::V3dRegs;

const V: u32 = PTE_VALID | PTE_WRITEABLE;

fn table() -> Vec<u32> { vec![0u32; PT_ENTRIES] }

#[test]
fn table_is_4mib_order_10() {
    assert_eq!(PT_BYTES, 4 << 20);
    assert_eq!(4096usize << PT_ORDER, PT_BYTES);
}

#[test]
fn pte_encoding_4k_64k_1m() {
    assert_eq!(pte(0x1234_5000, PageKind::Small), Ok(0x12345 | V));
    assert_eq!(pte(0x10_0000, PageKind::Big), Ok(0x100 | V | PTE_BIGPAGE));
    assert_eq!(pte(0x20_0000, PageKind::Super), Ok(0x200 | V | PTE_SUPERPAGE));
    // Highest frame below 64 GiB, and the first one past it.
    assert_eq!(pte(0xF_FFFF_F000, PageKind::Small), Ok(0x00FF_FFFF | V));
    assert_eq!(pte(0x10_0000_0000, PageKind::Small), Err(MmuError::PaTooHigh));
    assert_eq!(pte(0x1001, PageKind::Small), Err(MmuError::Misaligned));
}

#[test]
fn small_pages_when_unaligned() {
    let mut pt = table();
    assert_eq!(fill_ptes(&mut pt, 0x3000, 0x8000_1000, 0x3000, false), Ok(3));
    assert_eq!(&pt[2..7], &[0, 0x80001 | V, 0x80002 | V, 0x80003 | V, 0]);
}

#[test]
fn big_pages_fill_all_16_ptes_with_pfn_plus_i() {
    let mut pt = table();
    // 64 KiB aligned VA and PA, 128 KiB long: two big pages = 32 PTEs.
    assert_eq!(fill_ptes(&mut pt, 0x10000, 0x4000_0000, 0x20000, false), Ok(32));
    for i in 0..32u32 {
        assert_eq!(pt[0x10 + i as usize], (0x40000 + i) | V | PTE_BIGPAGE, "pte {i}");
    }
    assert_eq!(pt[0x30], 0);
}

#[test]
fn big_page_needs_both_va_and_pa_aligned() {
    let mut pt = table();
    fill_ptes(&mut pt, 0x11000, 0x4000_0000, 0x10000, false).unwrap();
    assert!(pt[0x11..0x21].iter().all(|e| e & PTE_BIGPAGE == 0));
    let mut pt = table();
    fill_ptes(&mut pt, 0x10000, 0x4000_1000, 0x10000, false).unwrap();
    assert!(pt[0x10..0x20].iter().all(|e| e & PTE_BIGPAGE == 0));
}

#[test]
fn mixed_run_small_then_big_then_small() {
    let mut pt = table();
    // VA/PA 0xF000: one small page, then a 64 KiB big page at 0x10000, then one small page.
    fill_ptes(&mut pt, 0xF000, 0x10_F000, 0x12000, false).unwrap();
    assert_eq!(pt[0xF], 0x10F | V);
    assert!((0x10..0x20).all(|i| pt[i] & PTE_BIGPAGE != 0));
    assert_eq!(pt[0x20], 0x120 | V);
}

#[test]
fn super_pages_only_when_allowed() {
    let mut pt = table();
    fill_ptes(&mut pt, 0x10_0000, 0x8010_0000, 0x10_0000, true).unwrap();
    for i in 0..256u32 {
        assert_eq!(pt[0x100 + i as usize], (0x80100 + i) | V | PTE_SUPERPAGE);
    }
    let mut pt = table();
    fill_ptes(&mut pt, 0x10_0000, 0x8010_0000, 0x10_0000, false).unwrap();
    assert!(pt[0x100..0x200].iter().all(|e| e & PTE_SUPERPAGE == 0 && e & PTE_BIGPAGE != 0));
}

#[test]
fn rejects_before_writing_anything() {
    let mut pt = table();
    assert_eq!(fill_ptes(&mut pt, 0xFFFF_F000, 0x1000, 0x2000, false), Err(MmuError::OutOfRange));
    assert_eq!(fill_ptes(&mut pt, 0x1000, 0xF_FFFF_F000, 0x2000, false), Err(MmuError::PaTooHigh));
    assert_eq!(fill_ptes(&mut pt, 0x1800, 0x1000, 0x1000, false), Err(MmuError::Misaligned));
    assert!(pt.iter().all(|&e| e == 0));
}

#[test]
fn enable_sequence_matches_linux() {
    let r = MockRegs::new();
    enable(&r, 0x1_2340_0000, 0x5000, &FakeClock::new()).unwrap();
    let w = r.hub_writes();
    assert_eq!(w[0], (MMU_PT_PA_BASE, 0x123400));
    assert_eq!(w[1], (MMU_CTL, MMU_CTL_ON));
    assert_eq!(w[2], (MMU_ILLEGAL_ADDR, 0x5 | MMU_ILLEGAL_ADDR_ENABLE));
    assert_eq!(w[3], (MMUC_CONTROL, MMUC_CONTROL_ENABLE));
    assert_eq!(w[4], (MMUC_CONTROL, MMUC_CONTROL_FLUSH | MMUC_CONTROL_ENABLE));
    assert_eq!(w[5], (MMU_CTL, MMU_CTL_ON | MMU_CTL_TLB_CLEAR));
    assert_eq!(w.len(), 6);
    assert_eq!(MMU_CTL_ON & MMU_CTL_TLB_CLEAR, 0);
}

#[test]
fn flush_order_mmuc_then_tlb_and_waits_for_each() {
    let r = MockRegs::new();
    r.mmuc_busy_reads.set(5);
    r.tlb_busy_reads.set(7);
    flush_all(&r, &FakeClock::new()).unwrap();
    let w = r.hub_writes();
    assert_eq!(w, vec![(MMUC_CONTROL, MMUC_CONTROL_FLUSH | MMUC_CONTROL_ENABLE), (MMU_CTL, MMU_CTL_TLB_CLEAR)]);
    // Both busy bits observed clear at the end.
    assert_eq!(r.hub_r(MMUC_CONTROL) & MMUC_CONTROL_FLUSHING, 0);
    assert_eq!(r.hub_r(MMU_CTL) & MMU_CTL_TLB_CLEARING, 0);
}

#[test]
fn flush_timeouts_are_bounded_and_named() {
    let r = MockRegs::new();
    r.mmuc_busy_reads.set(u32::MAX);
    let c = FakeClock::new();
    assert_eq!(flush_all(&r, &c), Err(MmuError::FlushTimeout(FlushStage::Mmuc)));
    assert!(c.0.get() >= FLUSH_TIMEOUT_US && c.0.get() < FLUSH_TIMEOUT_US + 10);
    // The TLB clear is never issued after a failed MMUC flush.
    assert!(r.hub_writes().iter().all(|w| w.0 != MMU_CTL));
    let r = MockRegs::new();
    r.tlb_busy_reads.set(u32::MAX);
    assert_eq!(flush_all(&r, &FakeClock::new()), Err(MmuError::FlushTimeout(FlushStage::Tlb)));
}

#[test]
fn ptes_are_written_before_the_flush_starts() {
    let r = MockRegs::new();
    let mut pt = table();
    r.pt_watch.set(Some((pt.as_ptr(), 0x40)));
    let mut m = Mmu { pt: &mut pt, allow_super: false, pa_limit: 1 << 36 };
    m.map(&r, &FakeClock::new(), 0x40000, 0x9000_0000, 0x1000).unwrap();
    m.unmap(&r, &FakeClock::new(), 0x40000, 0x1000).unwrap();
    assert_eq!(*r.pte_at_flush.borrow(), vec![0x90000 | V, 0]);
}

#[test]
fn va_reuse_after_free_maps_the_new_bo_only() {
    let r = MockRegs::new();
    let c = FakeClock::new();
    let mut pt = table();
    let mut m = Mmu { pt: &mut pt, allow_super: false, pa_limit: 1 << 36 };
    // BO A: 64 KiB at VA 0x10000 (big pages), its neighbour at 0x20000.
    m.map(&r, &c, 0x10000, 0x4000_0000, 0x10000).unwrap();
    m.map(&r, &c, 0x20000, 0x5000_0000, 0x1000).unwrap();
    m.unmap(&r, &c, 0x10000, 0x10000).unwrap();
    assert!(m.pt[0x10..0x20].iter().all(|&e| e == 0));
    assert_eq!(m.pt[0x20], 0x50000 | V);
    // BO B reuses the start of the span with fewer, unaligned pages: no stale A entries remain.
    m.map(&r, &c, 0x10000, 0x6000_3000, 0x2000).unwrap();
    assert_eq!(&m.pt[0x10..0x12], &[0x60003 | V, 0x60004 | V]);
    assert!(m.pt[0x12..0x20].iter().all(|&e| e == 0));
    // Every map and unmap flushed exactly once.
    let flushes = r.hub_writes().iter().filter(|w| w.0 == MMUC_CONTROL && w.1 & MMUC_CONTROL_FLUSH != 0).count();
    assert_eq!(flushes, 4);
}

#[test]
fn failed_flush_on_map_leaves_no_ptes() {
    let r = MockRegs::new();
    let mut pt = table();
    let mut m = Mmu { pt: &mut pt, allow_super: false, pa_limit: 1 << 36 };
    r.tlb_busy_reads.set(u32::MAX);
    assert!(m.map(&r, &FakeClock::new(), 0x8000, 0x1000_0000, 0x3000).is_err());
    assert!(m.pt.iter().all(|&e| e == 0));
}

#[test]
fn pa_limit_from_mmu_debug_info() {
    let r = MockRegs::new();
    r.hub.borrow_mut().insert(MMU_DEBUG_INFO, 0x0000_0620);
    assert_eq!(pa_width(&r), 36);
    let mut pt = table();
    let mut m = Mmu { pt: &mut pt, allow_super: false, pa_limit: 1 << 34 };
    assert_eq!(m.map(&r, &FakeClock::new(), 0x1000, (1 << 34) - 0x1000, 0x2000), Err(MmuError::PaTooHigh));
    assert!(r.hub_writes().is_empty());
}
