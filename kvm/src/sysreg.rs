//! ONE_REG access, KVM_GET_REG_LIST, ID-register sanitising and trapped
//! sysreg emulation for an arm64 vcpu.
//!
//! Errors are positive errno values (ENOENT, EINVAL); the ioctl layer negates.

use crate::abi::{self, coreregs, decode_reg, sys, RegId, RegSize};
use crate::hw::{HostRegs, VcpuHw};

pub const ENOENT: i32 = 2;
pub const EINVAL: i32 = 22;

/// PSCI_VERSION as exposed by KVM_REG_ARM_FW|0 (PSCI 1.0).
pub const FW_PSCI_VERSION: u64 = 0x1_0000;

/// A register value of the width its id declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Val {
    U32(u32),
    U64(u64),
    U128(u128),
}

impl Val {
    pub fn size(&self) -> RegSize {
        match self {
            Val::U32(_) => RegSize::U32,
            Val::U64(_) => RegSize::U64,
            Val::U128(_) => RegSize::U128,
        }
    }
    pub fn as_u128(&self) -> u128 {
        match *self {
            Val::U32(v) => v as u128,
            Val::U64(v) => v as u128,
            Val::U128(v) => v,
        }
    }
    /// Little-endian bytes as copied to/from the user buffer; returns the length.
    pub fn to_le(&self, out: &mut [u8; 16]) -> usize {
        let n = self.size().bytes();
        out[..n].copy_from_slice(&self.as_u128().to_le_bytes()[..n]);
        n
    }
    /// Build the value for register `id` from `bytes` (at least its size).
    pub fn from_le(id: u64, bytes: &[u8]) -> Result<Val, i32> {
        let size = RegSize::from_bits((id & abi::KVM_REG_SIZE_MASK) >> abi::KVM_REG_SIZE_SHIFT)
            .ok_or(EINVAL)?;
        let n = size.bytes();
        if n > 16 || bytes.len() < n {
            return Err(EINVAL);
        }
        let mut b = [0u8; 16];
        b[..n].copy_from_slice(&bytes[..n]);
        let v = u128::from_le_bytes(b);
        Ok(match size {
            RegSize::U32 => Val::U32(v as u32),
            RegSize::U64 => Val::U64(v as u64),
            RegSize::U128 => Val::U128(v),
            _ => return Err(EINVAL),
        })
    }
}

// ------------------------------------------------------------- the list

const N_CORE: usize = 31 + 1 + 4 + 5 + 32 + 2; // x0-30, sp, pc, pstate, sp_el1, elr_el1, spsr[5], vregs, fpsr, fpcr
const N_SYS: usize = abi::SYSREG_LIST.len();
pub const N_STATIC: usize = N_CORE + N_SYS + 1;

const fn build() -> [u64; N_STATIC] {
    let mut l = [0u64; N_STATIC];
    let mut n = 0;
    let mut i = 0;
    while i < 31 {
        l[n] = coreregs::x(i as u64);
        n += 1;
        i += 1;
    }
    l[n] = coreregs::SP;
    l[n + 1] = coreregs::PC;
    l[n + 2] = coreregs::PSTATE;
    l[n + 3] = coreregs::SP_EL1;
    l[n + 4] = coreregs::ELR_EL1;
    n += 5;
    i = 0;
    while i < 5 {
        l[n] = coreregs::spsr(i as u64);
        n += 1;
        i += 1;
    }
    i = 0;
    while i < 32 {
        l[n] = coreregs::vreg(i as u64);
        n += 1;
        i += 1;
    }
    l[n] = coreregs::FPSR;
    l[n + 1] = coreregs::FPCR;
    n += 2;
    i = 0;
    while i < N_SYS {
        l[n] = abi::SYSREG_LIST[i];
        n += 1;
        i += 1;
    }
    l[n] = abi::KVM_REG_ARM_PSCI_VERSION;
    l
}
static LIST: [u64; N_STATIC] = build();

/// Core + sysreg + firmware registers, in the order Linux lists them. The
/// per-level CCSIDR demux entries depend on the host and come from
/// [`reg_list_full`].
pub fn reg_list() -> &'static [u64] {
    &LIST
}

