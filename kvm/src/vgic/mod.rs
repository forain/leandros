//! Pure vGIC state machine (GICv2 and GICv3 emulation, list-register flush/fold).
//! No hardware access: the EL2 half sits behind `lr::VgicHw`.
use alloc::vec::Vec;

pub use crate::abi::vgic::*; // the UAPI constants stay reachable as kvm::vgic::GRP_* (this module shadows abi::vgic)
pub mod abi_shim;
pub mod attr;
pub mod dist;
pub mod lr;
pub mod redist;
pub mod v2dist;

use lr::{LrCodec, LrEntry, VgicHw, HCR_EN, HCR_UIE, MISR_EOI, MISR_U};

pub const NR_SGI: u32 = 16;
pub const NR_PRIV: u32 = 32;
pub const MAX_IRQS: u32 = 1024;
pub const MAINT_INTID: u32 = 25;
pub const MAX_VCPUS: usize = 64;

/// One bit per vCPU that must be kicked (woken or sent a kick SGI).
pub type KickMask = u64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Version {
    V2,
    V3,
}

#[derive(Clone, Copy, Debug)]
pub struct Irq {
    pub group1: bool,
    pub prio: u8,
    pub enabled: bool,
    /// Edge pending latch / software pending (ISPENDR).
    pub latch: bool,
    /// Input line level (level-triggered sources).
    pub line: bool,
    pub active: bool,
    pub edge: bool,
    /// v2 ITARGETSR mask (SPIs).
    pub target: u8,
    /// v3 IROUTER (SPIs): bit 31 = IRM, low 32 bits affinity.
    pub route: u64,
}

