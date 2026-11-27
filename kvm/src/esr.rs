//! Pure ESR_EL2 decode for exits from a guest (EL1/EL0 AArch64).

use crate::hw::{RawExit, EXIT_FIQ, EXIT_IRQ, EXIT_SERROR};

pub const EC_UNKNOWN: u8 = 0x00;
pub const EC_WFX: u8 = 0x01;
pub const EC_HVC64: u8 = 0x16;
pub const EC_SMC64: u8 = 0x17;
pub const EC_SYSREG: u8 = 0x18;
pub const EC_IABT_LOW: u8 = 0x20;
pub const EC_DABT_LOW: u8 = 0x24;

/// Data abort from the guest with its ISV syndrome (valid when `isv`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataAbort {
    /// Faulting guest physical address: HPFAR_EL2.FIPA<<8 | FAR_EL2[11:0].
    pub ipa: u64,
    pub isv: bool,
    /// Access size in bytes (1/2/4/8); meaningful only when `isv`.
    pub size: u8,
    /// Raw SAS field (log2 of size).
    pub sas: u8,
    /// Sign-extend a load result.
    pub sse: bool,
    /// Target/source register (31 = XZR/WZR).
    pub srt: u8,
    /// 64-bit register width (else 32-bit Wt).
    pub sf: bool,
    pub write: bool,
    /// Fault was on a stage-1 table walk access.
    pub s1ptw: bool,
    /// Cache maintenance operation.
    pub cm: bool,
    /// Synchronous external abort.
    pub ea: bool,
    /// Data fault status code (ESR[5:0]).
    pub dfsc: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InstAbort {
    pub ipa: u64,
    pub s1ptw: bool,
    pub ea: bool,
    pub ifsc: u8,
}

/// A trapped MSR/MRS/system instruction (EC 0x18).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SysRegTrap {
    pub op0: u8,
    pub op1: u8,
    pub crn: u8,
    pub crm: u8,
    pub op2: u8,
    pub rt: u8,
    /// MRS (read from the register into Rt).
    pub is_read: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// WFI (`is_wfe` false) or WFE.
    Wfx { is_wfe: bool },
    Hvc { imm: u16 },
    Smc { imm: u16 },
    SysReg(SysRegTrap),
    DataAbort(DataAbort),
    InstAbort(InstAbort),
    Irq,
    Fiq,
    SError,
    /// Anything else: exception class and ISS.
    Unknown { ec: u8, iss: u32 },
}

#[inline]
pub const fn ec(esr: u64) -> u8 { ((esr >> 26) & 0x3F) as u8 }
#[inline]
pub const fn iss(esr: u64) -> u32 { (esr & 0x01FF_FFFF) as u32 }
/// Instruction length of the trapped instruction in bytes (ESR.IL).
#[inline]
pub const fn insn_len(esr: u64) -> u64 { if esr & (1 << 25) != 0 { 4 } else { 2 } }

/// IPA of a stage-2 abort: HPFAR_EL2.FIPA (bits 43:4 = IPA[51:12]) plus the page offset of FAR.
#[inline]
pub const fn fault_ipa(far: u64, hpfar: u64) -> u64 { ((hpfar & 0x0000_0FFF_FFFF_FFF0) << 8) | (far & 0xFFF) }

/// Decode a synchronous exit syndrome.
pub fn decode(esr: u64, far: u64, hpfar: u64) -> Exit {
    let i = iss(esr);
    match ec(esr) {
        EC_WFX => Exit::Wfx { is_wfe: i & 1 != 0 },
        EC_HVC64 => Exit::Hvc { imm: (i & 0xFFFF) as u16 },
        EC_SMC64 => Exit::Smc { imm: (i & 0xFFFF) as u16 },
        EC_SYSREG => Exit::SysReg(SysRegTrap {
            op0: ((i >> 20) & 3) as u8,
            op2: ((i >> 17) & 7) as u8,
            op1: ((i >> 14) & 7) as u8,
            crn: ((i >> 10) & 0xF) as u8,
            rt: ((i >> 5) & 0x1F) as u8,
            crm: ((i >> 1) & 0xF) as u8,
            is_read: i & 1 != 0,
        }),
        EC_DABT_LOW => {
            let sas = ((i >> 22) & 3) as u8;
            Exit::DataAbort(DataAbort {
                ipa: fault_ipa(far, hpfar),
                isv: i & (1 << 24) != 0,
                size: 1 << sas,
                sas,
                sse: i & (1 << 21) != 0,
                srt: ((i >> 16) & 0x1F) as u8,
                sf: i & (1 << 15) != 0,
                write: i & (1 << 6) != 0,
                s1ptw: i & (1 << 7) != 0,
                cm: i & (1 << 8) != 0,
                ea: i & (1 << 9) != 0,
                dfsc: (i & 0x3F) as u8,
            })
        }
        EC_IABT_LOW => Exit::InstAbort(InstAbort {
            ipa: fault_ipa(far, hpfar),
            s1ptw: i & (1 << 7) != 0,
            ea: i & (1 << 9) != 0,
            ifsc: (i & 0x3F) as u8,
        }),
        e => Exit::Unknown { ec: e, iss: i },
    }
}

/// Decode a full [`RawExit`] including the asynchronous kinds.
pub fn decode_raw(r: &RawExit) -> Exit {
    match r.kind {
        EXIT_IRQ => Exit::Irq,
        EXIT_FIQ => Exit::Fiq,
        EXIT_SERROR => Exit::SError,
        _ => decode(r.esr, r.far, r.hpfar),
    }
}
