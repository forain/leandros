//! KVM_CREATE_DEVICE / KVM_{SET,GET,HAS}_DEVICE_ATTR handlers for the vGIC.
use super::abi_shim::*;
use super::redist::RD_STRIDE;
use super::*;

pub const fn sysreg(op0: u64, op1: u64, crn: u64, crm: u64, op2: u64) -> u64 {
    op0 << 14 | op1 << 11 | crn << 7 | crm << 3 | op2
}
pub const ICC_PMR_EL1: u64 = sysreg(3, 0, 4, 6, 0);
pub const ICC_BPR0_EL1: u64 = sysreg(3, 0, 12, 8, 3);
pub const ICC_AP0R0_EL1: u64 = sysreg(3, 0, 12, 8, 4);
pub const ICC_AP1R0_EL1: u64 = sysreg(3, 0, 12, 9, 0);
pub const ICC_BPR1_EL1: u64 = sysreg(3, 0, 12, 12, 3);
pub const ICC_CTLR_EL1: u64 = sysreg(3, 0, 12, 12, 4);
pub const ICC_SRE_EL1: u64 = sysreg(3, 0, 12, 12, 5);
pub const ICC_IGRPEN0_EL1: u64 = sysreg(3, 0, 12, 12, 6);
pub const ICC_IGRPEN1_EL1: u64 = sysreg(3, 0, 12, 12, 7);

impl Vgic {
    /// KVM_CREATE_DEVICE type -> Version (None = unsupported, e.g. ITS). With `test`, only probes.
    pub fn device_version(ty: u32) -> Option<Version> {
        match ty {
            KVM_DEV_TYPE_ARM_VGIC_V2 => Some(Version::V2),
            KVM_DEV_TYPE_ARM_VGIC_V3 => Some(Version::V3),
            _ => None,
        }
    }

    fn range_overlaps(&self, base: u64, size: u64, skip: u64) -> bool {
        let rs = [
            (0u64, self.dist_base, if self.version == Version::V3 { 0x1_0000 } else { 0x1000 }),
            (1, self.redist_base, self.nr_vcpus as u64 * RD_STRIDE),
            (2, self.cpu_base, 0x1_0000),
        ];
        rs.iter().any(|&(k, b, s)| k != skip && b.map_or(false, |b| base < b + s && b < base + size))
    }

    fn vcpu_of_mpidr(&self, attr: u64) -> Result<usize, i32> {
        let v = match self.version {
            Version::V3 => (attr >> 32 & 0xff) as usize, // Aff0 = vcpu id
            Version::V2 => (attr >> 32 & 0xff) as usize,
        };
        if v < self.nr_vcpus { Ok(v) } else { Err(EINVAL) }
    }

    pub fn attr_set(&mut self, group: u32, attr: u64, val: &mut u64) -> Result<(), i32> {
        self.attr_access(group, attr, val, true)
    }
    pub fn attr_get(&mut self, group: u32, attr: u64, val: &mut u64) -> Result<(), i32> {
        self.attr_access(group, attr, val, false)
    }

    pub fn attr_has(&self, group: u32, attr: u64) -> Result<(), i32> {
        let v3 = self.version == Version::V3;
        let ok = match group {
            GRP_ADDR => matches!(
                (v3, attr),
                (false, V2_ADDR_DIST) | (false, V2_ADDR_CPU) | (true, V3_ADDR_DIST) | (true, V3_ADDR_REDIST)
                    | (true, V3_ADDR_REDIST_REGION)
            ),
            GRP_DIST_REGS => true,
            GRP_CPU_REGS => !v3,
            GRP_REDIST_REGS | GRP_LEVEL_INFO => v3,
            GRP_CPU_SYSREGS => v3 && self.sysreg_known(attr & 0xffff),
            GRP_NR_IRQS => true,
            GRP_CTRL => attr == CTRL_INIT,
            _ => false,
        };
        if ok { Ok(()) } else { Err(ENXIO) }
    }

    fn sysreg_known(&self, r: u64) -> bool {
        matches!(r, ICC_PMR_EL1 | ICC_BPR0_EL1 | ICC_BPR1_EL1 | ICC_CTLR_EL1 | ICC_SRE_EL1 | ICC_IGRPEN0_EL1 | ICC_IGRPEN1_EL1)
            || (ICC_AP0R0_EL1..ICC_AP0R0_EL1 + 4).contains(&r)
            || (ICC_AP1R0_EL1..ICC_AP1R0_EL1 + 4).contains(&r)
    }

    fn attr_access(&mut self, group: u32, attr: u64, val: &mut u64, write: bool) -> Result<(), i32> {
        self.attr_has(group, attr)?;
        let v3 = self.version == Version::V3;
        match group {
            GRP_ADDR => self.addr_access(attr, val, write),
            GRP_NR_IRQS => {
                if write {
                    if self.ready {
                        return Err(EBUSY);
                    }
                    let n = *val as u32;
                    if n < 64 || n > MAX_IRQS || n & 31 != 0 {
                        return Err(EINVAL);
                    }
                    self.set_nr_irqs(n);
                } else {
                    *val = self.nr_irqs as u64;
                }
                Ok(())
            }
            GRP_CTRL => {
                if self.ready {
                    return Ok(());
                }
                let ok = if v3 {
                    self.dist_base.is_some() && self.redist_base.is_some()
                } else {
                    self.dist_base.is_some() && self.cpu_base.is_some()
                };
                if !ok {
                    return Err(ENXIO);
                }
                if v3 && self.redist_count != 0 && (self.redist_count as usize) < self.nr_vcpus {
                    return Err(EINVAL);
                }
                self.ready = true;
                Ok(())
            }
            _ => {
                if !self.ready {
                    return Err(EBUSY);
                }
                self.reg_access(group, attr, val, write)
            }
        }
    }