/// Full KVM_GET_REG_LIST contents: `reg_list()` followed by one CCSIDR demux
/// entry per cache (CLIDR Ctype). Fills `out`; Err(needed) when too small
/// (the ioctl then returns -E2BIG with `n = needed`).
pub fn reg_list_full(host: &HostRegs, out: &mut [u64]) -> Result<usize, usize> {
    let mut cs = [0u64; 16];
    let mut ncs = 0;
    for k in 0..7u64 {
        let ctype = (host.clidr >> (3 * k)) & 7;
        if ctype == 0 {
            break;
        }
        if ctype == 2 || ctype == 3 || ctype == 4 {
            cs[ncs] = k << 1;
            ncs += 1;
        }
        if ctype == 1 || ctype == 3 {
            cs[ncs] = (k << 1) | 1;
            ncs += 1;
        }
    }
    let need = N_STATIC + ncs;
    if out.len() < need {
        return Err(need);
    }
    out[..N_STATIC].copy_from_slice(&LIST);
    for i in 0..ncs {
        out[N_STATIC + i] = abi::ccsidr_reg(cs[i]);
    }
    Ok(need)
}

fn ccsidr_listed(host: &HostRegs, val: u8) -> bool {
    let mut buf = [0u64; N_STATIC + 16];
    match reg_list_full(host, &mut buf) {
        Ok(n) => buf[N_STATIC..n].contains(&abi::ccsidr_reg(val as u64)),
        Err(_) => false,
    }
}

// ------------------------------------------------------------- ID regs

/// op0=3 op1=0 crn=0 crm 1..=7: the ID register space (HCR_EL2.TID3 group).
pub const fn is_id_reg(op0: u8, op1: u8, crn: u8, crm: u8) -> bool {
    op0 == 3 && op1 == 0 && crn == 0 && crm >= 1 && crm <= 7
}

const fn clr(v: u64, hi: u32, lo: u32) -> u64 {
    let width = hi - lo + 1;
    let m = if width >= 64 { !0u64 } else { ((1u64 << width) - 1) << lo };
    v & !m
}

/// Hide from the guest everything we do not virtualise. `id` is a ONE_REG id
/// or just the 16-bit sysreg encoding; `host_val` the raw host value.
pub fn sanitize_id(id: u64, host_val: u64) -> u64 {
    let enc = id & 0xffff;
    let crm = ((enc >> 3) & 0xf) as u8;
    let op2 = (enc & 7) as u8;
    let mut v = host_val;
    match (crm, op2) {
        // ID_AA64PFR0: no EL2, RAS, SVE, SEL2, MPAM, AMU, RME; AArch64-only EL0.
        (4, 0) => {
            v = clr(v, 11, 8);
            v = clr(v, 31, 28);
            v = clr(v, 47, 32);
            v = clr(v, 55, 52);
            if v & 0xf != 0 {
                v = (v & !0xf) | 1;
            }
        }
        // ID_AA64PFR1: no MTE, MPAM_frac, RAS_frac, SME, NMI, GCS, THE, MTEX, PFAR.
        (4, 1) => {
            v = clr(v, 11, 8);
            v = clr(v, 19, 12);
            v = clr(v, 27, 24);
            v = clr(v, 63, 36);
        }
        // PFR2, ZFR0, SMFR0, and the RES0 holes: SVE/SME/MTE extras hidden.
        (4, _) => v = 0,
        // ID_AA64DFR0: keep DebugVer, BRPs, WRPs, CTX_CMPs, DoubleLock; hide
        // trace, PMU, SPE, BRBE, MTPMU, PMSS.
        (5, 0) => {
            v = clr(v, 7, 4);
            v = clr(v, 11, 8);
            v = clr(v, 19, 16);
            v = clr(v, 35, 32);
            v = clr(v, 63, 40);
        }
        (5, _) => v = 0,
        // ID_AA64ISAR1: no pointer authentication, no LS64.
        (6, 1) => {
            v = clr(v, 11, 4);
            v = clr(v, 31, 24);
            v = clr(v, 63, 60);
        }
        // ID_AA64ISAR2: no PAuth (APA3/GPA3/PAC_frac).
        (6, 2) => {
            v = clr(v, 15, 8);
            v = clr(v, 27, 24);
        }
        (6, 0) => {}
        (6, _) => v = 0,
        // ID_AA64MMFR0: no FGT, no ECV.
        (7, 0) => v = clr(v, 63, 56),
        // ID_AA64MMFR1: no TWED, HCX, TIDCP1.
        (7, 1) => {
            v = clr(v, 35, 32);
            v = clr(v, 43, 40);
            v = clr(v, 55, 52);
        }
        // ID_AA64MMFR2: no nested virt, no EVT.
        (7, 2) => {
            v = clr(v, 27, 24);
            v = clr(v, 59, 56);
        }
        // MMFR3 (TCRX/SCTLRX/S1PIE/S1POE...), MMFR4, and the rest: hidden.
        (7, _) => v = 0,
        // AArch32 ID_DFR0: no performance monitors, no trace filter.
        (1, 2) => {
            v = clr(v, 27, 24);
            v = clr(v, 31, 28);
        }
        // AArch32 ID_DFR1: no MTPMU.
        (3, 5) => v = clr(v, 3, 0),
        // The remaining AArch32 ID_* / MVFR* registers pass through.
        _ => {}
    }
    v
}

