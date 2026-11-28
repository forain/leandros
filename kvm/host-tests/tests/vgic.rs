use kvm::vgic::abi_shim::*;
use kvm::vgic::lr::*;
use kvm::vgic::*;

const DIST: u64 = 0x0800_0000;
const REDIST: u64 = 0x080a_0000;
const GICV2_DIST: u64 = 0x0800_0000;
const GICV2_CPU: u64 = 0x0801_0000;

fn v3(nr_spis: u32, vcpus: usize) -> Vgic {
    let mut g = Vgic::new(Version::V3, nr_spis, vcpus);
    g.attr_set(GRP_ADDR, V3_ADDR_DIST, &mut { DIST }).unwrap();
    g.attr_set(GRP_ADDR, V3_ADDR_REDIST, &mut { REDIST }).unwrap();
    g.attr_set(GRP_CTRL, CTRL_INIT, &mut 0).unwrap();
    g
}
fn v2(nr_spis: u32, vcpus: usize) -> Vgic {
    let mut g = Vgic::new(Version::V2, nr_spis, vcpus);
    g.attr_set(GRP_ADDR, V2_ADDR_DIST, &mut { GICV2_DIST }).unwrap();
    g.attr_set(GRP_ADDR, V2_ADDR_CPU, &mut { GICV2_CPU }).unwrap();
    g.attr_set(GRP_CTRL, CTRL_INIT, &mut 0).unwrap();
    g
}
fn rd(g: &mut Vgic, vcpu: usize, gpa: u64, len: usize) -> u64 {
    let mut v = 0;
    assert!(g.mmio(vcpu, gpa, len, false, &mut v), "unhandled read {gpa:#x}");
    v
}
fn wr(g: &mut Vgic, vcpu: usize, gpa: u64, len: usize, val: u64) {
    let mut v = val;
    assert!(g.mmio(vcpu, gpa, len, true, &mut v), "unhandled write {gpa:#x}");
}
fn d(g: &mut Vgic, off: u64) -> u64 { rd(g, 0, DIST + off, 4) }
fn dw(g: &mut Vgic, off: u64, v: u64) { wr(g, 0, DIST + off, 4, v) }
fn rdw(g: &mut Vgic, _vcpu: u64, off: u64, v: u64) { wr(g, 0, REDIST + off, 4, v) }
fn sgi(vcpu: u64, off: u64) -> u64 { vcpu * 0x2_0000 + 0x1_0000 + off }

fn enable_all(g: &mut Vgic, v3: bool) {
    dw(g, 0, if v3 { 2 } else { 1 });
}

#[test]
fn v3_dist_identification_and_ctlr() {
    let mut g = v3(96, 2);
    assert_eq!(d(&mut g, 0x004) & 0x1f, 3); // 128 irqs
    assert_eq!(d(&mut g, 0x004) >> 19 & 0x1f, 9);
    assert_eq!(d(&mut g, 0x008) & 0xfff, 0x43b);
    assert_eq!(d(&mut g, 0xffe8) & 0xf0, 0x30);
    assert_eq!(d(&mut g, 0) & 2, 0);
    dw(&mut g, 0, 2);
    assert_eq!(d(&mut g, 0) & 2, 2);
    assert!(d(&mut g, 0) & (1 << 4) != 0); // ARE_NS
    assert!(!g.mmio(0, DIST + 0x1_0000, 4, false, &mut 0)); // outside
}

#[test]
fn v3_spi_enable_pend_active_prio_cfg() {
    let mut g = v3(96, 2);
    // ISENABLER1 irq 32..63
    dw(&mut g, 0x104, 0x0000_0005);
    assert_eq!(d(&mut g, 0x104), 5);
    dw(&mut g, 0x184, 1); // clear irq 32
    assert_eq!(d(&mut g, 0x184), 4);
    // byte write to ISENABLER must not clobber other bits
    wr(&mut g, 0, DIST + 0x105, 1, 0x01); // irq 40
    assert_eq!(d(&mut g, 0x104), 0x104);
    // priority bytes
    wr(&mut g, 0, DIST + 0x420, 1, 0x80);
    wr(&mut g, 0, DIST + 0x421, 1, 0x40);
    assert_eq!(d(&mut g, 0x420), 0x4080);
    wr(&mut g, 0, DIST + 0x422, 2, 0xa0b0);
    assert_eq!(d(&mut g, 0x420), 0xa0b0_4080);
    // cfg: irq 33 edge
    dw(&mut g, 0xc08, 0b1000);
    assert_eq!(d(&mut g, 0xc08), 0b1000);
    // group
    dw(&mut g, 0x084, 0xf0);
    assert_eq!(d(&mut g, 0x084), 0xf0);
    // pend set/clear and active set/clear
    dw(&mut g, 0x204, 0x6);
    assert_eq!(d(&mut g, 0x204), 0x6);
    dw(&mut g, 0x284, 0x2);
    assert_eq!(d(&mut g, 0x204), 0x4);
    dw(&mut g, 0x304, 0x10);
    assert_eq!(d(&mut g, 0x304), 0x10);
    dw(&mut g, 0x384, 0x10);
    assert_eq!(d(&mut g, 0x304), 0);
    // beyond nr_irqs (128) is RAZ/WI
    dw(&mut g, 0x110, 0xffff_ffff);
    assert_eq!(d(&mut g, 0x110), 0);
    // words 0 in the distributor belong to the redistributor
    dw(&mut g, 0x100, 0xffff);
    assert_eq!(d(&mut g, 0x100), 0);
}

