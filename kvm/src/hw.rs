//! Minimal stand-in for the `VcpuHw` layout from lane kvm-switch (plan
//! section 4 item 4). Everything in kvm::sysreg and kvm::psci touches the
//! guest context only through the types in this module, so swapping in the
//! real struct is a one-file change: replace this module's definitions (or
//! `pub use` the real ones) and keep the field names used by sysreg.rs.
//!
//! The FP/SIMD file, the virtual-timer shadow and `host` (the sanitised
//! host ID snapshot) are not part of the plan's VcpuHw; they live in
//! `El1Regs` / `HostRegs` here and must be provided by the vcpu state.

/// Host ID-register snapshot, read once at KVM init. `id[crm][op2]` holds the
/// raw host value of the ID register op0=3 op1=0 crn=0 crm=`crm` op2=`op2`.
#[derive(Clone, Copy)]
pub struct HostRegs {
    pub id: [[u64; 8]; 8],
    /// CCSIDR_EL1 per CSSELR_EL1 value 0..16 (level<<1 | InD).
    pub ccsidr: [u32; 16],
    pub clidr: u64,
    pub ctr: u64,
    /// Physical counter at the time of the call (CNTPCT_EL0); the caller
    /// refreshes it before get/set of the timer count registers.
    pub cntpct: u64,
}

impl HostRegs {
    pub const fn zeroed() -> Self {
        HostRegs { id: [[0; 8]; 8], ccsidr: [0; 16], clidr: 0, ctr: 0, cntpct: 0 }
    }
}

#[derive(Clone, Copy)]
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
    /// spsr[0] = SPSR_EL1, then abt, und, irq, fiq (kvm_regs.spsr[5]).
    pub spsr: [u64; 5],
    pub vregs: [u128; 32],
    pub fpsr: u32,
    pub fpcr: u32,
    pub cntv_ctl: u64,
    pub cntv_cval: u64,
    pub cntp_ctl: u64,
    pub cntp_cval: u64,
}

#[derive(Clone, Copy)]
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
    pub host: HostRegs,
}

pub const RESET_PSTATE: u64 = 0x3c5; // EL1h, DAIF masked
pub const RESET_SCTLR_EL1: u64 = 0x30d0_0800;

impl VcpuHw {
    /// Reset state of vcpu `id` (KVM_ARM_VCPU_INIT).
    pub const fn new(id: u32, host: HostRegs) -> Self {
        VcpuHw {
            gp: [0; 31],
            sp_el0: 0,
            sp_el1: 0,
            pc: 0,
            pstate: RESET_PSTATE,
            el1: El1Regs {
                sctlr: RESET_SCTLR_EL1,
                cpacr: 0, ttbr0: 0, ttbr1: 0, tcr: 0, esr: 0, afsr0: 0, afsr1: 0,
                far: 0, mair: 0, amair: 0, vbar: 0, contextidr: 0, tpidr_el1: 0,
                tpidr_el0: 0, tpidrro_el0: 0, par: 0, csselr: 0, cntkctl: 0,
                mdscr: 0, elr_el1: 0, spsr: [0; 5], vregs: [0; 32], fpsr: 0,
                fpcr: 0, cntv_ctl: 0, cntv_cval: 0, cntp_ctl: 0, cntp_cval: 0,
            },
            hcr: 0,
            vttbr: 0,
            vmpidr: 0x8000_0000 | (id as u64 & 0xff),
            cntvoff: 0,
            host,
        }
    }
}
