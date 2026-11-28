use kvm::esr::*;
use kvm::hw::*;
use kvm::mmio::*;

// Syndromes are built from the ARM ARM ESR_EL2 encodings for the instruction
// named in each comment (EC<<26 | IL<<25 | ISS).
const IL: u64 = 1 << 25;

#[test]
fn hvc_smc_wfx() {
    assert_eq!(decode(0x5A00_0000, 0, 0), Exit::Hvc { imm: 0 }); // hvc #0
    assert_eq!(decode(0x5A00_1234, 0, 0), Exit::Hvc { imm: 0x1234 });
    assert_eq!(decode(0x5E00_0000, 0, 0), Exit::Smc { imm: 0 }); // smc #0
    assert_eq!(decode(0x0600_0000, 0, 0), Exit::Wfx { is_wfe: false }); // wfi
    assert_eq!(decode(0x0600_0001, 0, 0), Exit::Wfx { is_wfe: true }); // wfe
    assert_eq!(insn_len(0x5A00_0000), 4);
    assert_eq!(insn_len(0x5A00_0000 & !IL), 2);
}

#[test]
fn dabort_str_word() {
    // str w0,[x1] to unmapped IPA 0x0900_0018, translation fault L3, ISV SAS=2 SRT=0 WnR=1
    let e = decode(0x9380_0047, 0xFFFF_0000_0000_0018, 0x0900_0018 >> 8);
    match e {
        Exit::DataAbort(d) => {
            assert_eq!(d.ipa, 0x0900_0018);
            assert!(d.isv && d.write && !d.sse && !d.s1ptw && !d.cm && !d.ea);
            assert_eq!((d.size, d.sas, d.srt, d.dfsc), (4, 2, 0, 0x07));
            assert!(!d.sf);
        }
        o => panic!("{:?}", o),
    }
}

#[test]
fn dabort_ldrsb_ldrsw() {
    // ldrsb w3,[x4]: SAS=0 SSE=1 SRT=3 SF=0 read, DFSC 6
    match decode(0x9323_0006, 0x10, 0x4000_0000 >> 8) {
        Exit::DataAbort(d) => {
            assert_eq!((d.size, d.sse, d.srt, d.sf, d.write, d.dfsc), (1, true, 3, false, false, 6));
            assert_eq!(d.ipa, 0x4000_0010);
        }
        o => panic!("{:?}", o),
    }
    // ldrsw x5,[x6]: SAS=2 SSE=1 SRT=5 SF=1
    match decode(0x93A5_8005, 0, 0) {
        Exit::DataAbort(d) => assert_eq!((d.size, d.sse, d.srt, d.sf), (4, true, 5, true)),
        o => panic!("{:?}", o),
    }
    // non-ISV abort (e.g. ldp / DC ZVA style): ISV clear
    match decode(0x9200_0007, 0, 0) {
        Exit::DataAbort(d) => assert!(!d.isv),
        o => panic!("{:?}", o),
    }
    // stage-1 walk fault: S1PTW set
    match decode(0x9200_0087, 0, 0) {
        Exit::DataAbort(d) => assert!(d.s1ptw),
        o => panic!("{:?}", o),
    }
}

#[test]
fn iabort() {
    // instruction fetch from unmapped IPA 0x4000_1000, translation fault L3
    match decode(0x8200_0007, 0x4000_1000, 0x4000_1000 >> 8) {
        Exit::InstAbort(i) => assert_eq!((i.ipa, i.ifsc, i.s1ptw), (0x4000_1000, 7, false)),
        o => panic!("{:?}", o),
    }
}

#[test]
fn sysreg_traps() {
    // mrs x0, id_aa64pfr0_el1
    match decode(0x6230_0009, 0, 0) {
        Exit::SysReg(s) => assert_eq!(
            (s.op0, s.op1, s.crn, s.crm, s.op2, s.rt, s.is_read), (3, 0, 0, 4, 0, 0, true)),
        o => panic!("{:?}", o),
    }
    // msr icc_sgi1r_el1, x1
    match decode(0x623A_3036, 0, 0) {
        Exit::SysReg(s) => assert_eq!(
            (s.op0, s.op1, s.crn, s.crm, s.op2, s.rt, s.is_read), (3, 0, 12, 11, 5, 1, false)),
        o => panic!("{:?}", o),
    }
}