#[test]
fn v3_irouter_64_and_32() {
    let mut g = v3(64, 2);
    assert_eq!(rd(&mut g, 0, DIST + 0x6100, 8), 0);
    wr(&mut g, 0, DIST + 0x6100, 8, 1); // irq 32 -> vcpu 1
    assert_eq!(rd(&mut g, 0, DIST + 0x6100, 8), 1);
    assert_eq!(g.target_vcpu(32), Some(1));
    wr(&mut g, 0, DIST + 0x6108, 4, 0);
    assert_eq!(g.target_vcpu(33), Some(0));
    wr(&mut g, 0, DIST + 0x6108, 4, 7); // nonexistent cpu
    assert_eq!(g.target_vcpu(33), None);
    wr(&mut g, 0, DIST + 0x6108, 8, 1 << 31); // IRM
    assert_eq!(g.target_vcpu(33), Some(0));
    assert_eq!(rd(&mut g, 0, DIST + 0x6104, 4), 0);
}

#[test]
fn v3_redist_frames() {
    let mut g = v3(32, 3);
    for v in 0..3u64 {
        let t = rd(&mut g, 0, REDIST + v * 0x2_0000 + 8, 8);
        assert_eq!(t >> 32, v);
        assert_eq!(t >> 8 & 0xffff, v);
        assert_eq!(t >> 4 & 1, (v == 2) as u64);
    }
    assert_eq!(rd(&mut g, 0, REDIST + 0x14, 4), 0); // WAKER awake
    assert_eq!(rd(&mut g, 0, REDIST + 0xffe8, 4), 0x3b);
    // SGIs are enabled at reset, PPIs not
    assert_eq!(rd(&mut g, 0, REDIST + sgi(1, 0x100), 4), 0xffff);
    wr(&mut g, 0, REDIST + sgi(1, 0x100), 4, 1 << 27);
    assert_eq!(rd(&mut g, 0, REDIST + sgi(1, 0x100), 4), 0x0800_ffff);
    assert_eq!(rd(&mut g, 0, REDIST + sgi(0, 0x100), 4), 0xffff); // per vcpu
    wr(&mut g, 1, REDIST + sgi(1, 0x41b), 1, 0x20); // prio of irq 27
    assert_eq!(rd(&mut g, 0, REDIST + sgi(1, 0x418), 4), 0x2000_0000);
    // SGI ICFGR0 is read-only edge, PPI configurable
    wr(&mut g, 0, REDIST + sgi(0, 0xc00), 4, 0);
    assert_eq!(rd(&mut g, 0, REDIST + sgi(0, 0xc00), 4), 0xaaaa_aaaa);
    // out of region
    assert!(!g.mmio(0, REDIST + 3 * 0x2_0000, 4, false, &mut 0));
}

fn run_hw(g: &mut Vgic, vcpu: usize, hw: &mut MockHw<V3Codec>) {
    g.sync_in(vcpu, hw, &V3Codec);
}

#[test]
fn edge_spi_lifecycle() {
    let mut g = v3(32, 1);
    enable_all(&mut g, true);
    dw(&mut g, 0x104, 1); // enable irq 32
    dw(&mut g, 0xc08, 0); // irq 32 level default... set edge below
    dw(&mut g, 0xc08, 0b10);
    wr(&mut g, 0, DIST + 0x420, 1, 0x40);
    let mut hw = MockHw::new(V3Codec, 4);
    assert!(!g.pending(0));
    assert_eq!(g.irq_line(abi_shim::irq_line_decode(0).0 << 24 | 0, 1), Err(EINVAL)); // type CPU rejected
    let k = g.irq_line(1 << 24 | 32, 1).unwrap(); // KVM_IRQ_LINE takes the INTID itself (Linux: SPI < 32 is EINVAL)
    assert_eq!(k, 1);
    assert!(g.pending(0));
    run_hw(&mut g, 0, &mut hw);
    assert_eq!(hw.hcr, HCR_EN);
    let e = V3Codec.decode(hw.lrs[0]);
    assert_eq!((e.intid, e.prio, e.pending, e.active, e.eoi), (32, 0x40, true, false, false));
    assert!(!g.pending(0)); // latch consumed into LR
    assert_eq!(hw.ack(), Some((32, 0)));
    g.sync_out(0, &mut hw, &V3Codec);
    assert_eq!(d(&mut g, 0x304), 1); // active
    assert!(!g.pending(0));
    // second edge while active: pending+active
    g.irq_line(1 << 24 | 32, 0).unwrap();
    g.irq_line(1 << 24 | 32, 1).unwrap();
    assert!(g.pending(0));
    run_hw(&mut g, 0, &mut hw);
    let e = V3Codec.decode(hw.lrs[0]);
    assert!(e.pending && e.active);
    assert!(hw.eoi(32));
    g.sync_out(0, &mut hw, &V3Codec);
    assert_eq!(d(&mut g, 0x304), 0);
    assert!(g.pending(0)); // pending kept
    run_hw(&mut g, 0, &mut hw);
    assert_eq!(hw.ack(), Some((32, 0)));
    hw.eoi(32);
    g.sync_out(0, &mut hw, &V3Codec);
    assert!(!g.pending(0));
}

