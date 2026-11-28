use kvm::abi::*;
use kvm::irqfd::*;

fn a(flags: u32) -> KvmIrqfd { KvmIrqfd { fd: 3, gsi: 5, flags, resamplefd: 0, pad: [0; 16] } }

#[test]
fn flags_validated() {
    assert!(check(&a(0)).is_ok());
    assert!(check(&a(KVM_IRQFD_FLAG_RESAMPLE)).is_ok());
    assert_eq!(check(&a(0x80)), Err(EINVAL));
    assert!(deassign(&a(KVM_IRQFD_FLAG_DEASSIGN)));
    assert!(wants_resample(&a(KVM_IRQFD_FLAG_RESAMPLE)));
}

#[test]
fn assign_lookup_deassign() {
    let mut t = IrqfdTable::new();
    t.assign(Irqfd { slot: 7, gsi: 5, resample: None }).unwrap();
    assert_eq!(t.assign(Irqfd { slot: 7, gsi: 5, resample: None }), Err(EBUSY));
    t.assign(Irqfd { slot: 7, gsi: 6, resample: None }).unwrap();
    assert_eq!(t.gsis_for(7), vec![5, 6]);
    assert!(t.gsis_for(8).is_empty());
    assert!(t.has_slot(7));
    assert_eq!(t.deassign(7, 5).unwrap().gsi, 5);
    assert!(t.deassign(7, 5).is_none());
    assert_eq!(t.gsis_for(7), vec![6]);
    assert_eq!(t.take_all().len(), 1);
    assert!(t.is_empty());
}

#[test]
fn resample_maps_gsi_to_spi() {
    let mut t = IrqfdTable::new();
    t.assign(Irqfd { slot: 1, gsi: 0, resample: Some(9) }).unwrap();
    t.assign(Irqfd { slot: 2, gsi: 3, resample: None }).unwrap();
    assert_eq!(t.resample_for(32), vec![(0, 9)]);
    assert!(t.resample_for(35).is_empty());
    assert!(t.resample_for(33).is_empty());
}
