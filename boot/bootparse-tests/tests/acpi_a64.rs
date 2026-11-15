//! Synthetic aarch64 ACPI tables (QEMU-virt-shaped and Graviton-shaped).
use boot::acpi_a64::*;

fn hdr(sig: &[u8; 4], len: usize) -> Vec<u8> {
    let mut t = vec![0u8; len];
    t[0..4].copy_from_slice(sig);
    t[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    t
}
fn put64(t: &mut [u8], o: usize, v: u64) { t[o..o + 8].copy_from_slice(&v.to_le_bytes()); }
fn put32(t: &mut [u8], o: usize, v: u32) { t[o..o + 4].copy_from_slice(&v.to_le_bytes()); }

fn madt() -> Vec<u8> {
    let mut t = hdr(b"APIC", 44);
    for cpu in 0..4u32 {
        let mut e = vec![0u8; 80];
        e[0] = 0x0B; e[1] = 80;
        put32(&mut e, 12, 1);
        put64(&mut e, 68, cpu as u64);
        t.extend(e);
    }
    let mut d = vec![0u8; 24];
    d[0] = 0x0C; d[1] = 24; put64(&mut d, 8, 0x3FFF_0000); d[20] = 3;
    t.extend(d);
    for (b, l) in [(0x4000_0000u64, 0x20_0000u32), (0x5000_0000, 0x10_0000)] {
        let mut r = vec![0u8; 16];
        r[0] = 0x0E; r[1] = 16; put64(&mut r, 4, b); put32(&mut r, 12, l);
        t.extend(r);
    }
    let mut i = vec![0u8; 20];
    i[0] = 0x0F; i[1] = 20; put64(&mut i, 8, 0x3FFE_0000);
    t.extend(i);
    let n = t.len() as u32;
    put32(&mut t, 4, n);
    t
}

#[test]
fn madt_gic_ranges_its_cpus() {
    let m = parse_madt(&madt()).unwrap();
    assert_eq!((m.gicd_base, m.gic_version, m.cpus), (0x3FFF_0000, 3, 4));
    assert_eq!(m.gicr_n, 2);
    assert_eq!(m.gicr[0], (0x4000_0000, 0x20_0000));
    assert_eq!(m.gicr[1], (0x5000_0000, 0x10_0000));
    assert_eq!((m.its_n, m.its[0]), (1, 0x3FFE_0000));
    assert!(parse_madt(&hdr(b"XXXX", 60)).is_none());
}

fn spcr(iface: u8, access: u8, base: u64) -> Vec<u8> {
    let mut t = hdr(b"SPCR", 80);
    t[36] = iface; t[41] = 32; t[43] = access;
    put64(&mut t, 44, base);
    t[52] = 0x08; put32(&mut t, 54, 33); t[58] = 7;
    t
}

#[test]
fn spcr_pl011_and_16550() {
    let p = parse_spcr(&spcr(0x12, 3, 0x1_0000_0000)).unwrap();
    assert_eq!((p.kind, p.reg_shift, p.access_bytes, p.gsi), (UartKind::Ns16550, 2, 4, 33));
    let b = parse_spcr(&spcr(0x00, 1, 0x2000)).unwrap();
    assert_eq!((b.kind, b.reg_shift, b.access_bytes), (UartKind::Ns16550, 0, 1));
    let q = parse_spcr(&spcr(0x03, 0, 0x0900_0000)).unwrap();
    assert_eq!((q.kind, q.base, q.baud_code), (UartKind::Pl011, 0x0900_0000, 7));
    assert!(parse_spcr(&spcr(0x7F, 1, 0x1000)).is_none());
    assert!(parse_spcr(&spcr(0x00, 1, 0)).is_none());
}

#[test]
fn gtdt_mcfg_fadt() {
    let mut g = hdr(b"GTDT", 104);
    put32(&mut g, 48, 29); put32(&mut g, 56, 30); put32(&mut g, 64, 27); put32(&mut g, 72, 26);
    assert_eq!(parse_gtdt(&g).unwrap(), Gtdt { secure_el1: 29, ns_el1: 30, virt: 27, el2: 26 });

    let mut m = hdr(b"MCFG", 44 + 32);
    put64(&mut m, 44, 0x40_1000_0000); m[44 + 10] = 0; m[44 + 11] = 0xFF;
    put64(&mut m, 60, 0x50_0000_0000); m[60 + 8] = 1; m[60 + 10] = 0; m[60 + 11] = 0x7F;
    let mut out = [EcamRegion::default(); MAX_ECAM];
    assert_eq!(parse_mcfg(&m, &mut out), 2);
    assert_eq!((out[1].segment, out[1].base, out[1].end_bus), (1, 0x50_0000_0000, 0x7F));

    let mut f = hdr(b"FACP", 276);
    assert_eq!(parse_fadt_psci(&f), Psci::None);
    f[129] = 1; assert_eq!(parse_fadt_psci(&f), Psci::Smc);
    f[129] = 3; assert_eq!(parse_fadt_psci(&f), Psci::Hvc);
}

#[test]
fn ns16550_register_mapping() {
    assert_eq!(fr_from_lsr(0x60), FR_RXFE);            // THRE|TEMT, no data
    assert_eq!(fr_from_lsr(0x01), FR_TXFF);            // data ready, THR busy
    assert_eq!(fr_from_lsr(0x21), 0);
    assert_eq!(ns_off(NS_LSR, 2), 0x14);
    assert_eq!(ns_off(NS_LSR, 0), 5);
}