#[test]
fn level_spi_lifecycle_and_resample() {
    let mut g = v3(32, 1);
    enable_all(&mut g, true);
    dw(&mut g, 0x104, 1);
    let mut hw = MockHw::new(V3Codec, 4);
    g.set_line(32, 0, true);
    run_hw(&mut g, 0, &mut hw);
    let e = V3Codec.decode(hw.lrs[0]);
    assert!(e.pending && e.eoi);
    assert_eq!(hw.ack(), Some((32, 0)));
    // guest EOIs with the line still high: maintenance EOI, interrupt fires again
    hw.eoi(32);
    assert!(hw.maintenance_asserted());
    let m = g.maintenance(0, hw.misr());
    assert!(m.eoi && !m.underflow);
    g.sync_out(0, &mut hw, &V3Codec);
    assert_eq!(g.take_resample(), vec![32]);
    assert!(g.pending(0)); // line still high
    hw.eoi_seen = false;
    run_hw(&mut g, 0, &mut hw);
    assert_eq!(hw.ack(), Some((32, 0)));
    g.set_line(32, 0, false);
    hw.eoi(32);
    g.sync_out(0, &mut hw, &V3Codec);
    assert!(!g.pending(0));
    // line dropping before ack removes the interrupt
    g.set_line(32, 0, true);
    run_hw(&mut g, 0, &mut hw);
    g.set_line(32, 0, false);
    g.sync_out(0, &mut hw, &V3Codec); // never acked: pending state returns to the distributor
    assert!(!g.pending(0));
}

#[test]
fn priority_ordering_and_pmr() {
    let mut g = v3(32, 1);
    enable_all(&mut g, true);
    dw(&mut g, 0x104, 0b111);
    wr(&mut g, 0, DIST + 0x420, 1, 0x60);
    wr(&mut g, 0, DIST + 0x421, 1, 0x20);
    wr(&mut g, 0, DIST + 0x422, 1, 0x40);
    dw(&mut g, 0xc08, 0xa); // edge for 32, 33 and 34? bits: irq32 [1:0]=2, irq33 [3:2]=2
    dw(&mut g, 0xc08, 0b101010);
    for n in 32..35 {
        g.set_line(n, 0, true);
    }
    let mut hw = MockHw::new(V3Codec, 4);
    run_hw(&mut g, 0, &mut hw);
    assert_eq!(hw.ack(), Some((33, 0)));
    // lower-priority ones cannot preempt-ack while 33 is running? 0x40 and 0x60 are lower, so blocked
    assert_eq!(hw.ack(), None);
    hw.eoi(33);
    assert_eq!(hw.ack(), Some((34, 0)));
    hw.pmr = 0x50; // 32 (0x60) masked
    hw.eoi(34);
    assert_eq!(hw.ack(), None);
    hw.pmr = 0xff;
    assert_eq!(hw.ack(), Some((32, 0)));
    // equal priority sorts by intid
    let mut g = v3(32, 1);
    enable_all(&mut g, true);
    dw(&mut g, 0x104, 0b11);
    dw(&mut g, 0xc08, 0b1010);
    g.set_line(33, 0, true);
    g.set_line(32, 0, true);
    let mut lrs = [0u64; 2];
    g.flush(0, &mut lrs, &V3Codec);
    assert_eq!(V3Codec.decode(lrs[0]).intid, 32);
    assert_eq!(V3Codec.decode(lrs[1]).intid, 33);
}

