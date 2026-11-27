//! Per-vCPU hardware context shared by the world switch (arch/aarch64/src/kvm)
//! and the pure helpers (ESR decode, MMIO completion). Plain data, no hardware
//! access: the asm in `switch.s` hard-codes these offsets and `ctx.rs` asserts
//! them at compile time.

/// Host ID-register snapshot, read once at KVM init. `id[crm][op2]` holds the
/// raw host value of the ID register op0=3 op1=0 crn=0 crm=`crm` op2=`op2`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostRegs {
    pub id: [[u64; 8]; 8],
    /// CCSIDR_EL1 per CSSELR_EL1 value 0..16 (level<<1 | InD).
    pub ccsidr: [u32; 16],
    pub clidr: u64,
    pub ctr: u64,
    /// Physical counter at the time of the call (CNTPCT_EL0).
    pub cntpct: u64,
}

impl HostRegs {
    pub const fn zeroed() -> Self {
        HostRegs { id: [[0; 8]; 8], ccsidr: [0; 16], clidr: 0, ctr: 0, cntpct: 0 }
    }
}
impl Default for HostRegs { fn default() -> Self { Self::zeroed() } }

/// Guest EL1 system register file. The first 26 u64 (through `spsr`) are
/// switched by `switch.s` via the `*_EL12` encodings (or the direct registers
/// VHE does not redirect) in exactly this order; the rest (FP/SIMD, timer
/// shadow) is switched by the run loop. Field names match lane kvm-sysreg.
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct El1Regs {
    pub sctlr: u64,
    pub cpacr: u64,
    pub ttbr0: u64,
    pub ttbr1: u64,
    pub tcr: u64,
    pub esr: u64,
    pub afsr0: u64,
    pub afsr1: u64,
    pub far: u64,
    pub mair: u64,
    pub amair: u64,
    pub vbar: u64,
    pub contextidr: u64,
    pub tpidr_el1: u64,
    pub tpidr_el0: u64,
    pub tpidrro_el0: u64,
    pub par: u64,
    pub csselr: u64,
    pub cntkctl: u64,
    pub mdscr: u64,
    pub elr_el1: u64,
    /// spsr[0] = SPSR_EL1 (switched in asm), then abt, und, irq, fiq (kvm_regs.spsr[5]).
    pub spsr: [u64; 5],
    pub vregs: [u128; 32],
    pub fpsr: u32,
    pub fpcr: u32,
    pub cntv_ctl: u64,
    pub cntv_cval: u64,
    pub cntp_ctl: u64,
    pub cntp_cval: u64,
}

impl El1Regs {
    pub const fn zeroed() -> Self {
        El1Regs {
            sctlr: 0, cpacr: 0, ttbr0: 0, ttbr1: 0, tcr: 0, esr: 0, afsr0: 0, afsr1: 0,
            far: 0, mair: 0, amair: 0, vbar: 0, contextidr: 0, tpidr_el1: 0,
            tpidr_el0: 0, tpidrro_el0: 0, par: 0, csselr: 0, cntkctl: 0,
            mdscr: 0, elr_el1: 0, spsr: [0; 5], vregs: [0; 32], fpsr: 0,
            fpcr: 0, cntv_ctl: 0, cntv_cval: 0, cntp_ctl: 0, cntp_cval: 0,
        }
    }
}
impl Default for El1Regs { fn default() -> Self { Self::zeroed() } }

/// vGIC list-register state of one vCPU (filled by the vgic lanes; the world
/// switch does not touch it).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct IchState {
    pub hcr: u64,
    pub vmcr: u64,
    pub elrsr: u64,
    pub lr: [u64; 16],
    pub ap0r: [u64; 4],
    pub ap1r: [u64; 4],
}

/// Everything the switch loads into / saves from the CPU for one vCPU.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct VcpuHw {
    pub gp: [u64; 31],
    pub sp_el0: u64,
    pub sp_el1: u64,
    pub pc: u64,
    pub pstate: u64,
    pub el1: El1Regs,
    pub hcr: u64,
    pub vttbr: u64,
    pub vmpidr: u64,
    pub cntvoff: u64,
    pub ich: IchState,
    pub host: HostRegs,
}

/// `kind` values of [`RawExit`].
pub const EXIT_SYNC: u8 = 0;
pub const EXIT_IRQ: u8 = 1;
pub const EXIT_FIQ: u8 = 2;
pub const EXIT_SERROR: u8 = 3;

