use kmod_abi::layout;
use kmod_abi::{KmodCtx, KmodHeader, KSym, EM_AARCH64, EM_X86_64};
use kmod_elf::*;

const BID: [u8; 20] = *b"0123456789abcdefghij";

fn base(m: u16) -> u64 {
    if m == EM_X86_64 as u16 { layout::x86_64::BASE } else { layout::aarch64::BASE }
}
fn expect(m: u16) -> Expect<'static> {
    Expect { machine: m, build_id: &BID, window_base: base(m), window_size: layout::SIZE }
}
fn good(m: u16) -> Vec<u8> {
    let tc = text_tailcall(m, 0x1234_5678_9abc_def0);
    synth(m, &BID, base(m) + 3 * layout::SLOT_ALIGN, "demo_mod", &tc, b"rodata!", &[1, 2, 3, 4], 100)
}
const MS: [u16; 2] = [EM_X86_64 as u16, EM_AARCH64 as u16];
fn ph(i: usize) -> usize { 64 + 56 * i }
fn p32(v: &mut [u8], o: usize, x: u32) { v[o..o + 4].copy_from_slice(&x.to_le_bytes()) }
fn p64(v: &mut [u8], o: usize, x: u64) { v[o..o + 8].copy_from_slice(&x.to_le_bytes()) }
fn g64(v: &[u8], o: usize) -> u64 { u64::from_le_bytes(v[o..o + 8].try_into().unwrap()) }
const H: usize = 4096; // file offset of the KmodHeader

#[test]
fn layout_and_sizes() {
    assert_eq!(core::mem::size_of::<KmodHeader>(), 128);
    assert_eq!(core::mem::size_of::<KmodCtx>(), 48);
    assert_eq!(core::mem::size_of::<KSym>(), 16);
    assert_eq!(layout::TEST_SLOT_OFF, 1022 * layout::SLOT_ALIGN);
    assert_eq!(layout::NSLOTS, 1024);
    assert_eq!(base(62) + layout::SIZE, 0xffff_ffff_a000_0000);
}

#[test]
fn roundtrip() {
    for m in MS {
        let img = good(m);
        let p = validate(&img, &expect(m)).unwrap();
        assert_eq!(p.nsegs, 3);
        assert_eq!(p.hdr.slot_base, base(m) + 3 * layout::SLOT_ALIGN);
        assert_eq!(&p.hdr.name[..8], b"demo_mod");
        assert_eq!(p.segs[0].unwrap().perm, Perm::R);
        assert_eq!(p.segs[1].unwrap().perm, Perm::RX);
        let d = p.segs[2].unwrap();
        assert_eq!((d.perm, d.filesz, d.memsz), (Perm::RW, 4, 104));
        assert_eq!(p.hdr.init, p.segs[1].unwrap().vaddr);
        assert_eq!(ElfErr::Short.errno(), -8);
    }
}

#[test]
fn tailcall_bytes() {
    let x = text_tailcall(62, 0x1122_3344_5566_7788);
    assert_eq!(&x[..2], &[0x48, 0xb8]);
    assert_eq!(&x[10..12], &[0xff, 0xe0]);
    let a = text_tailcall(183, 0x1122_3344_5566_7788);
    assert_eq!(&a[..8], &[0x50, 0x00, 0x00, 0x58, 0x00, 0x02, 0x1f, 0xd6]);
    assert_eq!(g64(&a, 8), 0x1122_3344_5566_7788);
}

fn neg(m: u16, f: impl FnOnce(&mut Vec<u8>), want: ElfErr) {
    let mut img = good(m);
    f(&mut img);
    assert_eq!(validate(&img, &expect(m)).err(), Some(want), "machine {m}");
}

#[test]
fn one_negative_per_variant() {
    for m in MS {
        neg(m, |v| v.truncate(10), ElfErr::Short);
        neg(m, |v| v[4] = 1, ElfErr::NotElf64Le);
        neg(m, |v| v[16] = 3, ElfErr::NotExec);
        neg(m, |v| v[18] ^= 1, ElfErr::Machine);
        neg(m, |v| {
            v[56] = 9;
            for i in 3..9 {
                let p = ph(i);
                p32(v, p, 1);
                p32(v, p + 4, 4);
                p64(v, p + 8, 4096);
                p64(v, p + 16, base(m) + 3 * layout::SLOT_ALIGN + 0x10_0000 + i as u64 * 0x1000);
            }
        }, ElfErr::TooManySegs);
        neg(m, |v| { let o = ph(2) + 32; p64(v, o, 1 << 20); }, ElfErr::BadPhdr);
        neg(m, |v| { let o = ph(1) + 16; let x = g64(v, o); p64(v, o, x + 1); }, ElfErr::Unaligned);
        neg(m, |v| { let a = g64(v, ph(0) + 16); p64(v, ph(1) + 16, a); }, ElfErr::Overlap);
        neg(m, |v| p32(v, ph(1) + 4, 7), ElfErr::WplusX);
        neg(m, |v| p32(v, ph(2), 3), ElfErr::Interp);
        neg(m, |v| p64(v, ph(2) + 40, 0x30_0000), ElfErr::OutsideSlot);
        neg(m, |v| { p64(v, ph(2) + 16, 0xffff_ffff_ffff_f000); p64(v, ph(2) + 40, 0x2000); }, ElfErr::Overflow);
        neg(m, |v| p32(v, ph(0) + 4, 5), ElfErr::NoHeader);
        neg(m, |v| v[H] ^= 1, ElfErr::Magic);
        neg(m, |v| p32(v, H + 8, 2), ElfErr::AbiVersion);
        neg(m, |v| p64(v, H + 64, 0), ElfErr::InitNotRx);
    }
    // outside-window and build-id come from the expectation
    for m in MS {
        let img = good(m);
        let mut e = expect(m);
        e.window_base += 4 * layout::SLOT_ALIGN;
        assert_eq!(validate(&img, &e).err(), Some(ElfErr::SlotNotInWindow));
        let other = [9u8; 20];
        let mut e = expect(m);
        e.build_id = &other;
        assert_eq!(validate(&img, &e).err(), Some(ElfErr::BuildId));
    }
}

#[test]
fn fuzz_never_panics() {
    let mut s = 0x9e37_79b9_7f4a_7c15u64;
    let mut rnd = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    for m in MS {
        let orig = good(m);
        let ex = expect(m);
        for i in 0..10_000 {
            let mut v = orig.clone();
            match i % 3 {
                0 => { let n = (rnd() as usize) % (v.len() + 1); v.truncate(n); }
                1 => { for _ in 0..1 + rnd() % 4 { let p = rnd() as usize % v.len(); v[p] ^= 1 << (rnd() % 8); } }
                _ => {
                    // flip within header regions to reach deep checks
                    for _ in 0..1 + rnd() % 3 {
                        let p = if rnd() & 1 == 0 { rnd() as usize % 300 } else { H + rnd() as usize % 128 };
                        v[p] = rnd() as u8;
                    }
                }
            }
            let _ = validate(&v, &ex);
        }
    }
}