#[test]
fn lr_overflow_underflow_maintenance() {
    let mut g = v3(32, 1);
    enable_all(&mut g, true);
    dw(&mut g, 0x104, 0xff);
    dw(&mut g, 0xc08, 0xaaaa); // 8 edge irqs
    for n in 0..8u32 {
        wr(&mut g, 0, DIST + 0x420 + n as u64, 1, (n as u64 + 1) * 0x10);
        g.set_line(32 + n, 0, true);
    }
    let mut hw = MockHw::new(V3Codec, 4);
    run_hw(&mut g, 0, &mut hw);
    assert_eq!(hw.hcr, HCR_EN | HCR_UIE);
    let ids: Vec<u32> = hw.lrs.iter().map(|&r| V3Codec.decode(r).intid).collect();
    assert_eq!(ids, vec![32, 33, 34, 35]); // highest priority first
    assert!(g.pending(0)); // 4 still queued in the distributor
    assert!(hw.maintenance_asserted() == false); // 4 valid LRs
    for id in [32, 33, 34] {
        assert!(hw.ack().is_some());
        hw.eoi(id);
    }
    assert!(hw.maintenance_asserted()); // <= 1 valid: underflow
    let m = g.maintenance(0, hw.misr());
    assert!(m.underflow);
    g.sync_out(0, &mut hw, &V3Codec);
    run_hw(&mut g, 0, &mut hw);
    assert_eq!(hw.hcr, HCR_EN | HCR_UIE | 0 & 0 | 0 | 0); // 35 relisted plus 36..38 -> 4 LRs, one left
    let mut seen = vec![];
    while let Some((id, _)) = hw.ack() {
        seen.push(id);
        hw.eoi(id);
        break;
    }
    assert_eq!(seen, vec![35]);
}

#[test]
fn sgi_v3_routing() {
    let mut g = v3(32, 4);
    enable_all(&mut g, true);
    // SGI 5 to cpus 1 and 3 (Aff0 target list 0b1010), Aff1 = 0
    let k = g.inject_sgi(0, 0b1010 | 5 << 24);
    assert_eq!(k, 0b1010);
    assert!(g.pending(1) && g.pending(3) && !g.pending(0) && !g.pending(2));
    let mut hw = MockHw::new(V3Codec, 4);
    g.sync_in(1, &mut hw, &V3Codec);
    assert_eq!(hw.ack(), Some((5, 0)));
    // IRM: everyone but self
    let k = g.inject_sgi(2, 1 << 40 | 7 << 24);
    assert_eq!(k, 0b1011);
    // disabled SGI does not kick
    let mut g = v3(32, 4);
    enable_all(&mut g, true);
    rdw(&mut g, 0, sgi(0, 0x180), 1 << 9);
    assert_eq!(g.inject_sgi(1, 1 | 9 << 24), 0);
    // range selector: RS=1 targets cpu 16+; none exist
    assert_eq!(g.inject_sgi(0, 1 | 1 << 44 | 3 << 24), 0);
}

#[test]
fn ppi_line_and_irq_line_validation() {
    let mut g = v3(32, 2);
    enable_all(&mut g, true);
    rdw(&mut g, 1, sgi(1, 0x100), 1 << 27);
    let k = g.irq_line(2 << 24 | 1 << 16 | 27, 1).unwrap();
    assert_eq!(k, 0b10);
    assert!(g.pending(1) && !g.pending(0));
    g.irq_line(2 << 24 | 1 << 16 | 27, 0).unwrap();
    assert!(!g.pending(1));
    assert_eq!(g.irq_line(2 << 24 | 5 << 16 | 27, 1), Err(EINVAL)); // no vcpu 5
    assert_eq!(g.irq_line(2 << 24 | 40, 1), Err(EINVAL));
    assert_eq!(g.irq_line(1 << 24 | 500, 1), Err(EINVAL)); // out of range spi
    assert_eq!(g.irq_line(1 << 24 | 31, 1), Err(EINVAL)); // not an SPI
    assert_eq!(g.irq_line(1 << 24 | 63, 1), Ok(0)); // SPI not enabled: no kick
}

#[test]
fn disabled_distributor_and_pend_clear() {
    let mut g = v3(32, 1);
    dw(&mut g, 0x104, 1);
    g.set_line(32, 0, true);
    assert!(!g.pending(0)); // dist disabled
    let mut lrs = [0u64; 2];
    g.flush(0, &mut lrs, &V3Codec);
    assert_eq!(lrs, [0, 0]);
    dw(&mut g, 0, 2);
    assert!(g.pending(0));
    // soft pend + clear on an edge irq
    dw(&mut g, 0xc08, 0b10);
    g.set_line(32, 0, false);
    dw(&mut g, 0x204, 1);
    assert_eq!(d(&mut g, 0x204) & 1, 1);
    dw(&mut g, 0x284, 1);
    assert_eq!(d(&mut g, 0x204) & 1, 0);
    assert!(!g.pending(0));
}