/// What the exit vector captured. `esr`/`far`/`hpfar` are only meaningful for
/// `EXIT_SYNC` (hpfar only for stage-2 aborts).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct RawExit {
    pub kind: u8,
    pub esr: u64,
    pub far: u64,
    pub hpfar: u64,
}

// HCR_EL2 bits for a guest run (TGE clear). VI/VF/VSE are added by the run lane.
pub const HCR_VM: u64 = 1 << 0;
pub const HCR_SWIO: u64 = 1 << 1;
pub const HCR_FMO: u64 = 1 << 3;
pub const HCR_IMO: u64 = 1 << 4;
pub const HCR_AMO: u64 = 1 << 5;
pub const HCR_VF: u64 = 1 << 6;
pub const HCR_VI: u64 = 1 << 7;
pub const HCR_VSE: u64 = 1 << 8;
pub const HCR_FB: u64 = 1 << 9;
pub const HCR_BSU_IS: u64 = 1 << 10;
pub const HCR_TWI: u64 = 1 << 13;
pub const HCR_TWE: u64 = 1 << 14;
pub const HCR_TID3: u64 = 1 << 18;
pub const HCR_TSC: u64 = 1 << 19;
pub const HCR_TIDCP: u64 = 1 << 20;
pub const HCR_TACR: u64 = 1 << 21;
pub const HCR_TSW: u64 = 1 << 22;
pub const HCR_RW: u64 = 1 << 31;
pub const HCR_E2H: u64 = 1 << 34;
pub const HCR_APK: u64 = 1 << 40;
pub const HCR_API: u64 = 1 << 41;

/// Plan section 3 guest HCR_EL2 (TGE=0).
pub const GUEST_HCR: u64 = HCR_VM | HCR_SWIO | HCR_FMO | HCR_IMO | HCR_AMO | HCR_FB | HCR_BSU_IS
    | HCR_TWI | HCR_TWE | HCR_TID3 | HCR_TSC | HCR_TIDCP | HCR_TACR | HCR_TSW
    | HCR_RW | HCR_E2H | HCR_API | HCR_APK;

/// MDCR_EL2: TPMCR|TPM|TDA|TDOSA|TDRA.
pub const GUEST_MDCR: u64 = (1 << 5) | (1 << 6) | (1 << 9) | (1 << 10) | (1 << 11);
/// CPTR_EL2 (E2H layout): FPEN=0b11 (no FP/SIMD trap), ZEN=0, TTA=0.
pub const GUEST_CPTR: u64 = 0b11 << 20;
/// CNTHCTL_EL2 (E2H layout): EL1PCTEN=1, EL1PTEN=0 (guest ptimer trapped).
pub const GUEST_CNTHCTL: u64 = 1 << 10;

/// PSTATE for entering a guest at EL1h with DAIF masked.
pub const PSTATE_EL1H_DAIF: u64 = 0b0101 | (0xF << 6);

pub const RESET_PSTATE: u64 = 0x3c5; // EL1h, DAIF masked
pub const RESET_SCTLR_EL1: u64 = 0x30d0_0800;

impl VcpuHw {
    /// Reset state of vcpu `id` (KVM_ARM_VCPU_INIT).
    pub const fn new(id: u32, host: HostRegs) -> Self {
        let mut el1 = El1Regs::zeroed();
        el1.sctlr = RESET_SCTLR_EL1;
        VcpuHw {
            gp: [0; 31], sp_el0: 0, sp_el1: 0, pc: 0, pstate: RESET_PSTATE, el1,
            hcr: 0, vttbr: 0, vmpidr: 0x8000_0000 | (id as u64 & 0xff), cntvoff: 0,
            ich: IchState { hcr: 0, vmcr: 0, elrsr: 0, lr: [0; 16], ap0r: [0; 4], ap1r: [0; 4] },
            host,
        }
    }

    /// A zeroed vCPU that will start at `pc` as EL1h, MMU off, with the guest HCR.
    pub fn new_boot(pc: u64) -> Self {
        let mut h = Self::default();
        h.pc = pc;
        h.pstate = PSTATE_EL1H_DAIF;
        h.hcr = GUEST_HCR;
        h.el1.sctlr = 0x30C5_0830; // RES1 bits, MMU/caches off
        h
    }
}