#[test]
fn unknown_and_async() {
    assert_eq!(decode(0x0000_0000, 0, 0), Exit::Unknown { ec: 0, iss: 0 });
    // FP/SIMD trap EC 0x07
    assert_eq!(decode(0x1E00_0000, 0, 0), Exit::Unknown { ec: 7, iss: 0 });
    let r = |k| RawExit { kind: k, esr: 0x5A00_0000, far: 0, hpfar: 0 };
    assert_eq!(decode_raw(&r(EXIT_SYNC)), Exit::Hvc { imm: 0 });
    assert_eq!(decode_raw(&r(EXIT_IRQ)), Exit::Irq);
    assert_eq!(decode_raw(&r(EXIT_FIQ)), Exit::Fiq);
    assert_eq!(decode_raw(&r(EXIT_SERROR)), Exit::SError);
}

#[test]
fn sign_extension() {
    assert_eq!(load_value(true, true, 0, 0x80), 0xFFFF_FFFF_FFFF_FF80);
    assert_eq!(load_value(true, false, 0, 0x80), 0xFFFF_FF80);
    assert_eq!(load_value(true, true, 1, 0x8001), 0xFFFF_FFFF_FFFF_8001);
    assert_eq!(load_value(true, true, 2, 0x8000_0000), 0xFFFF_FFFF_8000_0000);
    assert_eq!(load_value(true, false, 2, 0x8000_0000), 0x8000_0000);
    assert_eq!(load_value(false, true, 0, 0x1FF), 0xFF); // data above access size ignored
    assert_eq!(load_value(true, true, 0, 0x7F), 0x7F);
    assert_eq!(load_value(true, true, 3, u64::MAX), u64::MAX);
    assert_eq!(load_value(false, true, 3, 0x1122_3344_5566_7788), 0x1122_3344_5566_7788);
}

#[test]
fn finish_read_regs() {
    let mut hw = VcpuHw::new_boot(0x4000_0000);
    hw.gp[3] = u64::MAX;
    finish_read(&mut hw, 3, true, false, 0, 0xF0);
    assert_eq!(hw.gp[3], 0xFFFF_FFF0); // Wt write clears the upper half
    finish_read(&mut hw, 31, false, true, 3, 0x1234); // XZR discarded
    assert!(hw.gp.iter().enumerate().all(|(i, v)| i == 3 || *v == 0));
    advance_pc(&mut hw, 4);
    assert_eq!(hw.pc, 0x4000_0004);
}

#[test]
fn layout() {
    use core::mem::offset_of;
    assert_eq!(offset_of!(VcpuHw, sp_el0), 248);
    assert_eq!(offset_of!(VcpuHw, pc), 264);
    assert_eq!(offset_of!(VcpuHw, el1), 288);
    assert_eq!(offset_of!(El1Regs, spsr), 21 * 8);
    assert_eq!(offset_of!(VcpuHw, hcr), 1056);
    assert_eq!(offset_of!(VcpuHw, cntvoff), 1080);
    assert_eq!(offset_of!(RawExit, hpfar), 24);
    assert_eq!(GUEST_HCR & (1 << 27), 0); // TGE clear
    let h = VcpuHw::new(3, HostRegs::zeroed());
    assert_eq!((h.vmpidr, h.pstate), (0x8000_0003, 0x3c5));
}

#[test]
fn sgi_register_decode() {
    use kvm::esr::SysRegTrap;
    let t = |op1, crm, op2, is_read| SysRegTrap { op0: 3, op1, crn: 12, crm, op2, rt: 0, is_read };
    assert!(t(0, 11, 5, false).is_sgi_write()); // ICC_SGI1R_EL1
    assert!(t(1, 11, 6, false).is_sgi_write()); // ICC_ASGI1R_EL1
    assert!(t(2, 11, 7, false).is_sgi_write()); // ICC_SGI0R_EL1
    assert!(!t(0, 11, 7, false).is_sgi_write()); // the old, wrong SGI0R encoding
    assert!(!t(0, 11, 5, true).is_sgi_write());
}