#[test]
fn group_in_lr_and_active_listing() {
    let mut g = v3(32, 1);
    enable_all(&mut g, true);
    dw(&mut g, 0x104, 3);
    dw(&mut g, 0x084, 1); // irq 32 group1
    dw(&mut g, 0x304, 2); // irq 33 active only
    dw(&mut g, 0x204, 1);
    let mut lrs = [0u64; 4];
    g.flush(0, &mut lrs, &V3Codec);
    let a = V3Codec.decode(lrs[0]);
    let b = V3Codec.decode(lrs[1]);
    assert!(a.intid == 32 && a.group1 && a.pending && !a.active);
    assert!(b.intid == 33 && !b.group1 && !b.pending && b.active);
}

#[test]
fn codecs_roundtrip() {
    let e = LrEntry { intid: 1019, src: 0, prio: 0xa8, group1: true, pending: true, active: true, eoi: true };
    assert_eq!(V3Codec.decode(V3Codec.encode(&e)), e);
    let raw = V3Codec.encode(&e);
    assert_eq!(raw & 0xffff_ffff, 1019);
    assert_eq!(raw >> 48 & 0xff, 0xa8);
    assert_eq!(raw >> 60 & 1, 1);
    assert_eq!(raw >> 62, 3);
    assert_eq!(raw >> 41 & 1, 1);
    let e2 = LrEntry { intid: 5, src: 3, prio: 0xa8, group1: true, pending: true, active: false, eoi: true };
    let r2 = V2Codec.encode(&e2);
    assert_eq!(r2 & 0x3ff, 5);
    assert_eq!(r2 >> 10 & 7, 3);
    assert_eq!(r2 >> 19 & 1, 1);
    assert_eq!(r2 >> 23 & 0x1f, 0x15);
    assert_eq!(r2 >> 28 & 3, 1);
    assert_eq!(r2 >> 30 & 1, 1);
    assert_eq!(V2Codec.decode(r2), e2);
    let e3 = LrEntry { pending: false, active: true, ..e2 };
    assert_eq!(V2Codec.decode(V2Codec.encode(&e3)).active, true);
    assert_eq!(V2Codec.encode(&e3) >> 28 & 3, 2);
}

// ---- GICv2 ------------------------------------------------------------------------------

fn d2(g: &mut Vgic, vcpu: usize, off: u64) -> u64 { rd(g, vcpu, GICV2_DIST + off, 4) }
fn d2w(g: &mut Vgic, vcpu: usize, off: u64, v: u64) { wr(g, vcpu, GICV2_DIST + off, 4, v) }

#[test]
fn v2_dist_registers() {
    let mut g = v2(96, 2);
    assert_eq!(d2(&mut g, 0, 4), (1 << 5) | 3);
    assert_eq!(d2(&mut g, 0, 0), 0);
    d2w(&mut g, 0, 0, 1);
    assert_eq!(d2(&mut g, 0, 0), 1);
    // banked words: each vcpu has its own private ISENABLER0
    d2w(&mut g, 1, 0x100, 1 << 27);
    assert_eq!(d2(&mut g, 1, 0x100), 0x0800_ffff);
    assert_eq!(d2(&mut g, 0, 0x100), 0xffff);
    // ITARGETSR: private = own bit, SPI byte writable
    assert_eq!(d2(&mut g, 1, 0x800), 0x0202_0202);
    wr(&mut g, 0, GICV2_DIST + 0x820, 1, 2);
    assert_eq!(d2(&mut g, 0, 0x820), 0x0101_0102);
    assert_eq!(g.target_vcpu(32), Some(1));
    wr(&mut g, 0, GICV2_DIST + 0x821, 1, 0b11);
    assert_eq!(g.target_vcpu(33), Some(0)); // lowest set bit
    wr(&mut g, 0, GICV2_DIST + 0x822, 1, 0b100);
    assert_eq!(g.target_vcpu(34), None);
    assert_eq!(d2(&mut g, 0, 0xfe8) & 0xf0, 0x20);
    // v2 cpu interface emulation fallback
    wr(&mut g, 0, GICV2_CPU + 4, 4, 0xf0);
    assert_eq!(rd(&mut g, 0, GICV2_CPU + 4, 4), 0xf0);
    assert_eq!(rd(&mut g, 1, GICV2_CPU + 4, 4), 0);
}

