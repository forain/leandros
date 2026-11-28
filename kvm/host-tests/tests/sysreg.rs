use kvm::abi::{self, coreregs, sys};
use kvm::hw::{HostRegs, VcpuHw};
use kvm::sysreg::*;

fn host() -> HostRegs {
    let mut h = HostRegs::zeroed();
    for crm in 0..8 {
        for op2 in 0..8 {
            h.id[crm][op2] = !0u64; // worst case: every feature advertised
        }
    }
    h.clidr = 0x0a20_0023; // L1: separate I+D (3), L2 unified (4), L3 unified (4)? decoded below
    for i in 0..16 {
        h.ccsidr[i] = 0x7003_e000 + i as u32;
    }
    h.ctr = 0x8444_c004;
    h.cntpct = 1_000_000;
    h
}

fn hw() -> VcpuHw {
    VcpuHw::new(3, host())
}

fn val_for(id: u64, seed: u64) -> Val {
    match abi::decode_reg(id).unwrap() {
        abi::RegId::Core { size, .. } => match size {
            abi::RegSize::U32 => Val::U32(seed as u32),
            abi::RegSize::U128 => Val::U128(((seed as u128) << 64) | !seed as u128),
            _ => Val::U64(seed),
        },
        _ => Val::U64(seed),
    }
}

#[test]
fn list_matches_plan_shape() {
    let l = reg_list();
    assert_eq!(l.len(), N_STATIC);
    assert!(l.contains(&coreregs::PC) && l.contains(&coreregs::PSTATE) && l.contains(&coreregs::FPCR));
    assert!(l.contains(&coreregs::x(30)) && l.contains(&coreregs::vreg(31)));
    for r in abi::SYSREG_LIST {
        assert!(l.contains(&r));
    }
    assert!(l.contains(&abi::KVM_REG_ARM_PSCI_VERSION));
    let mut s = l.to_vec();
    s.sort();
    s.dedup();
    assert_eq!(s.len(), l.len(), "duplicate ids");
    // QEMU timer id quirk kept
    assert_eq!(sys::TIMER_CNT, abi::sysreg(3, 3, 14, 3, 2));
    assert_eq!(sys::TIMER_CVAL, abi::sysreg(3, 3, 14, 0, 2));
}

#[test]
fn getset_roundtrip_every_listed_reg() {
    let mut h = hw();
    let mut full = [0u64; 128];
    let n = reg_list_full(&h.host, &mut full).unwrap();
    assert!(n > N_STATIC);
    let mut changed = 0;
    for (i, &id) in full[..n].iter().enumerate() {
        let before = get_one(&h, id).unwrap_or_else(|e| panic!("get {id:#x}: {e}"));
        // writing back the current value is always accepted (QEMU reset put)
        set_one(&mut h, id, before).unwrap_or_else(|e| panic!("put-back {id:#x}: {e}"));
        assert_eq!(get_one(&h, id).unwrap(), before);
        // a new value round-trips for the writable ones
        let new = val_for(id, 0x1111_2222_3333_0000 + i as u64);
        let new = if id == coreregs::PSTATE { Val::U64(0x3c4) } else { new };
        match set_one(&mut h, id, new) {
            Ok(()) => {
                changed += 1;
                let got = get_one(&h, id).unwrap();
                assert_eq!(got, new, "reg {id:#x}");
                set_one(&mut h, id, got).unwrap();
            }
            Err(e) => {
                assert_eq!(e, EINVAL);
                let ro = id == sys::MPIDR_EL1
                    || id == abi::KVM_REG_ARM_PSCI_VERSION
                    || matches!(abi::decode_reg(id), Some(abi::RegId::Ccsidr { .. }));
                assert!(ro, "unexpected EINVAL for {id:#x}");
                assert_eq!(get_one(&h, id).unwrap(), before);
            }
        }
    }
    assert!(changed >= 95);
}

#[test]
fn ccsidr_demux_follows_clidr() {
    let h = hw();
    let mut buf = [0u64; 128];
    let n = reg_list_full(&h.host, &mut buf).unwrap();
    // L1 ctype 3 => csselr 0,1 ; L2 ctype 4 => csselr 2 ; L3 ctype 0 => stop
    let extra = &buf[N_STATIC..n];
    assert_eq!(extra, &[abi::ccsidr_reg(0), abi::ccsidr_reg(1), abi::ccsidr_reg(2)][..]);
    assert_eq!(get_one(&h, abi::ccsidr_reg(2)).unwrap(), Val::U32(0x7003_e002));
    assert_eq!(get_one(&h, abi::ccsidr_reg(5)), Err(ENOENT));
    let mut small = [0u64; 4];
    assert_eq!(reg_list_full(&h.host, &mut small), Err(n));
}