impl Irq {
    const fn new() -> Self {
        Irq { group1: false, prio: 0, enabled: false, latch: false, line: false, active: false, edge: false, target: 1, route: 0 }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LrOwner {
    pub intid: u32,
    pub src: u8,
    /// The LR was loaded with the active bit (it carries the irq's active state).
    pub act: bool,
    pub eoi: bool,
}

/// Per-vCPU CPU-interface state exposed for save/restore (GICC_* / ICC_*); the hardware owns it while running.
#[derive(Clone, Copy, Debug)]
pub struct CpuIface {
    pub ctlr: u32,
    pub pmr: u32,
    pub bpr0: u32,
    pub bpr1: u32,
    pub ap0r: [u32; 4],
    pub ap1r: [u32; 4],
    pub igrpen0: u32,
    pub igrpen1: u32,
}

pub struct Cpu {
    pub priv_irqs: [Irq; 32],
    /// v2 SGI pending sources (bit per source CPU).
    pub sgi_src: [u8; 16],
    pub lr_owner: Vec<Option<LrOwner>>,
    pub iface: CpuIface,
}

impl Cpu {
    fn new() -> Self {
        let mut priv_irqs = [Irq::new(); 32];
        for (i, irq) in priv_irqs.iter_mut().enumerate() {
            if i < 16 {
                irq.edge = true;
                irq.enabled = true;
            }
        }
        Cpu {
            priv_irqs,
            sgi_src: [0; 16],
            lr_owner: Vec::new(),
            iface: CpuIface { ctlr: 0, pmr: 0, bpr0: 0, bpr1: 0, ap0r: [0; 4], ap1r: [0; 4], igrpen0: 0, igrpen1: 0 },
        }
    }
}

pub struct Vgic {
    pub version: Version,
    pub nr_vcpus: usize,
    pub nr_irqs: u32,
    pub dist_enabled: bool,
    pub spis: Vec<Irq>,
    pub cpus: Vec<Cpu>,
    pub dist_base: Option<u64>,
    pub redist_base: Option<u64>,
    /// v3 REDIST_REGION count (0 for a single REDIST address).
    pub redist_count: u32,
    /// v2 GICV/GICC base.
    pub cpu_base: Option<u64>,
    pub ready: bool,
    kicks: KickMask,
    resample: Vec<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Maint {
    pub eoi: bool,
    pub underflow: bool,
}

impl Vgic {
    pub fn new(v: Version, nr_spis: u32, nr_vcpus: usize) -> Self {
        let mut nr_vcpus = nr_vcpus.clamp(1, MAX_VCPUS);
        if v == Version::V2 {
            nr_vcpus = nr_vcpus.min(8);
        }
        let mut g = Vgic {
            version: v,
            nr_vcpus,
            nr_irqs: 32,
            dist_enabled: false,
            spis: Vec::new(),
            cpus: (0..nr_vcpus).map(|_| Cpu::new()).collect(),
            dist_base: None,
            redist_base: None,
            redist_count: 0,
            cpu_base: None,
            ready: false,
            kicks: 0,
            resample: Vec::new(),
        };
        g.set_nr_irqs(32 + ((nr_spis + 31) & !31));
        g
    }

    /// Shrink to the number of vCPUs the VM really created (KVM_CREATE_DEVICE runs
    /// before any vCPU exists, so the device starts at the maximum). Call before
    /// GRP_CTRL INIT; a no-op once `ready` or when `n` is not smaller.
    pub fn set_nr_vcpus(&mut self, n: usize) {
        let n = n.clamp(1, self.nr_vcpus);
        if self.ready || n >= self.nr_vcpus { return; }
        self.cpus.truncate(n);
        self.nr_vcpus = n;
    }

    /// Resize the SPI array (only meaningful before init).
    pub fn set_nr_irqs(&mut self, nr_irqs: u32) {
        let n = nr_irqs.clamp(32, MAX_IRQS);
        self.nr_irqs = n;
        self.spis = alloc::vec![Irq::new(); (n - 32) as usize];
    }

    pub fn take_kicks(&mut self) -> KickMask {
        core::mem::take(&mut self.kicks)
    }
    /// Level interrupts whose guest EOI was seen: the line source must be re-sampled.
    pub fn take_resample(&mut self) -> Vec<u32> {
        core::mem::take(&mut self.resample)
    }

    pub(crate) fn irq(&self, vcpu: usize, intid: u32) -> &Irq {
        if intid < NR_PRIV {
            &self.cpus[vcpu].priv_irqs[intid as usize]
        } else {
            &self.spis[(intid - NR_PRIV) as usize]
        }
    }
    pub(crate) fn irq_mut(&mut self, vcpu: usize, intid: u32) -> &mut Irq {
        if intid < NR_PRIV {
            &mut self.cpus[vcpu].priv_irqs[intid as usize]
        } else {
            &mut self.spis[(intid - NR_PRIV) as usize]
        }
    }

    /// vCPU an SPI is routed to, if any.
    pub fn target_vcpu(&self, intid: u32) -> Option<usize> {
        let i = &self.spis[(intid - NR_PRIV) as usize];
        match self.version {
            Version::V2 => {
                let m = i.target & ((1u16 << self.nr_vcpus.min(8)) - 1) as u8;
                if m == 0 { None } else { Some(m.trailing_zeros() as usize) }
            }
            Version::V3 => {
                let idx = if i.route & (1 << 31) != 0 {
                    0
                } else {
                    ((i.route & 0xff) | ((i.route >> 8 & 0xff) << 8)) as usize
                };
                if idx < self.nr_vcpus { Some(idx) } else { None }
            }
        }
    }

    fn belongs(&self, vcpu: usize, intid: u32) -> bool {
        intid < NR_PRIV || self.target_vcpu(intid) == Some(vcpu)
    }

    /// Pending as the guest would see it (latch, level line, v2 SGI sources).
    pub fn is_pending(&self, vcpu: usize, intid: u32) -> bool {
        if self.version == Version::V2 && intid < NR_SGI {
            return self.cpus[vcpu].sgi_src[intid as usize] != 0;
        }
        let i = self.irq(vcpu, intid);
        i.latch || (!i.edge && i.line)
    }

    fn deliverable(&self, vcpu: usize, intid: u32) -> bool {
        self.dist_enabled && self.irq(vcpu, intid).enabled && self.is_pending(vcpu, intid)
    }

    /// Anything for this vCPU to take (wake-up condition / HCR.VI equivalent).
    pub fn pending(&self, vcpu: usize) -> bool {
        if !self.dist_enabled || vcpu >= self.nr_vcpus {
            return false;
        }
        (0..self.nr_irqs).any(|id| self.belongs(vcpu, id) && self.deliverable(vcpu, id))
    }

    pub(crate) fn kick_one(&mut self, vcpu: usize) {
        if self.pending(vcpu) {
            self.kicks |= 1 << vcpu;
        }
    }
    pub(crate) fn kick_all(&mut self) {
        for v in 0..self.nr_vcpus {
            self.kick_one(v);
        }
    }

    // ---- KVM_IRQ_LINE ----------------------------------------------------------------------

    /// Drive an input line. intid 16..31 are PPIs of `vcpu`, >= 32 SPIs (vcpu ignored).
    pub fn set_line(&mut self, intid: u32, vcpu: usize, level: bool) -> KickMask {
        if intid < NR_SGI || intid >= self.nr_irqs || vcpu >= self.nr_vcpus {
            return 0;
        }
        let i = self.irq_mut(vcpu, intid);
        let rising = level && !i.line;
        i.line = level;
        if i.edge && rising {
            i.latch = true;
        }
        let target = if intid < NR_PRIV { Some(vcpu) } else { self.target_vcpu(intid) };
        match target {
            Some(t) if self.deliverable(t, intid) => 1 << t,
            _ => 0,
        }
    }

    /// KVM_IRQ_LINE: `irq` = type<<24 | vcpu<<16 | id. Type CPU (0) is the caller's (HCR.VI/VF).
    pub fn irq_line(&mut self, irq: u32, level: u32) -> Result<KickMask, i32> {
        let (ty, vcpu, id) = abi_shim::irq_line_decode(irq);
        match ty {
            abi_shim::IRQ_TYPE_SPI => {
                // Linux: userspace passes the INTID itself, and an SPI below 32 is EINVAL.
                let intid = id;
                if intid < NR_PRIV || intid >= self.nr_irqs {
                    return Err(abi_shim::EINVAL);
                }
                Ok(self.set_line(intid, 0, level != 0))
            }
            abi_shim::IRQ_TYPE_PPI => {
                if vcpu >= self.nr_vcpus || !(NR_SGI..NR_PRIV).contains(&id) {
                    return Err(abi_shim::EINVAL);
                }
                Ok(self.set_line(id, vcpu, level != 0))
            }
            _ => Err(abi_shim::EINVAL),
        }
    }

    // ---- SGIs -----------------------------------------------------------------------------

    fn raise_sgi(&mut self, from: usize, to: usize, sgi: u32) {
        if to >= self.nr_vcpus {
            return;
        }
        match self.version {
            Version::V2 => self.cpus[to].sgi_src[sgi as usize] |= 1 << (from & 7),
            Version::V3 => self.cpus[to].priv_irqs[sgi as usize].latch = true,
        }
    }

    /// v3: ICC_SGI1R_EL1 value; v2: GICD_SGIR value.
    pub fn inject_sgi(&mut self, from: usize, val: u64) -> KickMask {
        let mut targets: u64 = 0;
        let sgi;
        match self.version {
            Version::V3 => {
                sgi = (val >> 24 & 0xf) as u32;
                if val & (1 << 40) != 0 {
                    targets = (((1u128 << self.nr_vcpus) - 1) as u64) & !(1 << from);
                } else {
                    let list = val & 0xffff;
                    let rs = (val >> 44 & 0xf) as usize;
                    let aff1 = (val >> 16 & 0xff) as usize;
                    for b in 0..16 {
                        if list >> b & 1 != 0 {
                            let idx = aff1 * 256 + rs * 16 + b;
                            if idx < self.nr_vcpus {
                                targets |= 1 << idx;
                            }
                        }
                    }
                }
            }
            Version::V2 => {
                sgi = (val & 0xf) as u32;
                let all = ((1u16 << self.nr_vcpus) - 1) as u64;
                targets = match val >> 24 & 3 {
                    0 => (val >> 16 & 0xff) & all,
                    1 => all & !(1 << from),
                    2 => 1 << from,
                    _ => 0,
                };
            }
        }
        for t in 0..self.nr_vcpus {
            if targets >> t & 1 != 0 {
                self.raise_sgi(from, t, sgi);
                self.kick_one(t);
            }
        }
        self.take_kicks()
    }

    // ---- LR flush / fold ------------------------------------------------------------------

    /// Load pending interrupts into `lrs` (one slot per hardware LR). Returns the ICH_HCR/GICH_HCR value.
    pub fn flush(&mut self, vcpu: usize, lrs: &mut [u64], codec: &dyn LrCodec) -> u64 {
        let nr_lr = lrs.len();
        self.cpus[vcpu].lr_owner.resize(nr_lr, None);
        struct Cand {
            e: LrEntry,
            level: bool,
            /// Active level interrupt whose line is still high (the guest is in its handler and
            /// the run returned to userspace): listed active-only with the EOI bit, so the
            /// guest's EOI empties the LR and raises the maintenance interrupt that resamples
            /// the line. A pending+active LR would instead be re-delivered at once.
            relist: bool,
        }
        let mut cands: Vec<Cand> = Vec::new();
        if self.dist_enabled {
            for intid in 0..self.nr_irqs {
                if !self.belongs(vcpu, intid) {
                    continue;
                }
                let i = *self.irq(vcpu, intid);
                let pend = i.enabled && self.is_pending(vcpu, intid);
                if !pend && !i.active {
                    continue;
                }
                let v2sgi = self.version == Version::V2 && intid < NR_SGI;
                let srcs: u8 = if v2sgi && pend { self.cpus[vcpu].sgi_src[intid as usize] } else { 0 };
                let srcs_list: Vec<u8> = if v2sgi && pend {
                    (0..8u8).filter(|s| srcs >> s & 1 != 0).collect()
                } else {
                    alloc::vec![0]
                };
                for (n, &s) in srcs_list.iter().enumerate() {
                    let already = self.cpus[vcpu].lr_owner.iter().flatten().any(|o| o.intid == intid && o.src == s);
                    if already {
                        continue;
                    }
                    cands.push(Cand {
                        e: LrEntry {
                            intid,
                            src: s,
                            prio: i.prio,
                            group1: i.group1,
                            pending: pend && !(i.active && !i.edge && intid >= NR_SGI),
                            active: i.active && n == 0,
                            eoi: false,
                        },
                        level: !i.edge && intid >= NR_SGI,
                        relist: pend && i.active && !i.edge && intid >= NR_SGI,
                    });
                }
            }
        }
        cands.sort_by_key(|c| (c.e.prio, c.e.intid, c.e.src));
        let mut next = 0usize;
        let mut slot = 0usize;
        while slot < nr_lr && next < cands.len() {
            if self.cpus[vcpu].lr_owner[slot].is_none() {
                let c = &mut cands[next];
                c.e.eoi = c.level && ((c.e.pending && !c.e.active) || c.relist);
                lrs[slot] = codec.encode(&c.e);
                self.cpus[vcpu].lr_owner[slot] =
                    Some(LrOwner { intid: c.e.intid, src: c.e.src, act: c.e.active, eoi: c.e.eoi });
                if c.e.pending {
                    if self.version == Version::V2 && c.e.intid < NR_SGI {
                        self.cpus[vcpu].sgi_src[c.e.intid as usize] &= !(1 << c.e.src);
                    } else {
                        self.irq_mut(vcpu, c.e.intid).latch = false;
                    }
                }
                next += 1;
            }
            slot += 1;
        }
        for s in 0..nr_lr {
            if self.cpus[vcpu].lr_owner[s].is_none() {
                lrs[s] = 0;
            }
        }
        HCR_EN | if next < cands.len() { HCR_UIE } else { 0 }
    }

    /// Read back the LRs after a guest run: un-list everything and write the guest's progress to the irq state.
    pub fn fold(&mut self, vcpu: usize, lrs: &[u64], elrsr: u64, codec: &dyn LrCodec) {
        let n = lrs.len().min(self.cpus[vcpu].lr_owner.len());
        for slot in 0..n {
            let Some(o) = self.cpus[vcpu].lr_owner[slot].take() else { continue };
            let empty = elrsr >> slot & 1 == 1;
            let (pend, act) = if empty {
                (false, false)
            } else {
                let e = codec.decode(lrs[slot]);
                (e.pending, e.active)
            };
            let v2sgi = self.version == Version::V2 && o.intid < NR_SGI;
            if v2sgi {
                if pend {
                    self.cpus[vcpu].sgi_src[o.intid as usize] |= 1 << o.src;
                }
            }
            let i = self.irq_mut(vcpu, o.intid);
            if !v2sgi && pend && i.edge {
                i.latch = true;
            }
            i.active = if o.act { act } else { i.active || act };
            // An LR carrying the EOI bit stays non-empty in ELRSR until the maintenance interrupt
            // is serviced, so the guest's EOI is recognised by its state going invalid.
            if !pend && !act && o.eoi && !i.edge {
                self.resample.push(o.intid);
            }
        }
    }

    /// Decode MISR after a maintenance interrupt; the caller then folds and re-flushes.
    pub fn maintenance(&mut self, _vcpu: usize, misr: u64) -> Maint {
        Maint { eoi: misr & MISR_EOI != 0, underflow: misr & MISR_U != 0 }
    }

    /// flush into hardware: LRs and HCR.
    pub fn sync_in(&mut self, vcpu: usize, hw: &mut dyn VgicHw, codec: &dyn LrCodec) {
        let mut lrs = alloc::vec![0u64; hw.nr_lr()];
        let hcr = self.flush(vcpu, &mut lrs, codec);
        for (i, v) in lrs.iter().enumerate() {
            hw.set_lr(i, *v);
        }
        hw.set_hcr(hcr);
    }

    /// fold from hardware, clearing the LRs.
    pub fn sync_out(&mut self, vcpu: usize, hw: &mut dyn VgicHw, codec: &dyn LrCodec) {
        let lrs: Vec<u64> = (0..hw.nr_lr()).map(|i| hw.lr(i)).collect();
        let elrsr = hw.elrsr();
        self.fold(vcpu, &lrs, elrsr, codec);
        for i in 0..lrs.len() {
            hw.set_lr(i, 0);
        }
        hw.set_hcr(0);
    }
}