#[test]
fn v2_sgi_sources_and_lrs() {
    let mut g = v2(32, 3);
    d2w(&mut g, 0, 0, 1);
    // cpu0 and cpu1 both send SGI 3 to cpu2 (list filter)
    d2w(&mut g, 0, 0xf00, 0b100 << 16 | 3);
    d2w(&mut g, 1, 0xf00, 0b100 << 16 | 3);
    assert_eq!(g.take_kicks(), 0b100);
    assert_eq!(d2(&mut g, 2, 0xf10) >> 24, 0b11); // CPENDSGIR sources 0 and 1
    let mut hw = MockHw::new(V2Codec, 4);
    g.sync_in(2, &mut hw, &V2Codec);
    let mut srcs: Vec<u8> = hw.lrs.iter().filter(|&&r| r != 0).map(|&r| V2Codec.decode(r).src).collect();
    srcs.sort();
    assert_eq!(srcs, vec![0, 1]);
    assert_eq!(hw.ack().map(|x| x.0), Some(3));
    assert_eq!(hw.ack().map(|x| x.0), None); // same priority cannot preempt
    hw.eoi(3);
    assert_eq!(hw.ack().map(|x| x.0), Some(3));
    hw.eoi(3);
    g.sync_out(2, &mut hw, &V2Codec);
    assert_eq!(d2(&mut g, 2, 0xf10) >> 24, 0);
    // all-but-self filter and SPENDSGIR/CPENDSGIR writes
    d2w(&mut g, 1, 0xf00, 1 << 24 | 4);
    assert_eq!(g.take_kicks(), 0b101);
    d2w(&mut g, 0, 0xf20, 0x0200_0000); // set source 1 for SGI 3? byte 3 = SGI3
    assert_eq!(d2(&mut g, 0, 0xf10) >> 24, 2);
    d2w(&mut g, 0, 0xf10, 0x0200_0000);
    assert_eq!(d2(&mut g, 0, 0xf10) >> 24, 0);
    // self filter
    d2w(&mut g, 1, 0xf00, 2 << 24 | 9);
    assert!(g.pending(1));
}

#[test]
fn v2_lr_priority_five_bits() {
    let mut g = v2(32, 1);
    d2w(&mut g, 0, 0, 1);
    d2w(&mut g, 0, 0x104, 1);
    wr(&mut g, 0, GICV2_DIST + 0x420, 1, 0xa5);
    d2w(&mut g, 0, 0xc08, 0b10);
    g.set_line(32, 0, true);
    let mut lrs = [0u64; 4];
    g.flush(0, &mut lrs, &V2Codec);
    let e = V2Codec.decode(lrs[0]);
    assert_eq!((e.intid, e.prio), (32, 0xa0));
}

// ---- device attributes ------------------------------------------------------------------

#[test]
fn attr_addr_and_init_rules() {
    assert_eq!(Vgic::device_version(KVM_DEV_TYPE_ARM_VGIC_V3), Some(Version::V3));
    assert_eq!(Vgic::device_version(KVM_DEV_TYPE_ARM_VGIC_V2), Some(Version::V2));
    assert_eq!(Vgic::device_version(8), None);
    let mut g = Vgic::new(Version::V3, 64, 2);
    assert_eq!(g.attr_set(GRP_CTRL, CTRL_INIT, &mut 0), Err(ENXIO));
    assert_eq!(g.attr_get(GRP_ADDR, V3_ADDR_DIST, &mut 0), Err(ENXIO));
    assert_eq!(g.attr_set(GRP_ADDR, V3_ADDR_DIST, &mut 0x8000_1000), Err(EINVAL)); // not 64K aligned
    assert_eq!(g.attr_set(GRP_ADDR, V2_ADDR_DIST, &mut 0x8000_0000), Err(ENXIO)); // v2 attr on v3
    g.attr_set(GRP_ADDR, V3_ADDR_DIST, &mut { DIST }).unwrap();
    assert_eq!(g.attr_set(GRP_ADDR, V3_ADDR_DIST, &mut { DIST }), Err(EEXIST));
    let mut v = 0;
    g.attr_get(GRP_ADDR, V3_ADDR_DIST, &mut v).unwrap();
    assert_eq!(v, DIST);
    assert_eq!(g.attr_set(GRP_ADDR, V3_ADDR_REDIST, &mut { DIST + 0x8000 }), Err(EINVAL)); // overlap/alignment
    // region with too few redistributors fails at INIT
    g.attr_set(GRP_ADDR, V3_ADDR_REDIST_REGION, &mut { REDIST | 1 << 52 }).unwrap();
    assert_eq!(g.attr_set(GRP_CTRL, CTRL_INIT, &mut 0), Err(EINVAL));
    let mut g = Vgic::new(Version::V3, 64, 2);
    g.attr_set(GRP_ADDR, V3_ADDR_DIST, &mut { DIST }).unwrap();
    g.attr_set(GRP_ADDR, V3_ADDR_REDIST_REGION, &mut { REDIST | 2 << 52 }).unwrap();
    g.attr_get(GRP_ADDR, V3_ADDR_REDIST_REGION, &mut v).unwrap();
    assert_eq!(v, REDIST | 2 << 52);
    // NR_IRQS
    assert_eq!(g.attr_set(GRP_NR_IRQS, 0, &mut 100), Err(EINVAL));
    assert_eq!(g.attr_set(GRP_NR_IRQS, 0, &mut 32), Err(EINVAL));
    g.attr_set(GRP_NR_IRQS, 0, &mut 256).unwrap();
    g.attr_get(GRP_NR_IRQS, 0, &mut v).unwrap();
    assert_eq!(v, 256);
    // register groups need INIT first
    assert_eq!(g.attr_get(GRP_DIST_REGS, 4, &mut v), Err(EBUSY));
    g.attr_set(GRP_CTRL, CTRL_INIT, &mut 0).unwrap();
    g.attr_set(GRP_CTRL, CTRL_INIT, &mut 0).unwrap(); // idempotent
    assert_eq!(g.attr_set(GRP_NR_IRQS, 0, &mut 128), Err(EBUSY));
    assert_eq!(g.attr_set(GRP_ADDR, V3_ADDR_REDIST, &mut { 0 }), Err(EBUSY));
    g.attr_get(GRP_DIST_REGS, 4, &mut v).unwrap();
    assert_eq!(v & 0x1f, 7);
    // has_attr
    assert!(g.attr_has(GRP_ADDR, V3_ADDR_REDIST_REGION).is_ok());
    assert_eq!(g.attr_has(GRP_ADDR, V2_ADDR_CPU), Err(ENXIO));
    assert!(g.attr_has(GRP_CPU_SYSREGS, attr::ICC_PMR_EL1).is_ok());
    assert_eq!(g.attr_has(GRP_CPU_SYSREGS, attr::sysreg(3, 0, 1, 0, 0)), Err(ENXIO));
    assert_eq!(g.attr_has(GRP_CPU_REGS, 0), Err(ENXIO));
    assert_eq!(g.attr_has(99, 0), Err(ENXIO));
    assert!(g.attr_has(GRP_LEVEL_INFO, 0).is_ok());
}

