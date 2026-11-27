//! Register-class engine shared by the v2 distributor, v3 distributor and v3 redistributor SGI frame,
//! plus the v3 distributor MMIO handler.
use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cls {
    Group,
    EnS,
    EnC,
    PendS,
    PendC,
    ActS,
    ActC,
    Prio,
    Target,
    Cfg,
}

/// Offset (< 0xD00) -> (class, first irq of the word).
pub fn decode(off: u64) -> Option<(Cls, u32)> {
    let w = |base: u64| ((off - base) / 4 * 32) as u32;
    Some(match off {
        0x080..=0x0ff => (Cls::Group, w(0x80)),
        0x100..=0x17f => (Cls::EnS, w(0x100)),
        0x180..=0x1ff => (Cls::EnC, w(0x180)),
        0x200..=0x27f => (Cls::PendS, w(0x200)),
        0x280..=0x2ff => (Cls::PendC, w(0x280)),
        0x300..=0x37f => (Cls::ActS, w(0x300)),
        0x380..=0x3ff => (Cls::ActC, w(0x380)),
        0x400..=0x7ff => (Cls::Prio, ((off - 0x400) & !3) as u32),
        0x800..=0xbff => (Cls::Target, ((off - 0x800) & !3) as u32),
        0xc00..=0xcff => (Cls::Cfg, ((off - 0xc00) / 4 * 16) as u32),
        _ => return None,
    })
}

/// Set/clear style registers: a sub-word write must not read-modify-write.
pub fn is_w1(off: u64) -> bool {
    (0x100..0x400).contains(&off) || (0xf10..0xf30).contains(&off)
}

impl Vgic {
    /// Read one word of a class. `ua` = userspace (save/restore) view: pending returns the latch only.
    pub(crate) fn class_rd(&self, cls: Cls, vcpu: usize, first: u32, ua: bool) -> u32 {
        let mut out = 0u32;
        let (per, bits) = match cls {
            Cls::Prio | Cls::Target => (4, 8),
            Cls::Cfg => (16, 2),
            _ => (32, 1),
        };
        for k in 0..per {
            let id = first + k;
            if id >= self.nr_irqs {
                break;
            }
            let i = self.irq(vcpu, id);
            let v: u32 = match cls {
                Cls::Group => i.group1 as u32,
                Cls::EnS | Cls::EnC => i.enabled as u32,
                Cls::PendS | Cls::PendC => {
                    if ua && !(self.version == Version::V2 && id < NR_SGI) {
                        i.latch as u32
                    } else {
                        self.is_pending(vcpu, id) as u32
                    }
                }
                Cls::ActS | Cls::ActC => i.active as u32,
                Cls::Prio => i.prio as u32,
                Cls::Target => {
                    if self.version != Version::V2 {
                        0
                    } else if id < NR_PRIV {
                        1 << vcpu
                    } else {
                        i.target as u32
                    }
                }
                Cls::Cfg => (i.edge as u32) << 1,
            };
            out |= v << (k * bits);
        }
        out
    }

    pub(crate) fn class_wr(&mut self, cls: Cls, vcpu: usize, first: u32, val: u32, ua: bool) {
        let (per, bits) = match cls {
            Cls::Prio | Cls::Target => (4, 8),
            Cls::Cfg => (16, 2),
            _ => (32, 1),
        };
        let v2 = self.version == Version::V2;
        for k in 0..per {
            let id = first + k;
            if id >= self.nr_irqs {
                break;
            }
            let f = (val >> (k * bits)) & ((1u32 << bits) - 1);
            let bit = f & 1 != 0;
            let sgi = id < NR_SGI;
            let i = self.irq_mut(vcpu, id);
            match cls {
                Cls::Group => i.group1 = bit,
                Cls::EnS => {
                    if bit {
                        i.enabled = true
                    }
                }
                Cls::EnC => {
                    if bit {
                        i.enabled = false
                    }
                }
                Cls::PendS => {
                    if bit && !(v2 && sgi) {
                        i.latch = true
                    }
                }
                Cls::PendC => {
                    if bit && !(v2 && sgi) {
                        i.latch = false
                    }
                }
                Cls::ActS => {
                    if bit {
                        i.active = true
                    }
                }
                Cls::ActC => {
                    if bit {
                        i.active = false
                    }
                }
                Cls::Prio => i.prio = f as u8,
                Cls::Target => {
                    if v2 && id >= NR_PRIV {
                        i.target = f as u8
                    }
                }
                Cls::Cfg => {
                    if id >= NR_SGI {
                        i.edge = f & 2 != 0
                    }
                }
            }
        }
        let _ = ua;
    }

    pub(crate) fn dist_ctlr(&self) -> u32 {
        match self.version {
            // ARE_NS | DS | EnableGrp1
            Version::V3 => (1 << 4) | (1 << 6) | ((self.dist_enabled as u32) << 1),
            Version::V2 => self.dist_enabled as u32,
        }
    }

    pub(crate) fn dist_typer(&self) -> u32 {
        let lines = (self.nr_irqs / 32 - 1) & 0x1f;
        match self.version {
            Version::V3 => lines | (9 << 19), // IDbits 10
            Version::V2 => lines | (((self.nr_vcpus as u32 - 1) & 7) << 5),
        }
    }

    pub(crate) fn dist_iidr(&self) -> u32 {
        (2 << 24) | (1 << 12) | 0x43b
    }

