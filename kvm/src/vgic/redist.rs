//! GICv3 redistributor: per-vCPU RD frame (64K) followed by the SGI frame (64K).
use super::dist::{decode, Cls};
use super::*;

pub const RD_STRIDE: u64 = 0x2_0000;

impl Vgic {
    pub(crate) fn redist_typer(&self, vcpu: usize) -> u64 {
        let last = (vcpu + 1 == self.nr_vcpus) as u64;
        ((vcpu as u64) << 32) | ((vcpu as u64) << 8) | (last << 4)
    }

    pub(crate) fn redist_rd32(&self, vcpu: usize, rel: u64, ua: bool) -> u32 {
        if rel < 0x1_0000 {
            return match rel {
                0x4 => self.dist_iidr(),
                0x8 => self.redist_typer(vcpu) as u32,
                0xc => (self.redist_typer(vcpu) >> 32) as u32,
                0xffe8 => 0x3b,
                _ => 0, // CTLR, STATUSR, WAKER: RAZ (awake, LPIs off)
            };
        }
        let off = rel - 0x1_0000;
        match off {
            0x80..=0xcff => {
                let Some((cls, first)) = decode(off) else { return 0 };
                if first >= NR_PRIV {
                    return 0;
                }
                self.class_rd(cls, vcpu, first, ua)
            }
            _ => 0,
        }
    }

    pub(crate) fn redist_wr32(&mut self, vcpu: usize, rel: u64, val: u32, ua: bool) {
        if rel < 0x1_0000 {
            return;
        }
        let off = rel - 0x1_0000;
        if let Some((cls, first)) = decode(off) {
            if first < NR_PRIV && off >= 0x80 {
                self.class_wr(cls, vcpu, first, val, ua);
                self.kick_one(vcpu);
            }
        }
        let _ = Cls::Group;
    }

    pub(crate) fn redist_mmio(&mut self, _vcpu: usize, gpa: u64, len: usize, write: bool, val: &mut u64) -> bool {
        let Some(b) = self.redist_base else { return false };
        if gpa < b || gpa >= b + self.nr_vcpus as u64 * RD_STRIDE {
            return false;
        }
        let idx = ((gpa - b) / RD_STRIDE) as usize;
        let rel = (gpa - b) % RD_STRIDE;
        self.sized(rel, len, write, val, false, |s, o| s.redist_rd32(idx, o, false),
            |s, o, v| s.redist_wr32(idx, o, v, false));
        true
    }
}
