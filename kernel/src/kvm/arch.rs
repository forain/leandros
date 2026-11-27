//! Arch backend (kvm-switch `VcpuHw` + kvm-sysreg accessors) behind `ArchVcpu` (lane kvm-run).
use super::*;
use ::kvm::hw::{HostRegs, VcpuHw, GUEST_HCR, PSTATE_EL1H_DAIF};
use ::kvm::sysreg::{self, Val};
use spin::Once;

macro_rules! rd {
    ($name:literal) => {{
        let v: u64;
        unsafe { core::arch::asm!(concat!("mrs {}, ", $name), out(reg) v, options(nomem, nostack)); }
        v
    }};
}

static HOST: Once<HostRegs> = Once::new();

/// Host ID-register snapshot, taken once (ID registers are identical on every CPU we run on).
pub(super) fn host_regs() -> &'static HostRegs {
    HOST.call_once(|| {
        let mut h = HostRegs::zeroed();
    h.id[1][0] = rd!("S3_0_C0_C1_0");
    h.id[1][1] = rd!("S3_0_C0_C1_1");
    h.id[1][2] = rd!("S3_0_C0_C1_2");
    h.id[1][3] = rd!("S3_0_C0_C1_3");
    h.id[1][4] = rd!("S3_0_C0_C1_4");
    h.id[1][5] = rd!("S3_0_C0_C1_5");
    h.id[1][6] = rd!("S3_0_C0_C1_6");
    h.id[1][7] = rd!("S3_0_C0_C1_7");
    h.id[2][0] = rd!("S3_0_C0_C2_0");
    h.id[2][1] = rd!("S3_0_C0_C2_1");
    h.id[2][2] = rd!("S3_0_C0_C2_2");
    h.id[2][3] = rd!("S3_0_C0_C2_3");
    h.id[2][4] = rd!("S3_0_C0_C2_4");
    h.id[2][5] = rd!("S3_0_C0_C2_5");
    h.id[2][6] = rd!("S3_0_C0_C2_6");
    h.id[2][7] = rd!("S3_0_C0_C2_7");
    h.id[3][0] = rd!("S3_0_C0_C3_0");
    h.id[3][1] = rd!("S3_0_C0_C3_1");
    h.id[3][2] = rd!("S3_0_C0_C3_2");
    h.id[3][3] = rd!("S3_0_C0_C3_3");
    h.id[3][4] = rd!("S3_0_C0_C3_4");
    h.id[3][5] = rd!("S3_0_C0_C3_5");
    h.id[3][6] = rd!("S3_0_C0_C3_6");
    h.id[3][7] = rd!("S3_0_C0_C3_7");
    h.id[4][0] = rd!("S3_0_C0_C4_0");
    h.id[4][1] = rd!("S3_0_C0_C4_1");
    h.id[4][2] = rd!("S3_0_C0_C4_2");
    h.id[4][3] = rd!("S3_0_C0_C4_3");
    h.id[4][4] = rd!("S3_0_C0_C4_4");
    h.id[4][5] = rd!("S3_0_C0_C4_5");
    h.id[4][6] = rd!("S3_0_C0_C4_6");
    h.id[4][7] = rd!("S3_0_C0_C4_7");
    h.id[5][0] = rd!("S3_0_C0_C5_0");
    h.id[5][1] = rd!("S3_0_C0_C5_1");
    h.id[5][2] = rd!("S3_0_C0_C5_2");
    h.id[5][3] = rd!("S3_0_C0_C5_3");
    h.id[5][4] = rd!("S3_0_C0_C5_4");
    h.id[5][5] = rd!("S3_0_C0_C5_5");
    h.id[5][6] = rd!("S3_0_C0_C5_6");
    h.id[5][7] = rd!("S3_0_C0_C5_7");
    h.id[6][0] = rd!("S3_0_C0_C6_0");
    h.id[6][1] = rd!("S3_0_C0_C6_1");
    h.id[6][2] = rd!("S3_0_C0_C6_2");
    h.id[6][3] = rd!("S3_0_C0_C6_3");
    h.id[6][4] = rd!("S3_0_C0_C6_4");
    h.id[6][5] = rd!("S3_0_C0_C6_5");
    h.id[6][6] = rd!("S3_0_C0_C6_6");
    h.id[6][7] = rd!("S3_0_C0_C6_7");
    h.id[7][0] = rd!("S3_0_C0_C7_0");
    h.id[7][1] = rd!("S3_0_C0_C7_1");
    h.id[7][2] = rd!("S3_0_C0_C7_2");
    h.id[7][3] = rd!("S3_0_C0_C7_3");
    h.id[7][4] = rd!("S3_0_C0_C7_4");
    h.id[7][5] = rd!("S3_0_C0_C7_5");
    h.id[7][6] = rd!("S3_0_C0_C7_6");
    h.id[7][7] = rd!("S3_0_C0_C7_7");
        h.clidr = rd!("clidr_el1");
        h.ctr = rd!("ctr_el0");
        let old = rd!("csselr_el1");
        for sel in 0..16u64 {
            unsafe {
                core::arch::asm!("msr csselr_el1, {}", "isb", in(reg) sel, options(nomem, nostack));
            }
            h.ccsidr[sel as usize] = rd!("ccsidr_el1") as u32;
        }
        unsafe { core::arch::asm!("msr csselr_el1, {}", "isb", in(reg) old, options(nomem, nostack)); }
        h
    })
}

