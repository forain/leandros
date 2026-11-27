use kvm::ioctl::*;
use kvm::*;

#[test]
fn ioctl_numbers_match_linux() {
    assert_eq!(KVM_GET_API_VERSION, 0xAE00);
    assert_eq!(KVM_CREATE_VM, 0xAE01);
    assert_eq!(KVM_CHECK_EXTENSION, 0xAE03);
    assert_eq!(KVM_GET_VCPU_MMAP_SIZE, 0xAE04);
    assert_eq!(KVM_CREATE_VCPU, 0xAE41);
    assert_eq!(KVM_SET_USER_MEMORY_REGION, 0x4020AE46);
    assert_eq!(KVM_ARM_PREFERRED_TARGET, 0x8020AEAF);
    assert_eq!(KVM_IRQ_LINE, 0x4008AE61);
    assert_eq!(KVM_CREATE_DEVICE, 0xC00CAEE0);
    assert_eq!(KVM_IOEVENTFD, 0x4040AE79);
    assert_eq!(KVM_IRQFD, 0x4020AE76);
    assert_eq!(KVM_ENABLE_CAP, 0x4068AEA3);
    assert_eq!(KVM_SET_DEVICE_ATTR, 0x4018AEE1);
    assert_eq!(KVM_GET_DEVICE_ATTR, 0x4018AEE2);
    assert_eq!(KVM_HAS_DEVICE_ATTR, 0x4018AEE3);
    assert_eq!(KVM_ARM_VCPU_INIT, 0x4020AEAE);
    assert_eq!(KVM_GET_ONE_REG, 0x4010AEAB);
    assert_eq!(KVM_SET_ONE_REG, 0x4010AEAC);
    assert_eq!(KVM_GET_REG_LIST, 0xC008AEB0);
    assert_eq!(KVM_RUN, 0xAE80);
    assert_eq!(KVM_SET_SIGNAL_MASK, 0x4004AE8B);
    assert_eq!(KVM_GET_MP_STATE, 0x8004AE98);
    assert_eq!(KVM_SET_MP_STATE, 0x4004AE99);
}

#[test]
fn ioctl_field_decode_round_trip() {
    for &(dir_, nr_, sz) in &[(0usize, 0x80usize, 0usize), (1, 0x46, 32), (2, 0xaf, 32), (3, 0xe0, 12), (3, 0xb0, 8)] {
        let r = ioc(dir_, KVMIO, nr_, sz);
        assert_eq!((dir(r), ty(r), nr(r), size(r)), (dir_, KVMIO, nr_, sz));
    }
}

#[test]
fn caps() {
    use cap::*;
    assert_eq!(KVM_CAP_USER_MEMORY, 3);
    assert_eq!(KVM_CAP_DESTROY_MEMORY_REGION_WORKS, 21);
    assert_eq!(KVM_CAP_JOIN_MEMORY_REGIONS_WORKS, 30);
    assert_eq!(KVM_CAP_IOEVENTFD, 36);
    assert_eq!(KVM_CAP_INTERNAL_ERROR_DATA, 40);
    assert_eq!(KVM_CAP_ONE_REG, 70);
    assert_eq!(KVM_CAP_ARM_PSCI, 87);
    assert_eq!(KVM_CAP_DEVICE_CTRL, 89);
    assert_eq!(KVM_CAP_ARM_PSCI_0_2, 102);
    assert_eq!(KVM_CAP_IOEVENTFD_ANY_LENGTH, 122);
    assert_eq!(KVM_CAP_IMMEDIATE_EXIT, 136);
    assert_eq!(KVM_CAP_ARM_USER_IRQ, 144);
    assert_eq!(KVM_CAP_ARM_VM_IPA_SIZE, 165);
    assert_eq!(answer(KVM_CAP_USER_MEMORY, 40, false), 1);
    assert_eq!(answer(KVM_CAP_IOEVENTFD_ANY_LENGTH, 40, false), 1);
    assert_eq!(answer(KVM_CAP_ARM_VM_IPA_SIZE, 40, false), 40);
    assert_eq!(answer(KVM_CAP_NR_VCPUS, 40, false), 8);
    assert_eq!(answer(KVM_CAP_MAX_VCPUS, 40, false), 8);
    assert_eq!(answer(KVM_CAP_NR_MEMSLOTS, 40, false), 32);
    assert_eq!(answer(KVM_CAP_IRQFD, 40, false), 0);
    assert_eq!(answer(KVM_CAP_IRQFD, 40, true), 1);
    for c in [KVM_CAP_SYNC_MMU, KVM_CAP_IRQ_ROUTING, KVM_CAP_ARM_SVE, KVM_CAP_ARM_PMU_V3, KVM_CAP_ARM_MTE,
              KVM_CAP_VCPU_EVENTS, KVM_CAP_DIRTY_LOG_RING, KVM_CAP_COALESCED_MMIO, KVM_CAP_GUEST_MEMFD,
              KVM_CAP_USER_MEMORY2, KVM_CAP_ARM_NISV_TO_USER, KVM_CAP_STEAL_TIME, KVM_CAP_ARM_EL1_32BIT, 9999] {
        assert_eq!(answer(c, 40, true), 0, "cap {c}");
    }
}

