//! Generic PCI config-space save/restore (Linux `pci_save_state` /
//! `pci_restore_state`) for every scanned function. Inert unless called:
//! `kernel/src/pm.rs` drives it from the freeze-only cycle when asked.
//!
//! Saved: command, cache line/latency, BARs, ROM BAR, interrupt line, bridge
//! bus numbers and windows, PM cap PMCSR, MSI (ctrl/addr/data/mask), MSI-X
//! (ctrl + every table entry, read back from BAR MMIO), PCIe cap
//! (DevCtl/LnkCtl/SltCtl/RootCtl/DevCtl2/LnkCtl2), LTR and L1SS ext caps.
//! Restore is bridges first (scan order is ascending bus), PMCSR to D0, then
//! the header with COMMAND=0, then COMMAND last, then the capabilities.

use alloc::string::String;
use alloc::vec::Vec;
use crate::pci::{PciFunction, scan, bus_to_cpu};

#[derive(Clone, Copy)]
struct Ent { off: u16, w: u8, val: u32 }

pub struct FnState {
    f: PciFunction,
    vid_did: u32,
    command: u16,
    pmcsr_off: u16,
    pmcsr: u16,
    hdr: Vec<Ent>,
    caps: Vec<Ent>,
    /// (cap offset, ctrl, table phys, entries as dwords)
    msix: Option<(u16, u16, u64, Vec<u32>)>,
}

fn rd(f: &PciFunction, e: &Ent) -> u32 {
    if e.w == 2 { f.read16(e.off) as u32 } else { f.read32(e.off) }
}
fn wr(f: &PciFunction, e: &Ent) {
    if e.w == 2 { f.write16(e.off, e.val as u16) } else { f.write32(e.off, e.val) }
}
fn ent(f: &PciFunction, off: u16, w: u8, v: &mut Vec<Ent>) {
    let e = Ent { off, w, val: 0 };
    v.push(Ent { val: rd(f, &e), ..e });
}

fn msix_table(f: &PciFunction, p: u16, ctrl: u16) -> Option<(u64, usize)> {
    let t = f.read32(p + 4);
    let bar = f.bar((t & 7) as usize)?;
    if bar.io { return None; }
    let n = (ctrl & 0x7FF) as usize + 1;
    Some((bus_to_cpu(bar.base) + (t & !7) as u64, n))
}

fn msix_map(phys: u64, n: usize) -> Option<usize> {
    mm::kva::ioremap(phys, n * 16, mm::kva::CacheMode::Uncached)
}