    fn addr_access(&mut self, attr: u64, val: &mut u64, write: bool) -> Result<(), i32> {
        let (slot, size, align): (u64, u64, u64) = match attr {
            V2_ADDR_DIST => (0, 0x1000, 0x1000),
            V2_ADDR_CPU => (2, 0x1_0000, 0x1000),
            V3_ADDR_DIST => (0, 0x1_0000, 0x1_0000),
            V3_ADDR_REDIST | V3_ADDR_REDIST_REGION => (1, self.nr_vcpus as u64 * RD_STRIDE, 0x1_0000),
            _ => return Err(ENXIO),
        };
        let cur = match slot {
            0 => self.dist_base,
            1 => self.redist_base,
            _ => self.cpu_base,
        };
        if !write {
            let b = cur.ok_or(ENXIO)?;
            *val = if attr == V3_ADDR_REDIST_REGION { b | (self.redist_count as u64) << 52 } else { b };
            return Ok(());
        }
        if self.ready {
            return Err(EBUSY);
        }
        if cur.is_some() {
            return Err(EEXIST);
        }
        let (base, count) = if attr == V3_ADDR_REDIST_REGION {
            if *val & 0xfff != 0 {
                return Err(EINVAL); // only region index 0
            }
            (*val & 0x000f_ffff_ffff_f000, (*val >> 52) as u32)
        } else {
            (*val, 0)
        };
        if base & (align - 1) != 0 || base.checked_add(size).is_none() {
            return Err(EINVAL);
        }
        if attr == V3_ADDR_REDIST_REGION && count == 0 {
            return Err(EINVAL);
        }
        if self.range_overlaps(base, size, slot) {
            return Err(EINVAL);
        }
        match slot {
            0 => self.dist_base = Some(base),
            1 => {
                self.redist_base = Some(base);
                self.redist_count = count;
            }
            _ => self.cpu_base = Some(base),
        }
        Ok(())
    }

    fn reg_access(&mut self, group: u32, attr: u64, val: &mut u64, write: bool) -> Result<(), i32> {
        let off = attr & 0xffff_ffff;
        let v3 = self.version == Version::V3;
        match group {
            GRP_DIST_REGS => {
                let vcpu = if v3 { 0 } else { self.vcpu_of_mpidr(attr)? };
                if v3 && (0x6100..0x7fe0).contains(&off) && off & 7 == 0 {
                    if write { self.irouter_wr(off, *val) } else { *val = self.irouter_rd(off) }
                } else if write {
                    self.dist_wr32(vcpu, off, *val as u32, true);
                    self.kick_all();
                } else {
                    *val = self.dist_rd32(vcpu, off, true) as u64;
                }
                Ok(())
            }
            GRP_REDIST_REGS => {
                let vcpu = self.vcpu_of_mpidr(attr)?;
                if !write && off == 8 {
                    *val = self.redist_typer(vcpu);
                } else if write {
                    self.redist_wr32(vcpu, off, *val as u32, true)
                } else {
                    *val = self.redist_rd32(vcpu, off, true) as u64;
                }
                Ok(())
            }
            GRP_CPU_REGS => {
                let vcpu = self.vcpu_of_mpidr(attr)?;
                if write { self.v2_cpu_wr32(vcpu, off, *val as u32) } else { *val = self.v2_cpu_rd32(vcpu, off) as u64 }
                Ok(())
            }
            GRP_LEVEL_INFO => {
                let vcpu = self.vcpu_of_mpidr(attr)?;
                let base = off as u32;
                if base & 31 != 0 || base >= self.nr_irqs {
                    return Err(EINVAL);
                }
                if write {
                    for k in 0..32 {
                        let id = base + k;
                        if id >= self.nr_irqs || id < NR_SGI {
                            continue;
                        }
                        let _ = self.set_line(id, vcpu, *val >> k & 1 != 0);
                    }
                    self.kick_all();
                } else {
                    let mut v = 0u64;
                    for k in 0..32 {
                        let id = base + k;
                        if id < self.nr_irqs && id >= NR_SGI && self.irq(vcpu, id).line {
                            v |= 1 << k;
                        }
                    }
                    *val = v;
                }
                Ok(())
            }
            GRP_CPU_SYSREGS => {
                let vcpu = self.vcpu_of_mpidr(attr)?;
                let r = attr & 0xffff;
                let c = &mut self.cpus[vcpu].iface;
                let slot: &mut u32 = match r {
                    ICC_PMR_EL1 => &mut c.pmr,
                    ICC_BPR0_EL1 => &mut c.bpr0,
                    ICC_BPR1_EL1 => &mut c.bpr1,
                    ICC_CTLR_EL1 => &mut c.ctlr,
                    ICC_IGRPEN0_EL1 => &mut c.igrpen0,
                    ICC_IGRPEN1_EL1 => &mut c.igrpen1,
                    ICC_SRE_EL1 => {
                        if !write {
                            *val = 7;
                        }
                        return Ok(());
                    }
                    x if (ICC_AP0R0_EL1..ICC_AP0R0_EL1 + 4).contains(&x) => &mut c.ap0r[(x - ICC_AP0R0_EL1) as usize],
                    x => &mut c.ap1r[(x - ICC_AP1R0_EL1) as usize],
                };
                if write { *slot = *val as u32 } else { *val = *slot as u64 }
                Ok(())
            }
            _ => Err(ENXIO),
        }
    }
}