fn id_value(host: &HostRegs, op1: u8, crn: u8, crm: u8, op2: u8) -> Option<u64> {
    if !is_id_reg(3, op1, crn, crm) {
        return None;
    }
    let enc = abi::sysreg_enc(3, 0, 0, crm as u64, op2 as u64);
    Some(sanitize_id(enc, host.id[crm as usize][op2 as usize]))
}

// ------------------------------------------------------------- core regs

fn core_get(hw: &VcpuHw, off: u64, size: RegSize) -> Result<Val, i32> {
    let u64v = |v: u64| if size == RegSize::U64 { Ok(Val::U64(v)) } else { Err(EINVAL) };
    match off {
        0..=0x3c if off % 2 == 0 => u64v(hw.gp[(off / 2) as usize]),
        0x3e => u64v(hw.sp_el0),
        0x40 => u64v(hw.pc),
        0x42 => u64v(hw.pstate),
        0x44 => u64v(hw.sp_el1),
        0x46 => u64v(hw.el1.elr_el1),
        0x48 | 0x4a | 0x4c | 0x4e | 0x50 => u64v(hw.el1.spsr[((off - 0x48) / 2) as usize]),
        0x54..=0xd0 if (off - 0x54) % 4 == 0 => {
            if size == RegSize::U128 {
                Ok(Val::U128(hw.el1.vregs[((off - 0x54) / 4) as usize]))
            } else {
                Err(EINVAL)
            }
        }
        0xd4 | 0xd5 => {
            if size == RegSize::U32 {
                Ok(Val::U32(if off == 0xd4 { hw.el1.fpsr } else { hw.el1.fpcr }))
            } else {
                Err(EINVAL)
            }
        }
        _ => Err(ENOENT),
    }
}

fn core_set(hw: &mut VcpuHw, off: u64, size: RegSize, val: Val) -> Result<(), i32> {
    if val.size() != size {
        return Err(EINVAL);
    }
    // validate the slot and width through the getter
    core_get(hw, off, size)?;
    let v = val.as_u128();
    match off {
        0..=0x3c => hw.gp[(off / 2) as usize] = v as u64,
        0x3e => hw.sp_el0 = v as u64,
        0x40 => hw.pc = v as u64,
        0x42 => {
            // Only AArch64 EL0t/EL1t/EL1h are valid (no 32-bit guests).
            let mode = v as u64 & 0x1f;
            if !matches!(mode, 0x00 | 0x04 | 0x05) {
                return Err(EINVAL);
            }
            hw.pstate = v as u64;
        }
        0x44 => hw.sp_el1 = v as u64,
        0x46 => hw.el1.elr_el1 = v as u64,
        0x48 | 0x4a | 0x4c | 0x4e | 0x50 => hw.el1.spsr[((off - 0x48) / 2) as usize] = v as u64,
        0x54..=0xd0 => hw.el1.vregs[((off - 0x54) / 4) as usize] = v,
        0xd4 => hw.el1.fpsr = v as u32,
        _ => hw.el1.fpcr = v as u32,
    }
    Ok(())
}

// ------------------------------------------------------------- sysregs

fn sys_get(hw: &VcpuHw, id: u64, enc: (u8, u8, u8, u8, u8)) -> Result<u64, i32> {
    let e = &hw.el1;
    Ok(match id {
        sys::SCTLR_EL1 => e.sctlr,
        sys::CPACR_EL1 => e.cpacr,
        sys::TTBR0_EL1 => e.ttbr0,
        sys::TTBR1_EL1 => e.ttbr1,
        sys::TCR_EL1 => e.tcr,
        sys::ESR_EL1 => e.esr,
        sys::AFSR0_EL1 => e.afsr0,
        sys::AFSR1_EL1 => e.afsr1,
        sys::FAR_EL1 => e.far,
        sys::MAIR_EL1 => e.mair,
        sys::AMAIR_EL1 => e.amair,
        sys::VBAR_EL1 => e.vbar,
        sys::CONTEXTIDR_EL1 => e.contextidr,
        sys::TPIDR_EL1 => e.tpidr_el1,
        sys::TPIDR_EL0 => e.tpidr_el0,
        sys::TPIDRRO_EL0 => e.tpidrro_el0,
        sys::PAR_EL1 => e.par,
        sys::CSSELR_EL1 => e.csselr,
        sys::CNTKCTL_EL1 => e.cntkctl,
        sys::MDSCR_EL1 => e.mdscr,
        sys::MPIDR_EL1 => hw.vmpidr,
        sys::TIMER_CTL => e.cntv_ctl,
        sys::TIMER_CVAL => e.cntv_cval,
        // virtual count = physical count - CNTVOFF
        sys::TIMER_CNT => hw.host.cntpct.wrapping_sub(hw.cntvoff),
        sys::PTIMER_CTL => e.cntp_ctl,
        sys::PTIMER_CVAL => e.cntp_cval,
        sys::PTIMER_CNT => hw.host.cntpct,
        _ => {
            let (op0, op1, crn, crm, op2) = enc;
            if op0 == 3 && op1 == 0 && crn == 0 && crm >= 1 && crm <= 7 {
                return Ok(id_value(&hw.host, op1, crn, crm, op2).unwrap_or(0));
            }
            return Err(ENOENT);
        }
    })
}