#[test]
fn attr_v3_save_restore_roundtrip() {
    let mut a = v3(64, 2);
    enable_all(&mut a, true);
    dw(&mut a, 0x104, 0x1234);
    dw(&mut a, 0x204, 0x20); // soft pending irq 37
    dw(&mut a, 0x304, 0x4);
    wr(&mut a, 0, DIST + 0x424, 1, 0x70);
    dw(&mut a, 0xc08, 0x2);
    wr(&mut a, 0, DIST + 0x6100, 8, 1);
    rdw(&mut a, 1, sgi(1, 0x100), 1 << 30);
    a.set_line(33, 0, true);
    // SAVE
    let mut b = v3(64, 2);
    let mpidr = |v: u64| v << 32;
    for off in [0x0u64, 0x84, 0x104, 0x204, 0x304, 0x404, 0x424, 0xc04, 0xc08] {
        let mut v = 0;
        a.attr_get(GRP_DIST_REGS, off, &mut v).unwrap();
        b.attr_set(GRP_DIST_REGS, off, &mut v).unwrap();
    }
    let mut v = 0;
    a.attr_get(GRP_DIST_REGS, 0x6100, &mut v).unwrap();
    assert_eq!(v, 1);
    b.attr_set(GRP_DIST_REGS, 0x6100, &mut v).unwrap();
    let mut v = 0;
    a.attr_get(GRP_REDIST_REGS, mpidr(1) | 0x1_0100, &mut v).unwrap();
    b.attr_set(GRP_REDIST_REGS, mpidr(1) | 0x1_0100, &mut v).unwrap();
    assert_eq!(v, 0x4000_ffff);
    let mut v = 0;
    a.attr_get(GRP_LEVEL_INFO, mpidr(0) | 32, &mut v).unwrap();
    assert_eq!(v, 2);
    b.attr_set(GRP_LEVEL_INFO, mpidr(0) | 32, &mut v).unwrap();
    // uaccess pending view is the latch only: level line 33 is not in ISPENDR
    let mut p = 0;
    a.attr_get(GRP_DIST_REGS, 0x204, &mut p).unwrap();
    assert_eq!(p, 0x20);
    for off in [0x104u64, 0x204, 0x304, 0x404, 0x424, 0xc08] {
        let (mut x, mut y) = (0, 0);
        a.attr_get(GRP_DIST_REGS, off, &mut x).unwrap();
        b.attr_get(GRP_DIST_REGS, off, &mut y).unwrap();
        assert_eq!(x, y, "off {off:#x}");
    }
    assert!(b.pending(0) == a.pending(0));
    assert_eq!(b.target_vcpu(32), Some(1));
    let mut v = 0;
    assert_eq!(b.attr_get(GRP_REDIST_REGS, mpidr(9) | 0, &mut v), Err(EINVAL));
    // redist TYPER via attr is 64-bit
    b.attr_get(GRP_REDIST_REGS, mpidr(1) | 8, &mut v).unwrap();
    assert_eq!(v >> 32, 1);
    assert!(v & 0x10 != 0);
}

