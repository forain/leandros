use kvm::abi::*;
use kvm::ioevent::*;
use kvm::memslot::*;

const LIM: u64 = 1 << 40;
fn r(slot: u32, gpa: u64, size: u64, hva: u64, flags: u32) -> KvmUserspaceMemoryRegion {
    KvmUserspaceMemoryRegion { slot, flags, guest_phys_addr: gpa, memory_size: size, userspace_addr: hva }
}

#[test]
fn create_delete_and_errors() {
    let mut t = SlotTable::new();
    assert_eq!(t.set(&r(32, 0x8000_0000, 0x1000, 0x1000, 0), LIM), Err(22));
    assert_eq!(t.set(&r(1 << 16, 0x8000_0000, 0x1000, 0x1000, 0), LIM), Err(22)); // as_id 1
    assert_eq!(t.set(&r(1, 0x8000_0800, 0x1000, 0x1000, 0), LIM), Err(22));
    assert_eq!(t.set(&r(1, 0x8000_0000, 0x1000, 0x1800, 0), LIM), Err(22));
    assert_eq!(t.set(&r(1, 0x8000_0000, 0x1800, 0x1000, 0), LIM), Err(22));
    assert_eq!(t.set(&r(1, 0x8000_0000, 0x1000, 0x1000, KVM_MEM_LOG_DIRTY_PAGES), LIM), Err(22));
    assert_eq!(t.set(&r(1, 0x8000_0000, 0x1000, 0x1000, 8), LIM), Err(22));
    assert_eq!(t.set(&r(1, LIM, 0x1000, 0x1000, 0), LIM), Err(22)); // beyond IPA space
    assert_eq!(t.set(&r(1, u64::MAX & !0xfff, 0x2000, 0x1000, 0), LIM), Err(22));
    assert_eq!(t.set(&r(1, 0, 0, 0, 0), LIM), Err(22)); // delete of an empty slot
    assert_eq!(t.set(&r(1, 0x8000_0000, 0x1000, 0x1000, 0), LIM), Ok(Change::Created));
    assert_eq!(t.count(), 1);
    let old = t.get(1).unwrap();
    assert_eq!(t.set(&r(1, 0, 0, 0, 0), LIM), Ok(Change::Deleted(old)));
    assert_eq!(t.count(), 0);
}

#[test]
fn overlap_and_modify() {
    let mut t = SlotTable::new();
    t.set(&r(0, 0x4000_0000, 0x10000, 0x10_0000, 0), LIM).unwrap();
    assert_eq!(t.set(&r(1, 0x4000_8000, 0x10000, 0x20_0000, 0), LIM), Err(17));
    assert_eq!(t.set(&r(1, 0x4001_0000, 0x1000, 0x20_0000, 0), LIM), Ok(Change::Created)); // adjacent
    // same slot: size or hva change is EINVAL, moving the gpa is a move, identical is a no-op
    assert_eq!(t.set(&r(0, 0x4000_0000, 0x8000, 0x10_0000, 0), LIM), Err(22));
    assert_eq!(t.set(&r(0, 0x4000_0000, 0x10000, 0x30_0000, 0), LIM), Err(22));
    assert_eq!(t.set(&r(0, 0x4000_0000, 0x10000, 0x10_0000, KVM_MEM_READONLY), LIM), Err(22));
    assert_eq!(t.set(&r(0, 0x4000_0000, 0x10000, 0x10_0000, 0), LIM), Ok(Change::Nothing));
    let old = t.get(0).unwrap();
    assert_eq!(t.set(&r(0, 0x5000_0000, 0x10000, 0x10_0000, 0), LIM), Ok(Change::Moved(old)));
    assert!(t.find_gpa(0x4000_0000).is_none());
    assert_eq!(t.gpa_to_hva(0x5000_2345).map(|x| x.0), Some(0x10_2345));
}

#[test]
fn hva_ranges() {
    let mut t = SlotTable::new();
    t.set(&r(0, 0x8000_0000, 0x4000, 0x10_0000, 0), LIM).unwrap();
    t.set(&r(1, 0x9000_0000, 0x2000, 0x10_3000, KVM_MEM_READONLY), LIM).unwrap(); // hva overlaps slot 0's last page
    let mut out = [(0, 0); 32];
    let n = t.ranges_for_hva(0x10_2000, 0x2000, &mut out);
    assert_eq!(n, 2);
    assert_eq!(out[0], (0x8000_2000, 0x2000));
    assert_eq!(out[1], (0x9000_0000, 0x1000));
    assert_eq!(t.ranges_for_hva(0x20_0000, 0x1000, &mut out), 0);
    assert!(t.get(1).unwrap().readonly());
}

#[test]
fn ioevent_rules() {
    let mut a = KvmIoeventfd::default();
    a.addr = 0xa00_0050; a.len = 4; a.fd = 5;
    let e = IoEvent::from_abi(&a, 7).unwrap();
    assert!(e.matches(0xa00_0050, 4, 123));
    assert!(!e.matches(0xa00_0050, 2, 123));
    assert!(!e.matches(0xa00_0054, 4, 123));
    a.len = 0;
    let any = IoEvent::from_abi(&a, 7).unwrap();
    assert!(any.matches(0xa00_0050, 2, 0) && any.matches(0xa00_0050, 8, 0));
    a.len = 4; a.flags = KVM_IOEVENTFD_FLAG_DATAMATCH; a.datamatch = 9;
    let dm = IoEvent::from_abi(&a, 7).unwrap();
    assert!(dm.matches(0xa00_0050, 4, 9) && !dm.matches(0xa00_0050, 4, 8));
    a.flags = KVM_IOEVENTFD_FLAG_PIO;
    assert_eq!(IoEvent::from_abi(&a, 7), Err(22));
    a.flags = 0; a.len = 3;
    assert_eq!(IoEvent::from_abi(&a, 7), Err(22));
}