pub struct HwVcpu {
    id: u32,
    hw: Box<VcpuHw>,
}

impl HwVcpu {
    /// Reset in place: the run loop holds a pointer to the FP area across KVM_RUN.
    fn reset(&mut self) {
        *self.hw = VcpuHw::new(self.id, *host_regs());
        self.hw.hcr = GUEST_HCR;
    }

    fn fresh(id: u32) -> Box<VcpuHw> {
        let mut hw = Box::new(VcpuHw::new(id, *host_regs()));
        hw.hcr = GUEST_HCR;
        hw
    }
}

impl ArchVcpu for HwVcpu {
    fn init(&mut self, _init: &KvmVcpuInit) -> Result<(), i32> {
        self.reset();
        Ok(())
    }

    fn get_one(&mut self, id: u64, buf: &mut [u8]) -> Result<(), i32> {
        refresh_cnt(&mut self.hw);
        let v = sysreg::get_one(&self.hw, id)?;
        let mut out = [0u8; 16];
        let n = v.to_le(&mut out);
        if n != buf.len() { return Err(22); }
        buf.copy_from_slice(&out[..n]);
        Ok(())
    }

    fn set_one(&mut self, id: u64, buf: &[u8]) -> Result<(), i32> {
        refresh_cnt(&mut self.hw);
        let v: Val = Val::from_le(id, buf)?;
        sysreg::set_one(&mut self.hw, id, v)
    }

    fn reg_list(&self) -> &'static [u64] { sysreg::reg_list() }

    fn hw_mut(&mut self) -> Option<&mut VcpuHw> { Some(&mut self.hw) }

    fn power_on(&mut self, entry: u64, ctx: u64) {
        self.reset();
        self.hw.pc = entry;
        self.hw.gp[0] = ctx;
        self.hw.pstate = PSTATE_EL1H_DAIF;
    }
}

/// The count registers (TIMER_CNT) are derived from the physical counter.
fn refresh_cnt(hw: &mut VcpuHw) { hw.host.cntpct = rd!("cntpct_el0"); }

pub struct Backend;
impl ArchBackend for Backend {
    fn new_vcpu(&self, _vm: &Vm, id: u32) -> Result<Box<dyn ArchVcpu>, i32> {
        Ok(Box::new(HwVcpu { id, hw: HwVcpu::fresh(id) }))
    }
}
pub(super) static BACKEND: Backend = Backend;