pub fn save_all() -> Vec<FnState> {
    let mut out = Vec::new();
    for d in scan().iter() {
        let f = d.function();
        let vd = f.read32(0);
        if vd & 0xFFFF == 0xFFFF { continue; }
        let htype = f.read8(0x0E) & 0x7F;
        let mut hdr = Vec::new();
        let mut caps = Vec::new();
        ent(&f, 0x0C, 2, &mut hdr);
        match htype {
            0 => {
                for o in (0x10u16..=0x24).step_by(4) { ent(&f, o, 4, &mut hdr); }
                ent(&f, 0x30, 4, &mut hdr);
                ent(&f, 0x3C, 4, &mut hdr);
            }
            1 => {
                ent(&f, 0x10, 4, &mut hdr); ent(&f, 0x14, 4, &mut hdr);
                ent(&f, 0x18, 4, &mut hdr);
                ent(&f, 0x1C, 2, &mut hdr); // IO base/limit (upper half is RW1C status)
                for o in [0x20u16, 0x24, 0x28, 0x2C, 0x30, 0x38, 0x3C] { ent(&f, o, 4, &mut hdr); }
            }
            _ => { ent(&f, 0x3C, 2, &mut hdr); }
        }
        let (mut pmcsr_off, mut pmcsr) = (0u16, 0u16);
        let mut msix = None;
        for (id, p) in f.capabilities() {
            match id {
                0x01 => { pmcsr_off = p + 4; pmcsr = f.read16(p + 4); }
                0x05 => {
                    let c = f.read16(p + 2);
                    let is64 = c & 0x80 != 0;
                    let maskable = c & 0x100 != 0;
                    ent(&f, p + 4, 4, &mut caps);
                    if is64 {
                        ent(&f, p + 8, 4, &mut caps); ent(&f, p + 0xC, 2, &mut caps);
                        if maskable { ent(&f, p + 0x10, 4, &mut caps); }
                    } else {
                        ent(&f, p + 8, 2, &mut caps);
                        if maskable { ent(&f, p + 0xC, 4, &mut caps); }
                    }
                    ent(&f, p + 2, 2, &mut caps); // ctrl (enable) last
                }
                0x10 => {
                    ent(&f, p + 8, 2, &mut caps);
                    ent(&f, p + 0x10, 2, &mut caps);
                    let ver_type = (f.read16(p + 2) >> 4) & 0xF;
                    if f.read16(p + 2) & 0x100 != 0 || ver_type == 6 || ver_type == 4 { ent(&f, p + 0x18, 2, &mut caps); }
                    if ver_type == 4 || ver_type == 6 { ent(&f, p + 0x1C, 2, &mut caps); }
                    if (f.read16(p + 2) & 0xF) >= 2 {
                        ent(&f, p + 0x28, 2, &mut caps);
                        ent(&f, p + 0x30, 2, &mut caps);
                    }
                }
                0x11 => {
                    let c = f.read16(p + 2);
                    let mut tbl = Vec::new();
                    let mut phys = 0;
                    if let Some((ph, n)) = msix_table(&f, p, c) {
                        if let Some(va) = msix_map(ph, n) {
                            for i in 0..n * 4 {
                                tbl.push(mrd((va + i * 4) as *const u32));
                            }
                            mm::kva::iounmap(va);
                            phys = ph;
                        }
                    }
                    msix = Some((p, c, phys, tbl));
                }
                _ => {}
            }
        }
        for (id, _v, p) in f.ext_capabilities() {
            match id {
                0x18 => ent(&f, p + 4, 4, &mut caps),
                0x1E => { ent(&f, p + 8, 4, &mut caps); ent(&f, p + 0xC, 4, &mut caps); }
                _ => {}
            }
        }
        out.push(FnState { f, vid_did: vd, command: f.read16(4), pmcsr_off, pmcsr, hdr, caps, msix });
    }
    out
}

/// Restore every saved function. `all`: write every register unconditionally
/// (what real power loss needs, including the COMMAND=0 pulse); otherwise
/// only registers that differ are written (safe on running devices).
/// Returns the functions whose vendor id reads 0xFFFF (link down).
pub fn restore_all(st: &[FnState], all: bool, delay_ms: fn(u32)) -> Vec<PciFunction> {
    let mut dead = Vec::new();
    for s in st {
        let f = &s.f;
        if f.read16(0) == 0xFFFF { dead.push(*f); continue; }
        if s.pmcsr_off != 0 {
            let cur = f.read16(s.pmcsr_off);
            if cur & 3 != 0 {
                f.write16(s.pmcsr_off, cur & !3);
                if cur & 3 == 3 { delay_ms(10); }
            }
            if (f.read16(s.pmcsr_off) & !3) != (s.pmcsr & !3) { f.write16(s.pmcsr_off, s.pmcsr & !3); }
        }
        let need = all || s.hdr.iter().chain(s.caps.iter()).any(|e| rd(f, e) != e.val) || f.read16(4) != s.command
            || s.msix.as_ref().map_or(false, |m| msix_differs(m));
        if !need { continue; }
        if all { f.write16(4, 0); } else if f.read16(4) & 7 != 0 && s.hdr.iter().any(|e| rd(f, e) != e.val) { f.write16(4, 0); }
        // Header: BARs and bridge windows in descending offset like Linux.
        for e in s.hdr.iter().rev() { if all || rd(f, e) != e.val { wr(f, e); } }
        // COMMAND before the capabilities: the MSI-X table is BAR memory.
        f.write16(4, s.command);
        for e in &s.caps { if all || rd(f, e) != e.val { wr(f, e); } }
        if let Some(m) = &s.msix { msix_restore(f, m); }
    }
    dead
}

fn msix_differs(m: &(u16, u16, u64, Vec<u32>)) -> bool {
    if m.2 == 0 { return false; }
    let n = m.3.len() / 4;
    let Some(va) = msix_map(m.2, n) else { return false };
    let mut d = false;
    for (i, v) in m.3.iter().enumerate() {
        if mrd((va + i * 4) as *const u32) != *v { d = true; break; }
    }
    mm::kva::iounmap(va);
    d
}