#[test]
fn errors_and_core_rules() {
    let mut h = hw();
    assert_eq!(get_one(&h, 0), Err(ENOENT));
    // wrong width
    assert_eq!(get_one(&h, abi::core_reg(0x40, abi::RegSize::U32)), Err(EINVAL));
    assert_eq!(set_one(&mut h, coreregs::PC, Val::U32(1)), Err(EINVAL));
    // AArch32 / EL2 pstate refused
    assert_eq!(set_one(&mut h, coreregs::PSTATE, Val::U64(0x1d3)), Err(EINVAL));
    assert_eq!(set_one(&mut h, coreregs::PSTATE, Val::U64(0x3c9)), Err(EINVAL));
    assert_eq!(h.pstate, 0x3c5);
    // unknown sysreg and fw reg
    assert_eq!(get_one(&h, abi::sysreg(3, 7, 15, 15, 7)), Err(ENOENT));
    assert_eq!(get_one(&h, abi::fw_reg(5)), Err(ENOENT));
    // reset values + MPIDR
    assert_eq!(get_one(&h, sys::SCTLR_EL1), Ok(Val::U64(0x30d0_0800)));
    assert_eq!(get_one(&h, sys::MPIDR_EL1), Ok(Val::U64(0x8000_0003)));
    assert_eq!(set_one(&mut h, sys::MPIDR_EL1, Val::U64(1)), Err(EINVAL));
    assert_eq!(get_one(&h, abi::KVM_REG_ARM_PSCI_VERSION), Ok(Val::U64(0x10000)));
}

#[test]
fn timer_count_uses_offset_and_byte_codec() {
    let mut h = hw();
    set_one(&mut h, sys::TIMER_CNT, Val::U64(400)).unwrap();
    assert_eq!(h.cntvoff, 1_000_000 - 400);
    h.host.cntpct = 1_000_100;
    assert_eq!(get_one(&h, sys::TIMER_CNT), Ok(Val::U64(500)));
    set_one(&mut h, sys::PTIMER_CNT, Val::U64(7)).unwrap();
    let v = Val::from_le(coreregs::vreg(0), &[1u8; 16]).unwrap();
    let mut b = [0u8; 16];
    assert_eq!(v.to_le(&mut b), 16);
    assert_eq!(b, [1u8; 16]);
    assert_eq!(Val::from_le(coreregs::FPSR, &[1, 0, 0, 0]), Ok(Val::U32(1)));
    assert_eq!(Val::from_le(coreregs::PC, &[1, 2]), Err(EINVAL));
}

fn field(v: u64, hi: u32, lo: u32) -> u64 {
    (v >> lo) & ((1u64 << (hi - lo + 1)) - 1)
}

#[test]
fn id_masking() {
    let h = hw();
    let g = |r: u64| match get_one(&h, r) {
        Ok(Val::U64(v)) => v,
        x => panic!("{x:?}"),
    };
    let pfr0 = g(sys::ID_AA64PFR0_EL1);
    assert_eq!(field(pfr0, 35, 32), 0, "SVE");
    assert_eq!(field(pfr0, 47, 44), 0, "AMU");
    assert_eq!(field(pfr0, 43, 40), 0, "MPAM");
    assert_eq!(field(pfr0, 31, 28), 0, "RAS");
    assert_eq!(field(pfr0, 11, 8), 0, "EL2");
    assert_eq!(field(pfr0, 3, 0), 1, "EL0 AArch64-only");
    assert_eq!(field(pfr0, 59, 56), 0xf, "CSV2 kept");
    let pfr1 = g(sys::ID_AA64PFR1_EL1);
    assert_eq!(field(pfr1, 11, 8), 0, "MTE");
    assert_eq!(field(pfr1, 27, 24), 0, "SME");
    assert_eq!(field(pfr1, 3, 0), 0xf, "BT kept");
    let dfr0 = g(sys::ID_AA64DFR0_EL1);
    assert_eq!(field(dfr0, 11, 8), 0, "PMUVer");
    assert_eq!(field(dfr0, 35, 32), 0, "SPE");
    assert_eq!(field(dfr0, 7, 4), 0, "TraceVer");
    assert_eq!(field(dfr0, 3, 0), 0xf, "DebugVer kept");
    assert_eq!(g(sys::ID_AA64ZFR0_EL1), 0);
    assert_eq!(g(abi::sysreg(3, 0, 0, 4, 5)), 0, "SMFR0");
    assert_eq!(g(abi::sysreg(3, 0, 0, 4, 2)), 0, "PFR2");
    assert_eq!(g(abi::sysreg(3, 0, 0, 5, 1)), 0, "DFR1");
    let isar1 = g(sys::ID_AA64ISAR1_EL1);
    assert_eq!(field(isar1, 11, 4), 0, "PAuth APA/API");
    assert_eq!(field(isar1, 31, 24), 0, "PAuth GPA/GPI");
    assert_eq!(g(sys::ID_AA64ISAR0_EL1), !0);
    assert_eq!(field(g(sys::ID_AA64MMFR0_EL1), 63, 60), 0, "ECV");
    assert_eq!(field(g(sys::ID_AA64MMFR2_EL1), 27, 24), 0, "NV");
    assert_eq!(g(abi::sysreg(3, 0, 0, 7, 3)), 0, "MMFR3");
    // AArch32 ID_DFR0 performance monitors hidden
    assert_eq!(field(g(abi::sysreg(3, 0, 0, 1, 2)), 27, 24), 0);
    // sanitize_id takes a bare encoding too, and is idempotent
    let enc = abi::sysreg_enc(3, 0, 0, 4, 0);
    assert_eq!(sanitize_id(enc, !0), pfr0);
    assert_eq!(sanitize_id(enc, pfr0), pfr0);
    // ID regs: set equal ok, different EINVAL
    let mut h2 = hw();
    assert_eq!(set_one(&mut h2, sys::ID_AA64PFR0_EL1, Val::U64(pfr0)), Ok(()));
    assert_eq!(set_one(&mut h2, sys::ID_AA64PFR0_EL1, Val::U64(!0)), Err(EINVAL));
}

