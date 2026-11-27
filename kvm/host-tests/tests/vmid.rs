use kvm::vmid::*;

#[test]
fn first_allocs_are_unique_nonzero_no_flush() {
    let mut a = VmidAlloc::new(8);
    let mut seen = std::collections::HashSet::new();
    for _ in 0..255 {
        let mut s = VmidSlot::default();
        let (id, flush) = a.get(&mut s).unwrap();
        assert!(id != 0 && !flush && seen.insert(id));
    }
}

#[test]
fn same_gen_slot_is_stable() {
    let mut a = VmidAlloc::new(8);
    let mut s = VmidSlot::default();
    let (id, _) = a.get(&mut s).unwrap();
    assert_eq!(a.get(&mut s), Some((id, false)));
    assert_eq!(a.in_use_count(), 2);
}

#[test]
fn rollover_flushes_and_bumps_generation() {
    let mut a = VmidAlloc::new(8);
    let mut slots = vec![VmidSlot::default(); 255];
    for s in &mut slots { a.get(s).unwrap(); }
    let g0 = a.generation();
    let mut extra = VmidSlot::default();
    let (id, flush) = a.get(&mut extra).unwrap();
    assert!(flush && a.generation() == g0 + 1 && id != 0);
    // every old slot is stale now and gets a fresh vmid, without a second flush
    let mut s0 = slots[0];
    let (id2, f2) = a.get(&mut s0).unwrap();
    assert!(!f2 && id2 != id && s0.gen == a.generation());
}

#[test]
fn running_vmid_survives_rollover_and_returns_to_owner() {
    let mut a = VmidAlloc::new(8);
    let mut slots = vec![VmidSlot::default(); 255];
    for s in &mut slots { a.get(s).unwrap(); }
    let owner = slots[10];
    a.enter(owner.vmid);
    let mut extra = VmidSlot::default();
    assert!(a.get(&mut extra).unwrap().1);
    assert_ne!(extra.vmid, owner.vmid, "running vmid not reissued");
    // fill the rest of the new generation: never hands out the carried vmid
    for _ in 0..253 {
        let mut s = VmidSlot::default();
        let (id, _) = a.get(&mut s).unwrap();
        assert_ne!(id, owner.vmid);
    }
    let mut o = owner;
    assert_eq!(a.get(&mut o), Some((owner.vmid, false)));
    assert_eq!(o.gen, a.generation());
    a.exit(owner.vmid);
}

#[test]
fn release_makes_vmid_reusable_and_resets_slot() {
    let mut a = VmidAlloc::new(8);
    let mut s = VmidSlot::default();
    a.get(&mut s).unwrap();
    let before = a.in_use_count();
    a.release(&mut s);
    assert_eq!(s, VmidSlot::default());
    assert_eq!(a.in_use_count(), before - 1);
}

#[test]
fn sixteen_bit_space_and_vmid0_reserved() {
    let mut a = VmidAlloc::new(16);
    let mut ids = std::collections::HashSet::new();
    for _ in 0..1000 {
        let mut s = VmidSlot::default();
        let (id, f) = a.get(&mut s).unwrap();
        assert!(id != 0 && !f && ids.insert(id));
    }
    assert!(ids.iter().any(|&i| i > 255));
}

#[test]
fn many_rollovers_never_duplicate_running() {
    let mut a = VmidAlloc::new(8);
    let mut live = VmidSlot::default();
    a.get(&mut live).unwrap();
    a.enter(live.vmid);
    for _ in 0..2000 {
        let mut s = VmidSlot::default();
        let (id, _) = a.get(&mut s).unwrap();
        assert_ne!(id, live.vmid);
    }
    assert_eq!(a.get(&mut live).map(|x| x.0).is_some(), true);
}