    /// 32-bit distributor register read (shared by v2 and v3). `vcpu` is the accessing vCPU (banked v2 words).
    pub(crate) fn dist_rd32(&self, vcpu: usize, off: u64, ua: bool) -> u32 {
        match off {
            0x000 => self.dist_ctlr(),
            0x004 => self.dist_typer(),
            0x008 => self.dist_iidr(),
            0x080..=0xcff => {
                let Some((cls, first)) = decode(off) else { return 0 };
                if first < NR_PRIV && self.version == Version::V3 {
                    return 0; // lives in the redistributor
                }
                self.class_rd(cls, vcpu, first, ua)
            }
            0x6100..=0x7fdf if self.version == Version::V3 => {
                let r = self.irouter_rd(off & !7);
                if off & 4 != 0 { (r >> 32) as u32 } else { r as u32 }
            }
            0xffe8 if self.version == Version::V3 => 0x3b,
            0xfe8 if self.version == Version::V2 => 0x2b,
            0xf10..=0xf2f if self.version == Version::V2 => self.v2_sgi_pend_rd(vcpu, off),
            _ => 0,
        }
    }

    pub(crate) fn dist_wr32(&mut self, vcpu: usize, off: u64, val: u32, ua: bool) {
        match off {
            0x000 => {
                self.dist_enabled = match self.version {
                    Version::V3 => val & 2 != 0,
                    Version::V2 => val & 1 != 0,
                };
                self.kick_all();
            }
            0x080..=0xcff => {
                let Some((cls, first)) = decode(off) else { return };
                if first < NR_PRIV && self.version == Version::V3 {
                    return;
                }
                self.class_wr(cls, vcpu, first, val, ua);
                self.kick_all();
            }
            0x6100..=0x7fdf if self.version == Version::V3 => {
                let a = off & !7;
                let mut r = self.irouter_rd(a);
                if off & 4 != 0 {
                    r = (r & 0xffff_ffff) | ((val as u64) << 32);
                } else {
                    r = (r & !0xffff_ffff) | val as u64;
                }
                self.irouter_wr(a, r);
                self.kick_all();
            }
            0xf00 if self.version == Version::V2 => {
                let k = self.inject_sgi(vcpu, val as u64);
                self.kicks |= k;
            }
            0xf10..=0xf2f if self.version == Version::V2 => {
                self.v2_sgi_pend_wr(vcpu, off, val);
                self.kick_all();
            }
            _ => {}
        }
    }

    fn irouter_idx(&self, off: u64) -> Option<u32> {
        let n = ((off - 0x6100) / 8) as u32 + 32;
        if n < self.nr_irqs { Some(n) } else { None }
    }
    pub(crate) fn irouter_rd(&self, off: u64) -> u64 {
        self.irouter_idx(off).map_or(0, |n| self.spis[(n - 32) as usize].route)
    }
    pub(crate) fn irouter_wr(&mut self, off: u64, v: u64) {
        if let Some(n) = self.irouter_idx(off) {
            self.spis[(n - 32) as usize].route = v & 0xff_80ff_ffff;
        }
    }

    /// Sized access with sub-word handling; returns the value for reads.
    pub(crate) fn sized(&mut self, off: u64, len: usize, write: bool, val: &mut u64, is64: bool,
        mut rd: impl FnMut(&mut Self, u64) -> u32, mut wr: impl FnMut(&mut Self, u64, u32)) {
        let _ = is64;
        if len == 8 {
            let o = off & !7;
            if write {
                wr(self, o, *val as u32);
                wr(self, o + 4, (*val >> 32) as u32);
            } else {
                let lo = rd(self, o) as u64;
                let hi = rd(self, o + 4) as u64;
                *val = lo | hi << 32;
            }
            return;
        }
        let o = off & !3;
        let sh = ((off & 3) * 8) as u32;
        let mask: u32 = match len { 1 => 0xff, 2 => 0xffff, _ => 0xffff_ffff };
        if write {
            let v = (*val as u32 & mask) << sh;
            if len == 4 {
                wr(self, o, *val as u32);
            } else if is_w1(o) {
                wr(self, o, v);
            } else {
                let old = rd(self, o);
                wr(self, o, (old & !(mask << sh)) | v);
            }
        } else {
            *val = ((rd(self, o) >> sh) & mask) as u64;
        }
    }

    /// Top-level MMIO. `vcpu` is the faulting vCPU. Returns false if gpa is not ours.
    pub fn mmio(&mut self, vcpu: usize, gpa: u64, len: usize, write: bool, val: &mut u64) -> bool {
        if !matches!(len, 1 | 2 | 4 | 8) || vcpu >= self.nr_vcpus {
            return false;
        }
        if let Some(b) = self.dist_base {
            let size = if self.version == Version::V3 { 0x1_0000 } else { 0x1000 };
            if gpa >= b && gpa < b + size {
                let off = gpa - b;
                self.sized(off, len, write, val, false, |s, o| s.dist_rd32(vcpu, o, false),
                    |s, o, v| s.dist_wr32(vcpu, o, v, false));
                return true;
            }
        }
        match self.version {
            Version::V3 => self.redist_mmio(vcpu, gpa, len, write, val),
            Version::V2 => self.v2_cpu_mmio(vcpu, gpa, len, write, val),
        }
    }
}