#[test]
fn exits() {
    use exit::*;
    assert_eq!(KVM_EXIT_MMIO, 6);
    assert_eq!(KVM_EXIT_FAIL_ENTRY, 9);
    assert_eq!(KVM_EXIT_INTR, 10);
    assert_eq!(KVM_EXIT_INTERNAL_ERROR, 17);
    assert_eq!(KVM_EXIT_SYSTEM_EVENT, 24);
    assert_eq!((KVM_SYSTEM_EVENT_SHUTDOWN, KVM_SYSTEM_EVENT_RESET, KVM_SYSTEM_EVENT_CRASH), (1, 2, 3));
}

#[test]
fn struct_layouts() {
    use core::mem::{offset_of, size_of};
    assert_eq!(size_of::<KvmRun>(), 2352);
    assert_eq!(offset_of!(KvmRun, exit), 32);
    assert_eq!(offset_of!(KvmRun, s), 304);
    let mut r = KvmRun::zeroed();
    r.exit.mmio = KvmRunMmio { phys_addr: 0x0900_0000, data: [1, 2, 3, 4, 5, 6, 7, 8], len: 4, is_write: 1 };
    let base = &r as *const KvmRun as *const u8;
    unsafe {
        assert_eq!(*(base.add(32) as *const u64), 0x0900_0000);
        assert_eq!(*base.add(32 + 8), 1);
        assert_eq!(*(base.add(32 + 16) as *const u32), 4);
        assert_eq!(*base.add(32 + 20), 1);
    }
    let mut r = KvmRun::zeroed();
    r.exit.system_event = KvmRunSystemEvent { type_: 2, ndata: 0, data: [0; 16] };
    let base = &r as *const KvmRun as *const u8;
    assert_eq!(unsafe { *(base.add(32) as *const u32) }, 2);
    assert_eq!(core::mem::size_of::<KvmIoeventfd>(), 64);
    assert_eq!(core::mem::size_of::<KvmDeviceAttr>(), 24);
    assert_eq!(KvmIoeventfd::default().fd, -1);
}

#[test]
fn irq_line_encoding() {
    assert_eq!(irq_line(KVM_ARM_IRQ_TYPE_SPI, 0, 33), 0x0100_0021);
    assert_eq!(irq_line(KVM_ARM_IRQ_TYPE_PPI, 3, 27), 0x0203_001b);
    assert_eq!(irq_line(KVM_ARM_IRQ_TYPE_CPU, 1, KVM_ARM_IRQ_CPU_FIQ), 0x0001_0001);
    for &(t, v, n) in &[(0, 0, 0), (1, 0, 987), (2, 7, 31), (0, 255, 0xffff)] {
        assert_eq!(irq_line_decode(irq_line(t, v, n)), (t, v, n));
    }
}