fn sys_set(hw: &mut VcpuHw, id: u64, enc: (u8, u8, u8, u8, u8), v: u64) -> Result<(), i32> {
    let (op0, op1, crn, crm, op2) = enc;
    match id {
        sys::SCTLR_EL1 => hw.el1.sctlr = v,
        sys::CPACR_EL1 => hw.el1.cpacr = v,
        sys::TTBR0_EL1 => hw.el1.ttbr0 = v,
        sys::TTBR1_EL1 => hw.el1.ttbr1 = v,
        sys::TCR_EL1 => hw.el1.tcr = v,
        sys::ESR_EL1 => hw.el1.esr = v,
        sys::AFSR0_EL1 => hw.el1.afsr0 = v,
        sys::AFSR1_EL1 => hw.el1.afsr1 = v,
        sys::FAR_EL1 => hw.el1.far = v,
        sys::MAIR_EL1 => hw.el1.mair = v,
        sys::AMAIR_EL1 => hw.el1.amair = v,
        sys::VBAR_EL1 => hw.el1.vbar = v,
        sys::CONTEXTIDR_EL1 => hw.el1.contextidr = v,
        sys::TPIDR_EL1 => hw.el1.tpidr_el1 = v,
        sys::TPIDR_EL0 => hw.el1.tpidr_el0 = v,
        sys::TPIDRRO_EL0 => hw.el1.tpidrro_el0 = v,
        sys::PAR_EL1 => hw.el1.par = v,
        sys::CSSELR_EL1 => hw.el1.csselr = v,
        sys::CNTKCTL_EL1 => hw.el1.cntkctl = v,
        sys::MDSCR_EL1 => hw.el1.mdscr = v,
        // MPIDR is fixed by the vcpu id; a set must write the same value.
        sys::MPIDR_EL1 => {
            if v != hw.vmpidr {
                return Err(EINVAL);
            }
        }
        sys::TIMER_CTL => hw.el1.cntv_ctl = v,
        sys::TIMER_CVAL => hw.el1.cntv_cval = v,
        sys::TIMER_CNT => hw.cntvoff = hw.host.cntpct.wrapping_sub(v),
        sys::PTIMER_CTL => hw.el1.cntp_ctl = v,
        sys::PTIMER_CVAL => hw.el1.cntp_cval = v,
        // no ECV/CNTPOFF: accepted and ignored
        sys::PTIMER_CNT => {}
        _ => {
            if op0 == 3 && op1 == 0 && crn == 0 && (1..=7).contains(&crm) {
                // ID registers are read-only; a set must equal the sanitised value.
                return if id_value(&hw.host, op1, crn, crm, op2) == Some(v) { Ok(()) } else { Err(EINVAL) };
            }
            return Err(ENOENT);
        }
    }
    Ok(())
}

/// KVM_GET_ONE_REG.
pub fn get_one(hw: &VcpuHw, id: u64) -> Result<Val, i32> {
    match decode_reg(id).ok_or(ENOENT)? {
        RegId::Core { off_words, size } => core_get(hw, off_words, size),
        RegId::Sysreg { op0, op1, crn, crm, op2 } => {
            if id & abi::KVM_REG_SIZE_MASK != (RegSize::U64 as u64) << abi::KVM_REG_SIZE_SHIFT {
                return Err(EINVAL);
            }
            sys_get(hw, id, (op0, op1, crn, crm, op2)).map(Val::U64)
        }
        RegId::Ccsidr { val } => {
            if id & abi::KVM_REG_SIZE_MASK != (RegSize::U32 as u64) << abi::KVM_REG_SIZE_SHIFT {
                return Err(EINVAL);
            }
            if !ccsidr_listed(&hw.host, val) {
                return Err(ENOENT);
            }
            Ok(Val::U32(hw.host.ccsidr[(val & 15) as usize]))
        }
        RegId::Fw { n } => {
            if n == 0 {
                if id != abi::KVM_REG_ARM_PSCI_VERSION {
                    return Err(EINVAL);
                }
                Ok(Val::U64(FW_PSCI_VERSION))
            } else {
                Err(ENOENT)
            }
        }
    }
}

