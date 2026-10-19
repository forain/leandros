//! Minimal ELF64 PT_LOAD reader for `kexec_file_load`: physical segments only,
//! nothing is mapped.

use alloc::vec::Vec;

use super::{EIO, ENOEXEC};
use crate::syscall::{read_fd_upto, sys_lseek_pub};

#[cfg(target_arch = "x86_64")]
const EM_TARGET: u16 = 62;
#[cfg(target_arch = "aarch64")]
const EM_TARGET: u16 = 183;

pub const MAX_SEGS: usize = 16;
const HDR_READ: usize = 32 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct Seg {
    pub paddr: u64,
    pub vaddr: u64,
    pub offset: u64,
    pub filesz: u64,
    pub memsz: u64,
}

pub struct Parsed {
    /// Physical entry point.
    pub entry: u64,
    pub segs: Vec<Seg>,
}

fn rd16(b: &[u8], o: usize) -> u16 { u16::from_le_bytes([b[o], b[o + 1]]) }
fn rd32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) }
fn rd64(b: &[u8], o: usize) -> u64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }

/// Parse the kernel image behind `fd`. Errors are negative errnos.
pub fn parse(fd: usize) -> Result<Parsed, isize> {
    let r = sys_lseek_pub(fd, 0, 0);
    if r < 0 { return Err(r); }
    let mut hdr = alloc::vec![0u8; HDR_READ];
    let n = read_fd_upto(fd, &mut hdr);
    if n < 0 { return Err(n); }
    let hdr = &hdr[..n as usize];
    if hdr.len() < 64 || &hdr[..4] != b"\x7fELF" || hdr[4] != 2 || hdr[5] != 1 {
        return Err(ENOEXEC);
    }
    let e_type = rd16(hdr, 16);
    let e_machine = rd16(hdr, 18);
    if e_type != 2 || e_machine != EM_TARGET { return Err(ENOEXEC); }
    let e_entry = rd64(hdr, 24);
    let phoff = rd64(hdr, 32) as usize;
    let phentsize = rd16(hdr, 54) as usize;
    let phnum = rd16(hdr, 56) as usize;
    if phentsize != 56 || phnum == 0 || phnum > 64 { return Err(ENOEXEC); }
    let ph_end = phoff.checked_add(phnum * 56).ok_or(ENOEXEC)?;
    if ph_end > hdr.len() { return Err(ENOEXEC); }

    let mut segs: Vec<Seg> = Vec::new();
    for i in 0..phnum {
        let o = phoff + i * 56;
        if rd32(hdr, o) != 1 { continue; } // PT_LOAD only
        let s = Seg {
            offset: rd64(hdr, o + 8),
            vaddr: rd64(hdr, o + 16),
            paddr: rd64(hdr, o + 24),
            filesz: rd64(hdr, o + 32),
            memsz: rd64(hdr, o + 40),
        };
        if s.memsz == 0 { continue; }
        if s.filesz > s.memsz { return Err(ENOEXEC); }
        if s.paddr.checked_add(s.memsz).is_none() || s.offset.checked_add(s.filesz).is_none()
            || s.vaddr.checked_add(s.memsz).is_none() {
            return Err(ENOEXEC);
        }
        if segs.len() == MAX_SEGS { return Err(ENOEXEC); }
        segs.push(s);
    }
    if segs.is_empty() { return Err(ENOEXEC); }

    // Physical entry: through the segment holding e_entry.
    let seg = segs.iter()
        .find(|s| e_entry >= s.vaddr && e_entry < s.vaddr + s.memsz)
        .ok_or(ENOEXEC)?;
    #[allow(unused_mut)]
    let mut entry = seg.paddr + (e_entry - seg.vaddr);

    #[cfg(target_arch = "x86_64")]
    {
        // Must be a multiboot2 kernel: header magic in the first 32 KiB.
        let magic = 0xE852_50D6u32.to_le_bytes();
        let found = hdr.chunks(8).any(|c| c.len() >= 4 && c[..4] == magic);
        if !found || entry >= (1 << 32) { return Err(ENOEXEC); }
    }
    #[cfg(target_arch = "aarch64")]
    {
        // arm64 Image header: "ARM\x64" at offset 0x38 of the lowest segment.
        let low = segs.iter().min_by_key(|s| s.paddr).unwrap();
        let mut m = [0u8; 4];
        let r = sys_lseek_pub(fd, low.offset as usize + 0x38, 0);
        if r < 0 { return Err(r); }
        let n = read_fd_upto(fd, &mut m);
        if n < 0 { return Err(n); }
        if n != 4 { return Err(EIO); }
        if &m != b"ARM\x64" { return Err(ENOEXEC); }
        // The boot protocol enters at the image start (code0 = `b _start`),
        // not at e_entry (_start itself, +0x40 past the header).
        if entry < low.paddr || entry >= low.paddr + low.memsz { return Err(ENOEXEC); }
        entry = low.paddr;
    }
    #[cfg(target_arch = "x86_64")]
    let _ = EIO;
    Ok(Parsed { entry, segs })
}