fn iss(op0: u32, op1: u32, crn: u32, crm: u32, op2: u32, rt: u32, read: bool) -> u32 {
    (op0 << 20) | (op2 << 17) | (op1 << 14) | (crn << 10) | (rt << 5) | (crm << 1) | read as u32
}

#[test]
fn trapped_sysregs() {
    let mut h = hw();
    h.pc = 0x1000;
    // MRS x5, ID_AA64PFR0_EL1
    assert_eq!(trap(&mut h, iss(3, 0, 0, 4, 0, 5, true)), TrapResult::Handled);
    assert_eq!(h.gp[5], get_one(&h, sys::ID_AA64PFR0_EL1).map(|v| v.as_u128() as u64).unwrap());
    assert_eq!(h.pc, 0x1004);
    // MSR to an ID register -> UNDEF, pc unchanged
    assert_eq!(trap(&mut h, iss(3, 0, 0, 4, 0, 5, false)), TrapResult::Undef);
    assert_eq!(h.pc, 0x1004);
    // MSR TTBR0_EL1, x7 ; MRS x8, TTBR0_EL1 ; XZR source/sink
    h.gp[7] = 0xdead_beef;
    assert_eq!(trap(&mut h, iss(3, 0, 2, 0, 0, 7, false)), TrapResult::Handled);
    assert_eq!(h.el1.ttbr0, 0xdead_beef);
    assert_eq!(trap(&mut h, iss(3, 0, 2, 0, 0, 8, true)), TrapResult::Handled);
    assert_eq!(h.gp[8], 0xdead_beef);
    assert_eq!(trap(&mut h, iss(3, 0, 2, 0, 0, 31, false)), TrapResult::Handled);
    assert_eq!(h.el1.ttbr0, 0);
    // CSSELR=2, CCSIDR/CLIDR/CTR reads
    h.el1.csselr = 2;
    assert_eq!(trap(&mut h, iss(3, 1, 0, 0, 0, 1, true)), TrapResult::Handled);
    assert_eq!(h.gp[1], 0x7003_e002);
    assert_eq!(trap(&mut h, iss(3, 1, 0, 0, 1, 2, true)), TrapResult::Handled);
    assert_eq!(h.gp[2], h.host.clidr);
    assert_eq!(trap(&mut h, iss(3, 3, 0, 0, 1, 3, true)), TrapResult::Handled);
    assert_eq!(h.gp[3], h.host.ctr);
    // PMU (PMCR_EL0 = 3,3,9,12,0) -> UNDEF; debug regs RAZ/WI; OSLSR
    assert_eq!(trap(&mut h, iss(3, 3, 9, 12, 0, 4, true)), TrapResult::Undef);
    h.gp[4] = 99;
    assert_eq!(trap(&mut h, iss(2, 0, 0, 0, 4, 4, true)), TrapResult::Handled);
    assert_eq!(h.gp[4], 0);
    assert_eq!(trap(&mut h, iss(2, 0, 1, 1, 4, 4, true)), TrapResult::Handled);
    assert_eq!(h.gp[4], 8);
    // MPIDR write refused
    assert_eq!(trap(&mut h, iss(3, 0, 0, 0, 5, 4, false)), TrapResult::Undef);
    // unknown register
    assert_eq!(trap(&mut h, iss(3, 5, 15, 3, 1, 4, true)), TrapResult::Undef);
}