/// KVM_SET_ONE_REG.
pub fn set_one(hw: &mut VcpuHw, id: u64, val: Val) -> Result<(), i32> {
    match decode_reg(id).ok_or(ENOENT)? {
        RegId::Core { off_words, size } => core_set(hw, off_words, size, val),
        RegId::Sysreg { op0, op1, crn, crm, op2 } => {
            let v = match val {
                Val::U64(v) if id & abi::KVM_REG_SIZE_MASK == (RegSize::U64 as u64) << abi::KVM_REG_SIZE_SHIFT => v,
                _ => return Err(EINVAL),
            };
            sys_set(hw, id, (op0, op1, crn, crm, op2), v)
        }
        RegId::Ccsidr { .. } => {
            // read-only: a set must write back the current value
            if get_one(hw, id)? == val { Ok(()) } else { Err(EINVAL) }
        }
        RegId::Fw { .. } => {
            if get_one(hw, id)? == val { Ok(()) } else { Err(EINVAL) }
        }
    }
}

// ------------------------------------------------------------- trapped MSR/MRS

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrapResult {
    /// Emulated; the guest pc was advanced past the instruction.
    Handled,
    /// Inject an UNDEF (synchronous exception) into the guest.
    Undef,
}

/// Emulate a trapped MSR/MRS (EC 0x18) given its ISS. Reads write Rt (XZR is
/// discarded), writes take Rt (XZR reads 0).
pub fn trap(hw: &mut VcpuHw, iss: u32) -> TrapResult {
    let op0 = ((iss >> 20) & 3) as u8;
    let op2 = ((iss >> 17) & 7) as u8;
    let op1 = ((iss >> 14) & 7) as u8;
    let crn = ((iss >> 10) & 0xf) as u8;
    let rt = ((iss >> 5) & 0x1f) as usize;
    let crm = ((iss >> 1) & 0xf) as u8;
    let read = iss & 1 != 0;
    let enc = abi::sysreg_enc(op0 as u64, op1 as u64, crn as u64, crm as u64, op2 as u64);
    let id = abi::KVM_REG_ARM64 | ((RegSize::U64 as u64) << abi::KVM_REG_SIZE_SHIFT) | abi::KVM_REG_ARM64_SYSREG | enc;
    let e5 = (op0, op1, crn, crm, op2);

    // Result of a read, or None for UNDEF; for writes Some(0) on accept.
    let wval = if rt == 31 { 0 } else { hw.gp[rt] };
    let out: Option<u64> = if abi::SYSREG_LIST.contains(&id) {
        if read {
            sys_get(hw, id, e5).ok()
        } else {
            sys_set(hw, id, e5, wval).ok().map(|_| 0)
        }
    } else if is_id_reg(op0, op1, crn, crm) {
        if read { id_value(&hw.host, op1, crn, crm, op2) } else { None }
    } else if e5 == (3, 3, 0, 0, 1) {
        // CTR_EL0
        if read { Some(hw.host.ctr) } else { None }
    } else if e5 == (3, 1, 0, 0, 0) {
        // CCSIDR_EL1, selected by CSSELR_EL1
        if read { Some(hw.host.ccsidr[(hw.el1.csselr & 15) as usize] as u64) } else { None }
    } else if e5 == (3, 1, 0, 0, 1) {
        if read { Some(hw.host.clidr) } else { None }
    } else if e5 == (2, 0, 1, 1, 4) {
        // OSLSR_EL1: OS lock model implemented, not locked
        if read { Some(0x8) } else { None }
    } else if op0 == 2 || e5 == (3, 0, 1, 0, 1) {
        // external debug registers and ACTLR_EL1: RAZ/WI
        Some(0)
    } else {
        // PMU (crn 9), AMU, SPE, everything unknown: UNDEF
        None
    };
    match out {
        None => TrapResult::Undef,
        Some(v) => {
            if read && rt != 31 {
                hw.gp[rt] = v;
            }
            hw.pc = hw.pc.wrapping_add(4);
            TrapResult::Handled
        }
    }
}