#[test]
fn attr_cpu_sysregs_and_v2_cpu_regs() {
    let mut g = v3(32, 2);
    use kvm::vgic::attr::*;
    for (r, val) in [(ICC_PMR_EL1, 0xf0u64), (ICC_BPR1_EL1, 3), (ICC_CTLR_EL1, 0x400), (ICC_IGRPEN1_EL1, 1),
        (ICC_AP1R0_EL1 + 2, 0xdead), (ICC_AP0R0_EL1 + 3, 0xbeef)] {
        let mut v = val;
        g.attr_set(GRP_CPU_SYSREGS, 1 << 32 | r, &mut v).unwrap();
        let mut o = 0;
        g.attr_get(GRP_CPU_SYSREGS, 1 << 32 | r, &mut o).unwrap();
        assert_eq!(o, val);
        g.attr_get(GRP_CPU_SYSREGS, r, &mut o).unwrap();
        assert_eq!(o, 0); // per vcpu
    }
    let mut o = 0;
    g.attr_get(GRP_CPU_SYSREGS, ICC_SRE_EL1, &mut o).unwrap();
    assert_eq!(o, 7);
    let mut g2 = v2(32, 2);
    g2.attr_set(GRP_CPU_REGS, 1 << 32 | 4, &mut 0x80).unwrap();
    let mut o = 0;
    g2.attr_get(GRP_CPU_REGS, 1 << 32 | 4, &mut o).unwrap();
    assert_eq!(o, 0x80);
    g2.attr_get(GRP_DIST_REGS, 1 << 32 | 0x800, &mut o).unwrap();
    assert_eq!(o, 0x0202_0202);
}

#[test]
fn hw_trait_flush_fold_cycle_many_vcpus() {
    // vcpu routing of SPIs and per-vcpu LR ownership
    let mut g = v3(64, 2);
    enable_all(&mut g, true);
    dw(&mut g, 0x104, 3);
    wr(&mut g, 0, DIST + 0x6108, 8, 1); // irq 33 -> vcpu 1
    assert_eq!(g.set_line(32, 0, true), 1);
    assert_eq!(g.set_line(33, 0, true), 2);
    let mut h0 = MockHw::new(V3Codec, 2);
    let mut h1 = MockHw::new(V3Codec, 2);
    g.sync_in(0, &mut h0, &V3Codec);
    g.sync_in(1, &mut h1, &V3Codec);
    assert_eq!(h0.ack().unwrap().0, 32);
    assert_eq!(h1.ack().unwrap().0, 33);
    g.sync_out(0, &mut h0, &V3Codec);
    g.sync_out(1, &mut h1, &V3Codec);
    assert_eq!(d(&mut g, 0x304), 3);
    // flush twice without a fold does not duplicate entries
    let mut lrs = [0u64; 4];
    g.set_line(32, 0, false);
    g.set_line(32, 0, true);
    g.flush(0, &mut lrs, &V3Codec);
    let n = lrs.iter().filter(|&&r| r != 0).count();
    g.flush(0, &mut lrs, &V3Codec);
    assert_eq!(lrs.iter().filter(|&&r| r != 0).count(), n);
}

#[test]
fn shrink_to_created_vcpus_before_init() {
    // KVM_CREATE_DEVICE runs before any vCPU exists; the kernel device shrinks to the
    // real count before CTRL INIT so a REDIST_REGION sized for them is accepted.
    let mut g = Vgic::new(Version::V3, 64, 8);
    g.set_nr_vcpus(2);
    assert_eq!(g.nr_vcpus, 2);
    g.attr_set(GRP_ADDR, V3_ADDR_DIST, &mut { DIST }).unwrap();
    g.attr_set(GRP_ADDR, V3_ADDR_REDIST_REGION, &mut { REDIST | 2u64 << 52 }).unwrap();
    g.attr_set(GRP_CTRL, CTRL_INIT, &mut 0).unwrap();
    g.set_nr_vcpus(1); // too late: ignored
    assert_eq!(g.nr_vcpus, 2);
    g.set_nr_vcpus(0); // clamps
}

/// A userspace exit between the guest's ack and EOI (an MMIO access in the handler) must not
/// make the still-high level line re-deliver: the active LR carries the EOI bit, so the EOI
/// empties it, raises the maintenance interrupt and the line gets resampled.
#[test]
fn level_spi_exit_between_ack_and_eoi_still_resamples() {
    let mut g = v3(32, 1);
    enable_all(&mut g, true);
    dw(&mut g, 0x104, 1);
    let mut hw = MockHw::new(V3Codec, 4);
    g.set_line(32, 0, true);
    run_hw(&mut g, 0, &mut hw);
    assert_eq!(hw.ack(), Some((32, 0)));
    g.sync_out(0, &mut hw, &V3Codec); // exit to userspace in the handler
    assert!(g.take_resample().is_empty());
    run_hw(&mut g, 0, &mut hw); // re-enter: active, line high
    let e = V3Codec.decode(hw.lrs[0]);
    assert!(e.active && !e.pending && e.eoi);
    hw.eoi(32);
    assert!(hw.maintenance_asserted());
    g.sync_out(0, &mut hw, &V3Codec);
    assert_eq!(g.take_resample(), vec![32]);
}
