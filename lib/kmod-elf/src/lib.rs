//! Pure validator and synthesiser for kernel-module ELF images (kmod ABI v1).
#![no_std]
extern crate alloc;

use alloc::vec::Vec;
use kmod_abi::{KmodHeader, EM_AARCH64, EM_X86_64, HEADER_SIZE, KMOD_ABI_VERSION, KMOD_MAGIC};

pub struct Expect<'a> {
    pub machine: u16,
    pub build_id: &'a [u8],
    pub window_base: u64,
    pub window_size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Perm {
    R,
    RX,
    RW,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seg {
    pub vaddr: u64,
    pub memsz: u64,
    pub off: u64,
    pub filesz: u64,
    pub perm: Perm,
}

#[derive(Clone, Copy, Debug)]
pub struct Plan {
    pub hdr: KmodHeader,
    pub segs: [Option<Seg>; 8],
    pub nsegs: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElfErr {
    Short,
    NotElf64Le,
    NotExec,
    Machine,
    TooManySegs,
    BadPhdr,
    Unaligned,
    Overlap,
    WplusX,
    Interp,
    OutsideSlot,
    SlotNotInWindow,
    NoHeader,
    Magic,
    AbiVersion,
    BuildId,
    InitNotRx,
    Overflow,
}

impl ElfErr {
    /// Always -ENOEXEC.
    pub fn errno(&self) -> i32 {
        -8
    }
}

const PAGE: u64 = 4096;
const SLOT_ALIGN: u64 = 0x20_0000;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PT_TLS: u32 = 7;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
const EHSIZE: usize = 64;
const PHSIZE: usize = 56;

fn rd16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rd64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn page_up(v: u64) -> Option<u64> {
    v.checked_add(PAGE - 1).map(|x| x & !(PAGE - 1))
}

pub fn validate(img: &[u8], ex: &Expect) -> Result<Plan, ElfErr> {
    if img.len() < EHSIZE {
        return Err(ElfErr::Short);
    }
    if img[0..4] != [0x7f, b'E', b'L', b'F'] || img[4] != 2 || img[5] != 1 {
        return Err(ElfErr::NotElf64Le);
    }
    if rd16(img, 16) != 2 {
        return Err(ElfErr::NotExec);
    }
    let e_machine = rd16(img, 18);
    if e_machine != ex.machine {
        return Err(ElfErr::Machine);
    }
    let phoff = rd64(img, 32);
    let phentsize = rd16(img, 54) as usize;
    let phnum = rd16(img, 56) as usize;
    if phentsize != PHSIZE {
        return Err(ElfErr::BadPhdr);
    }
    let tbl_end = (phnum as u64)
        .checked_mul(PHSIZE as u64)
        .and_then(|n| n.checked_add(phoff))
        .ok_or(ElfErr::Overflow)?;
    if tbl_end > img.len() as u64 {
        return Err(ElfErr::Short);
    }
    let phoff = phoff as usize;

    let mut segs: [Option<Seg>; 8] = [None; 8];
    let mut n = 0usize;
    let mut prev_end: u64 = 0;
    for i in 0..phnum {
        let p = phoff + i * PHSIZE;
        let ty = rd32(img, p);
        match ty {
            PT_INTERP | PT_DYNAMIC | PT_TLS => return Err(ElfErr::Interp),
            PT_LOAD => {}
            _ => continue,
        }
        if n == 8 {
            return Err(ElfErr::TooManySegs);
        }
        let flags = rd32(img, p + 4);
        let off = rd64(img, p + 8);
        let vaddr = rd64(img, p + 16);
        let filesz = rd64(img, p + 32);
        let memsz = rd64(img, p + 40);
        if vaddr % PAGE != 0 || off % PAGE != vaddr % PAGE {
            return Err(ElfErr::Unaligned);
        }
        if filesz > memsz {
            return Err(ElfErr::BadPhdr);
        }
        let fend = off.checked_add(filesz).ok_or(ElfErr::Overflow)?;
        let mend = vaddr.checked_add(memsz).ok_or(ElfErr::Overflow)?;
        let mend_pg = page_up(mend).ok_or(ElfErr::Overflow)?;
        if fend > img.len() as u64 {
            return Err(ElfErr::Short);
        }
        if flags & PF_R == 0 {
            return Err(ElfErr::BadPhdr);
        }
        let perm = match (flags & PF_W != 0, flags & PF_X != 0) {
            (true, true) => return Err(ElfErr::WplusX),
            (true, false) => Perm::RW,
            (false, true) => Perm::RX,
            (false, false) => Perm::R,
        };
        if n > 0 && vaddr < prev_end {
            return Err(ElfErr::Overlap);
        }
        prev_end = mend_pg;
        segs[n] = Some(Seg { vaddr, memsz, off, filesz, perm });
        n += 1;
    }
    if n == 0 {
        return Err(ElfErr::NoHeader);
    }
    let first = segs[0].unwrap();
    if first.perm != Perm::R || first.filesz < HEADER_SIZE as u64 {
        return Err(ElfErr::NoHeader);
    }
    let hdr = KmodHeader::from_bytes(&img[first.off as usize..]).ok_or(ElfErr::NoHeader)?;
    if hdr.magic != KMOD_MAGIC {
        return Err(ElfErr::Magic);
    }
    if hdr.abi_version != KMOD_ABI_VERSION {
        return Err(ElfErr::AbiVersion);
    }
    if hdr.header_size != HEADER_SIZE as u32 || hdr.slot_base != first.vaddr {
        return Err(ElfErr::NoHeader);
    }
    if hdr.machine != e_machine as u32 || !(hdr.machine == EM_X86_64 || hdr.machine == EM_AARCH64)
    {
        return Err(ElfErr::Machine);
    }
    if hdr.build_id_len != 20
        || ex.build_id.len() != 20
        || hdr.build_id[..] != *ex.build_id
    {
        return Err(ElfErr::BuildId);
    }
    // slot geometry
    if hdr.slot_base % SLOT_ALIGN != 0 || hdr.slot_size == 0 || hdr.slot_size % SLOT_ALIGN != 0 {
        return Err(ElfErr::SlotNotInWindow);
    }
    let slot_end = hdr.slot_base.checked_add(hdr.slot_size).ok_or(ElfErr::Overflow)?;
    let win_end = ex.window_base.checked_add(ex.window_size).ok_or(ElfErr::Overflow)?;
    if hdr.slot_base < ex.window_base || slot_end > win_end {
        return Err(ElfErr::SlotNotInWindow);
    }
    for s in segs.iter().take(n).flatten() {
        // checked above: vaddr + memsz does not overflow
        if s.vaddr < hdr.slot_base || s.vaddr + s.memsz > slot_end {
            return Err(ElfErr::OutsideSlot);
        }
    }
    let in_rx = segs
        .iter()
        .take(n)
        .flatten()
        .any(|s| s.perm == Perm::RX && hdr.init >= s.vaddr && hdr.init < s.vaddr + s.memsz);
    if !in_rx {
        return Err(ElfErr::InitNotRx);
    }
    Ok(Plan { hdr, segs, nsegs: n })
}

fn w16(v: &mut [u8], o: usize, x: u16) {
    v[o..o + 2].copy_from_slice(&x.to_le_bytes());
}
fn w32(v: &mut [u8], o: usize, x: u32) {
    v[o..o + 4].copy_from_slice(&x.to_le_bytes());
}
fn w64(v: &mut [u8], o: usize, x: u64) {
    v[o..o + 8].copy_from_slice(&x.to_le_bytes());
}

/// Build a valid ET_EXEC with three PT_LOADs: R (header + rodata) at
/// `slot_base`, RX (text) and RW (data + bss). `init` is the start of text.
pub fn synth(
    machine: u16,
    build_id: &[u8],
    slot_base: u64,
    name: &str,
    text: &[u8],
    rodata: &[u8],
    data: &[u8],
    bss: u64,
) -> Vec<u8> {
    let r_len = (HEADER_SIZE + rodata.len()) as u64;
    let r_va = slot_base;
    let t_va = r_va + page_up(r_len).unwrap();
    let d_va = t_va + page_up(text.len() as u64).unwrap();
    let d_mem = data.len() as u64 + bss;
    let end = d_va + page_up(d_mem).unwrap();
    let span = end - slot_base;
    let slot_size = span.div_ceil(SLOT_ALIGN).max(1) * SLOT_ALIGN;

    let mut hdr = KmodHeader {
        magic: KMOD_MAGIC,
        abi_version: KMOD_ABI_VERSION,
        header_size: HEADER_SIZE as u32,
        machine: machine as u32,
        flags: 0,
        build_id: [0; 20],
        build_id_len: 20,
        slot_base,
        slot_size,
        init: t_va,
        exit: 0,
        ids: 0,
        ids_len: 0,
        ids_entry_size: 0,
        name: [0; 32],
    };
    let bl = build_id.len().min(20);
    hdr.build_id[..bl].copy_from_slice(&build_id[..bl]);
    let nb = name.as_bytes();
    let nl = nb.len().min(31);
    hdr.name[..nl].copy_from_slice(&nb[..nl]);

    // file offset of a segment = PAGE + (vaddr - slot_base)
    let foff = |va: u64| (PAGE + (va - slot_base)) as usize;
    let mut f = alloc::vec![0u8; foff(d_va) + data.len()];
    f[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    f[4] = 2;
    f[5] = 1;
    f[6] = 1;
    w16(&mut f, 16, 2);
    w16(&mut f, 18, machine);
    w32(&mut f, 20, 1);
    w64(&mut f, 24, t_va);
    w64(&mut f, 32, EHSIZE as u64);
    w16(&mut f, 52, EHSIZE as u16);
    w16(&mut f, 54, PHSIZE as u16);
    w16(&mut f, 56, 3);

    hdr.write_bytes(&mut f[foff(r_va)..foff(r_va) + HEADER_SIZE]);
    f[foff(r_va) + HEADER_SIZE..foff(r_va) + HEADER_SIZE + rodata.len()].copy_from_slice(rodata);
    f[foff(t_va)..foff(t_va) + text.len()].copy_from_slice(text);
    f[foff(d_va)..foff(d_va) + data.len()].copy_from_slice(data);

    let ph = [
        (PF_R, r_va, r_len, r_len),
        (PF_R | PF_X, t_va, text.len() as u64, text.len() as u64),
        (PF_R | PF_W, d_va, data.len() as u64, d_mem),
    ];
    for (i, (fl, va, fs, ms)) in ph.iter().enumerate() {
        let p = EHSIZE + i * PHSIZE;
        w32(&mut f, p, PT_LOAD);
        w32(&mut f, p + 4, *fl);
        w64(&mut f, p + 8, foff(*va) as u64);
        w64(&mut f, p + 16, *va);
        w64(&mut f, p + 24, *va);
        w64(&mut f, p + 32, *fs);
        w64(&mut f, p + 40, *ms);
        w64(&mut f, p + 48, PAGE);
    }
    f
}

/// 16-byte stub that jumps to an absolute address.
pub fn text_tailcall(machine: u16, target: u64) -> [u8; 16] {
    let mut o = [0xccu8; 16];
    if machine as u32 == EM_AARCH64 {
        o[0..4].copy_from_slice(&0x5800_0050u32.to_le_bytes()); // ldr x16,#8
        o[4..8].copy_from_slice(&0xd61f_0200u32.to_le_bytes()); // br x16
        o[8..16].copy_from_slice(&target.to_le_bytes());
    } else {
        o[0] = 0x48;
        o[1] = 0xb8;
        o[2..10].copy_from_slice(&target.to_le_bytes());
        o[10] = 0xff;
        o[11] = 0xe0;
    }
    o
}
