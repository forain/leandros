//! GICv2 specifics: SGI pending-source registers and the (emulated fallback) GICC interface.
use super::*;

impl Vgic {
    /// GICD_CPENDSGIR (0xf10) / SPENDSGIR (0xf20): one byte per SGI, one bit per source CPU.
    pub(crate) fn v2_sgi_pend_rd(&self, vcpu: usize, off: u64) -> u32 {
        let base = ((off & !3) - if off >= 0xf20 { 0xf20 } else { 0xf10 }) as usize;
        let mut v = 0u32;
        for k in 0..4 {
            v |= (self.cpus[vcpu].sgi_src[base + k] as u32) << (8 * k);
        }
        v
    }

    pub(crate) fn v2_sgi_pend_wr(&mut self, vcpu: usize, off: u64, val: u32) {
        let set = off >= 0xf20;
        let base = ((off & !3) - if set { 0xf20 } else { 0xf10 }) as usize;
        for k in 0..4 {
            let m = (val >> (8 * k)) as u8;
            let s = &mut self.cpus[vcpu].sgi_src[base + k];
            if set { *s |= m } else { *s &= !m }
        }
    }

    pub(crate) fn v2_cpu_rd32(&self, vcpu: usize, off: u64) -> u32 {
        let c = &self.cpus[vcpu].iface;
        match off {
            0x00 => c.ctlr,
            0x04 => c.pmr,
            0x08 => c.bpr0,
            0x1c => c.bpr1,
            0xd0 => c.ap0r[0],
            0xfc => self.dist_iidr(),
            _ => 0, // IAR/EOIR/RPR/HPPIR are served by the GICV hardware frame
        }
    }

    pub(crate) fn v2_cpu_wr32(&mut self, vcpu: usize, off: u64, val: u32) {
        let c = &mut self.cpus[vcpu].iface;
        match off {
            0x00 => c.ctlr = val,
            0x04 => c.pmr = val & 0xff,
            0x08 => c.bpr0 = val & 7,
            0x1c => c.bpr1 = val & 7,
            0xd0 => c.ap0r[0] = val,
            _ => {}
        }
    }

    pub(crate) fn v2_cpu_mmio(&mut self, vcpu: usize, gpa: u64, len: usize, write: bool, val: &mut u64) -> bool {
        let Some(b) = self.cpu_base else { return false };
        if gpa < b || gpa >= b + 0x1000 {
            return false;
        }
        self.sized(gpa - b, len, write, val, false, |s, o| s.v2_cpu_rd32(vcpu, o), |s, o, v| s.v2_cpu_wr32(vcpu, o, v));
        true
    }
}