fn msix_restore(f: &PciFunction, m: &(u16, u16, u64, Vec<u32>)) {
    let (p, ctrl, phys, tbl) = (m.0, m.1, m.2, &m.3);
    f.write16(p + 2, ctrl | 0x4000); // function mask while the table is rewritten
    if phys != 0 {
        if let Some(va) = msix_map(phys, tbl.len() / 4) {
            for (i, v) in tbl.iter().enumerate() {
                if mrd((va + i * 4) as *const u32) != *v {
                    mwr((va + i * 4) as *mut u32, *v);
                }
            }
            mm::kva::iounmap(va);
        }
    }
    f.write16(p + 2, ctrl);
}

/// Compare the live config space with the saved state. Returns the number of
/// dwords compared and a description of every mismatch.
pub fn verify(st: &[FnState]) -> (usize, Vec<String>) {
    let (mut n, mut bad) = (0usize, Vec::new());
    for s in st {
        let f = &s.f;
        let tag = alloc::format!("{:02x}:{:02x}.{} {:04x}:{:04x}", f.bus, f.dev, f.func, s.vid_did & 0xFFFF, s.vid_did >> 16);
        if f.read32(0) != s.vid_did { bad.push(alloc::format!("{} id now {:08x}", tag, f.read32(0))); continue; }
        n += 1;
        if f.read16(4) != s.command { bad.push(alloc::format!("{} command {:04x} != {:04x}", tag, f.read16(4), s.command)); }
        for e in s.hdr.iter().chain(s.caps.iter()) {
            n += 1;
            let v = rd(f, e);
            if v != e.val { bad.push(alloc::format!("{} @{:#x} {:x} != saved {:x}", tag, e.off, v, e.val)); }
        }
        if let Some(m) = &s.msix {
            n += 1 + m.3.len();
            let c = f.read16(m.0 + 2);
            if c & 0xBFFF != m.1 & 0xBFFF { bad.push(alloc::format!("{} msix ctrl {:04x} != {:04x}", tag, c, m.1)); }
            if m.2 != 0 {
                if let Some(va) = msix_map(m.2, m.3.len() / 4) {
                    for (i, v) in m.3.iter().enumerate() {
                        let cur = mrd((va + i * 4) as *const u32);
                        if cur != *v { bad.push(alloc::format!("{} msix tbl dword {} {:x} != {:x}", tag, i, cur, v)); }
                    }
                    mm::kva::iounmap(va);
                }
            }
        }
    }
    (n, bad)
}

/// Overwrite scratch-safe registers of every saved function: interrupt line,
/// cache line size, latency timer, and (deep) MSI data and MSI-X entry 0 data.
/// Returns how many clobbers actually changed a register.
pub fn clobber(st: &[FnState], deep: bool) -> usize {
    let mut stuck = 0;
    for s in st {
        let f = &s.f;
        let mut hit = false;
        for off in [0x3Cu16, 0x0C, 0x0D] {
            if s.hdr.iter().all(|e| !(e.off <= off && off < e.off + e.w as u16)) { continue; }
            let o = f.read8(off);
            f.write8(off, o ^ 0x5A);
            if f.read8(off) != o { hit = true; }
        }
        if deep {
            for (id, p) in f.capabilities() {
                if id == 0x05 {
                    let is64 = f.read16(p + 2) & 0x80 != 0;
                    let off = if is64 { p + 0xC } else { p + 8 };
                    let o = f.read16(off);
                    f.write16(off, o ^ 0x00F0);
                    if f.read16(off) != o { hit = true; }
                }
            }
            if let Some(m) = &s.msix {
                if m.2 != 0 {
                    if let Some(va) = msix_map(m.2, 1) {
                        let a = (va + 8) as *mut u32;
                        let o = mrd(a as *const u32);
                        mwr(a, o ^ 0xF0);
                        if mrd(a as *const u32) != o { hit = true; }
                        mm::kva::iounmap(va);
                    }
                }
            }
        }
        if hit { stuck += 1; }
    }
    stuck
}

pub fn count(st: &[FnState]) -> usize { st.len() }
pub fn msix_entries(st: &[FnState]) -> usize { st.iter().map(|s| s.msix.as_ref().map_or(0, |m| m.3.len() / 4)).sum() }

fn mrd(a: *const u32) -> u32 { unsafe { leandros_lib::mmio::read(a) } }
fn mwr(a: *mut u32, v: u32) { unsafe { leandros_lib::mmio::write(a, v) } }