#[test]
fn vcpu_init_features() {
    let mut f = [0u32; 7];
    f[0] = 1 << KVM_ARM_VCPU_POWER_OFF | 1 << KVM_ARM_VCPU_PSCI_0_2;
    assert!(vcpu_feature_set(&f, KVM_ARM_VCPU_POWER_OFF));
    assert!(!vcpu_feature_set(&f, KVM_ARM_VCPU_EL1_32BIT));
    assert!(vcpu_feature_set(&f, KVM_ARM_VCPU_PSCI_0_2));
    assert_eq!(KVM_ARM_TARGET_GENERIC_V8, 5);
}

#[test]
fn vgic_and_device_consts() {
    assert_eq!((KVM_DEV_TYPE_ARM_VGIC_V2, KVM_DEV_TYPE_ARM_VGIC_V3, KVM_DEV_TYPE_ARM_VGIC_ITS), (5, 7, 8));
    assert_eq!(KVM_CREATE_DEVICE_TEST, 1);
    use vgic::*;
    assert_eq!((GRP_ADDR, GRP_DIST_REGS, GRP_CPU_REGS, GRP_NR_IRQS, GRP_CTRL), (0, 1, 2, 3, 4));
    assert_eq!((GRP_REDIST_REGS, GRP_CPU_SYSREGS, GRP_LEVEL_INFO), (5, 6, 7));
    assert_eq!((V2_ADDR_TYPE_DIST, V2_ADDR_TYPE_CPU), (0, 1));
    assert_eq!((V3_ADDR_TYPE_DIST, V3_ADDR_TYPE_REDIST, V3_ADDR_TYPE_REDIST_REGION), (2, 3, 5));
    assert_eq!(CTRL_INIT, 0);
    assert_eq!((KVM_IOEVENTFD_FLAG_DATAMATCH, KVM_IOEVENTFD_FLAG_PIO, KVM_IOEVENTFD_FLAG_DEASSIGN), (1, 2, 4));
    assert_eq!((KVM_IRQFD_FLAG_DEASSIGN, KVM_IRQFD_FLAG_RESAMPLE), (1, 2));
}

#[test]
fn core_reg_ids() {
    use coreregs::*;
    assert_eq!(x(0), 0x6030_0000_0010_0000);
    assert_eq!(x(1), 0x6030_0000_0010_0002);
    assert_eq!(x(30), 0x6030_0000_0010_003c);
    assert_eq!(SP, 0x6030_0000_0010_003e);
    assert_eq!(PC, 0x6030_0000_0010_0040);
    assert_eq!(PSTATE, 0x6030_0000_0010_0042);
    assert_eq!(SP_EL1, 0x6030_0000_0010_0044);
    assert_eq!(ELR_EL1, 0x6030_0000_0010_0046);
    assert_eq!(spsr(0), 0x6030_0000_0010_0048);
    assert_eq!(spsr(4), 0x6030_0000_0010_0050);
    assert_eq!(vreg(0), 0x6040_0000_0010_0054);
    assert_eq!(vreg(31), 0x6040_0000_0010_0054 + 4 * 31);
    assert_eq!(FPSR, 0x6020_0000_0010_00d4);
    assert_eq!(FPCR, 0x6020_0000_0010_00d5);
    // kvm_regs: 34*8 + 8 + 8 + 5*8 = 328 -> vregs aligned to 336 -> word 84 = 0x54
    assert_eq!(336 / 4, 0x54);
    assert_eq!((336 + 32 * 16) / 4, 0xd4);
    assert_eq!(WORDS * 4, 856 - 0 + 0 /* sizeof(kvm_regs) incl. tail padding */);
}

