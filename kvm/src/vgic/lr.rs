//! List-register codecs, the hardware trait and a mock implementation for host tests.
use alloc::vec::Vec;

/// One decoded list register.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct LrEntry {
    pub intid: u32,
    /// v2 SGI source CPU (CPUID field); 0 otherwise.
    pub src: u8,
    /// Full 8-bit priority (v2 keeps only the top 5 bits).
    pub prio: u8,
    pub group1: bool,
    pub pending: bool,
    pub active: bool,
    /// Maintenance interrupt on EOI (level interrupts).
    pub eoi: bool,
}

pub trait LrCodec {
    fn encode(&self, e: &LrEntry) -> u64;
    fn decode(&self, raw: u64) -> LrEntry;
}

/// ICH_LR<n>_EL2: vINTID 31:0, EOI 41, prio 55:48, G1 60, HW 61, state 63:62 (63 pending, 62 active... see below).
/// Architecturally State = bits 63:62, bit 62 = pending, bit 63 = active.
pub struct V3Codec;
impl LrCodec for V3Codec {
    fn encode(&self, e: &LrEntry) -> u64 {
        let mut v = e.intid as u64;
        if e.eoi {
            v |= 1 << 41;
        }
        v |= (e.prio as u64) << 48;
        if e.group1 {
            v |= 1 << 60;
        }
        if e.pending {
            v |= 1 << 62;
        }
        if e.active {
            v |= 1 << 63;
        }
        v
    }
    fn decode(&self, raw: u64) -> LrEntry {
        LrEntry {
            intid: raw as u32,
            src: 0,
            prio: (raw >> 48) as u8,
            group1: raw >> 60 & 1 != 0,
            pending: raw >> 62 & 1 != 0,
            active: raw >> 63 & 1 != 0,
            eoi: raw >> 41 & 1 != 0,
        }
    }
}

/// GICH_LR<n>: VirtualID 9:0, CPUID 12:10, EOI 19, prio 27:23, state 29:28 (28 pending, 29 active), G1 30, HW 31.
pub struct V2Codec;
impl LrCodec for V2Codec {
    fn encode(&self, e: &LrEntry) -> u64 {
        let mut v = (e.intid as u64) & 0x3ff;
        v |= ((e.src as u64) & 7) << 10;
        if e.eoi {
            v |= 1 << 19;
        }
        v |= ((e.prio as u64) >> 3) << 23;
        if e.pending {
            v |= 1 << 28;
        }
        if e.active {
            v |= 1 << 29;
        }
        if e.group1 {
            v |= 1 << 30;
        }
        v
    }
    fn decode(&self, raw: u64) -> LrEntry {
        LrEntry {
            intid: (raw & 0x3ff) as u32,
            src: (raw >> 10 & 7) as u8,
            prio: ((raw >> 23 & 0x1f) as u8) << 3,
            group1: raw >> 30 & 1 != 0,
            pending: raw >> 28 & 1 != 0,
            active: raw >> 29 & 1 != 0,
            eoi: raw >> 19 & 1 != 0,
        }
    }
}

pub const HCR_EN: u64 = 1;
pub const HCR_UIE: u64 = 1 << 1;
pub const MISR_EOI: u64 = 1;
pub const MISR_U: u64 = 1 << 1;

/// What the EL2 half (kvm-vhe/kvm-switch) must provide: ICH_*_EL2 or GICH_* access.
pub trait VgicHw {
    fn nr_lr(&self) -> usize;
    fn lr(&self, i: usize) -> u64;
    fn set_lr(&mut self, i: usize, v: u64);
    /// Bit i set = LR i is empty.
    fn elrsr(&self) -> u64;
    fn misr(&self) -> u64;
    fn hcr(&self) -> u64;
    fn set_hcr(&mut self, v: u64);
}

/// Mock hardware plus a tiny guest CPU interface (ack/EOI with priority drop and PMR).
pub struct MockHw<C: LrCodec> {
    pub codec: C,
    pub lrs: Vec<u64>,
    pub hcr: u64,
    pub pmr: u8,
    /// Running-priority stack (priority of each active interrupt acked by the mock guest).
    pub running: Vec<u8>,
    pub eoi_seen: bool,
}

impl<C: LrCodec> MockHw<C> {
    pub fn new(codec: C, nr_lr: usize) -> Self {
        Self { codec, lrs: alloc::vec![0; nr_lr], hcr: 0, pmr: 0xff, running: Vec::new(), eoi_seen: false }
    }
    fn running_prio(&self) -> u8 {
        self.running.iter().copied().min().unwrap_or(0xff)
    }
    /// Guest reads IAR: highest-priority pending LR below PMR and running priority.
    pub fn ack(&mut self) -> Option<(u32, u8)> {
        if self.hcr & HCR_EN == 0 {
            return None;
        }
        let rp = self.running_prio();
        let mut best: Option<(usize, LrEntry)> = None;
        for (i, &raw) in self.lrs.iter().enumerate() {
            let e = self.codec.decode(raw);
            if !e.pending || e.prio >= self.pmr || e.prio >= rp {
                continue;
            }
            if best.map_or(true, |(_, b)| e.prio < b.prio) {
                best = Some((i, e));
            }
        }
        let (i, mut e) = best?;
        e.pending = false;
        e.active = true;
        self.lrs[i] = self.codec.encode(&e);
        self.running.push(e.prio);
        Some((e.intid, e.src))
    }
    /// Guest EOI (EOImode 0: priority drop plus deactivate).
    pub fn eoi(&mut self, intid: u32) -> bool {
        for i in 0..self.lrs.len() {
            let mut e = self.codec.decode(self.lrs[i]);
            if e.intid == intid && e.active {
                if let Some(p) = self.running.iter().rposition(|&p| p == e.prio) {
                    self.running.remove(p);
                }
                e.active = false;
                if e.eoi {
                    self.eoi_seen = true;
                }
                self.lrs[i] = if e.pending { self.codec.encode(&e) } else { 0 };
                return true;
            }
        }
        false
    }
    fn valid(&self) -> usize {
        self.lrs.iter().filter(|&&r| { let e = self.codec.decode(r); e.pending || e.active }).count()
    }
    /// True when the maintenance interrupt line would be asserted.
    pub fn maintenance_asserted(&self) -> bool {
        self.misr() != 0
    }
}

impl<C: LrCodec> VgicHw for MockHw<C> {
    fn nr_lr(&self) -> usize { self.lrs.len() }
    fn lr(&self, i: usize) -> u64 { self.lrs[i] }
    fn set_lr(&mut self, i: usize, v: u64) { self.lrs[i] = v }
    fn elrsr(&self) -> u64 {
        let mut m = 0;
        for (i, &r) in self.lrs.iter().enumerate() {
            let e = self.codec.decode(r);
            if !e.pending && !e.active {
                m |= 1 << i;
            }
        }
        m
    }
    fn misr(&self) -> u64 {
        let mut m = 0;
        if self.hcr & HCR_EN == 0 {
            return 0;
        }
        if self.eoi_seen {
            m |= MISR_EOI;
        }
        if self.hcr & HCR_UIE != 0 && self.valid() <= 1 {
            m |= MISR_U;
        }
        m
    }
    fn hcr(&self) -> u64 { self.hcr }
    fn set_hcr(&mut self, v: u64) { self.hcr = v }
}