#[test]
fn sysreg_ids() {
    use sys::*;
    // values published in Linux selftests / QEMU
    assert_eq!(SCTLR_EL1, 0x6030_0000_0013_c080);
    assert_eq!(TTBR0_EL1, 0x6030_0000_0013_c100);
    assert_eq!(TTBR1_EL1, 0x6030_0000_0013_c101);
    assert_eq!(TCR_EL1, 0x6030_0000_0013_c102);
    assert_eq!(MPIDR_EL1, 0x6030_0000_0013_c005);
    assert_eq!(CPACR_EL1, 0x6030_0000_0013_c082);
    assert_eq!(VBAR_EL1, 0x6030_0000_0013_c600);
    assert_eq!(TPIDR_EL0, 0x6030_0000_0013_de82);
    assert_eq!(TIMER_CTL, 0x6030_0000_0013_df19);
    assert_eq!(TIMER_CNT, 0x6030_0000_0013_df1a);
    assert_eq!(TIMER_CVAL, 0x6030_0000_0013_df02);
    assert_eq!(MDSCR_EL1, 0x6030_0000_0013_8012);
    assert_eq!(ID_AA64PFR0_EL1, 0x6030_0000_0013_c020);
    assert_eq!(sysreg_enc(3, 3, 14, 3, 1), 0xdf19);
}

#[test]
fn reg_list_unique_and_decodable() {
    for (i, &a) in SYSREG_LIST.iter().enumerate() {
        for &b in &SYSREG_LIST[i + 1..] {
            assert_ne!(a, b);
        }
        assert!(matches!(decode_reg(a), Some(RegId::Sysreg { .. })));
    }
    assert_eq!(KVM_REG_ARM_PSCI_VERSION, 0x6030_0000_0014_0000);
    assert_eq!(decode_reg(KVM_REG_ARM_PSCI_VERSION), Some(RegId::Fw { n: 0 }));
}

#[test]
fn decode_round_trips() {
    for i in 0..31 {
        assert_eq!(decode_reg(coreregs::x(i)), Some(RegId::Core { off_words: 2 * i, size: RegSize::U64 }));
    }
    assert_eq!(decode_reg(coreregs::vreg(5)), Some(RegId::Core { off_words: 0x54 + 20, size: RegSize::U128 }));
    assert_eq!(decode_reg(coreregs::FPCR), Some(RegId::Core { off_words: 0xd5, size: RegSize::U32 }));
    for op0 in [2u64, 3] {
        for op1 in 0..8 {
            for crn in [0u64, 1, 7, 14, 15] {
                for crm in [0u64, 3, 15] {
                    for op2 in [0u64, 5, 7] {
                        let id = sysreg(op0, op1, crn, crm, op2);
                        assert_eq!(
                            decode_reg(id),
                            Some(RegId::Sysreg { op0: op0 as u8, op1: op1 as u8, crn: crn as u8, crm: crm as u8, op2: op2 as u8 })
                        );
                    }
                }
            }
        }
    }
    assert_eq!(ccsidr_reg(2), 0x6020_0000_0011_0002);
    assert_eq!(decode_reg(ccsidr_reg(2)), Some(RegId::Ccsidr { val: 2 }));
}

#[test]
fn decode_rejects_garbage() {
    assert_eq!(decode_reg(0), None);
    assert_eq!(decode_reg(0x4030_0000_0013_c080), None); // KVM_REG_ARM (32-bit), not arm64
    assert_eq!(decode_reg(0x6070_0000_0013_c080), None); // bad size
    assert_eq!(decode_reg(0x6030_0000_0015_0000), None); // unknown coproc
    assert_eq!(decode_reg(0x6030_0000_0011_0100), None); // demux id != CCSIDR
    assert_eq!(decode_reg(0x6030_0001_0013_c080), None); // reserved bits
    assert_eq!(decode_reg(0x6130_0000_0013_c080), None); // arch byte
    assert_eq!(RegSize::U128.bytes(), 16);
}
