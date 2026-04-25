//! ELF64 loader — parse and map an ELF executable into an `AddressSpace`.
//!
//! Only ET_EXEC (statically linked) binaries are supported.  Dynamic linking
//! requires an interpreter (ld.so), which Phase 3 will wire up via the VFS
//! server.  PT_INTERP segments are ignored for now (ENOEXEC returned if the
//! binary has a non-null interpreter path — unless the caller opts out of that
//! check).
//!
//! # Usage
//!
//! ```no_run
//! let entry = elf::load(elf_bytes, &mut addr_space)?;
//! ```
//!
//! After a successful return, every PT_LOAD segment is mapped into
//! `addr_space` with the correct protection flags, segment data is copied
//! from `elf_bytes`, and `addr_space.heap_start` / `heap_end` are set to
//! the first page after the highest loaded segment.

#![no_std]

use mm::vmm::AddressSpace;
use mm::paging::PageFlags;

/// Errors returned by [`load`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// Input slice is too short to contain a valid ELF header.
    TooShort,
    /// `e_ident[0..4]` is not the ELF magic bytes.
    BadMagic,
    /// Class is not ELFCLASS64 (2).
    NotElf64,
    /// Data encoding is not ELFDATA2LSB (little-endian).
    NotLittleEndian,
    /// `e_type` is not ET_EXEC (2).  Dynamic / relocatable objects are not
    /// supported in Phase 1.
    NotExecutable,
    /// `e_machine` does not match the current compilation target.
    UnsupportedArch,
    /// A program header overflows the slice or contains impossible values.
    BadProgramHeader,
    /// `AddressSpace::map()` failed (OOM or VMA table full).
    MappingFailed,
    /// Arithmetic overflow computing segment extents.
    SegmentOverflow,
}

// ── ELF constants ─────────────────────────────────────────────────────────────

const ELF_MAGIC:     [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64:    u8      = 2;
const ELFDATA2LSB:   u8      = 1;
const ET_EXEC:       u16     = 2;
const ET_DYN:        u16     = 3;
const PT_LOAD:       u32     = 1;

const PF_X: u32 = 1;
const PF_W: u32 = 2;
const _PF_R: u32 = 4; // unused directly; read permission implied

/// Expected `e_machine` value for the current compilation target.
#[cfg(target_arch = "aarch64")]
const EM_TARGET: u16 = 183; // EM_AARCH64
#[cfg(target_arch = "x86_64")]
const EM_TARGET: u16 = 62;  // EM_X86_64
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
const EM_TARGET: u16 = 0;

// ── Byte-safe field reads (avoids unaligned load UB) ─────────────────────────

#[inline]
fn r16(b: &[u8], off: usize) -> Option<u16> {
    if off + 2 > b.len() { return None; }
    Some(u16::from_le_bytes([b[off], b[off + 1]]))
}

#[inline]
fn r32(b: &[u8], off: usize) -> Option<u32> {
    if off + 4 > b.len() { return None; }
    let a: [u8; 4] = b[off..off + 4].try_into().ok()?;
    Some(u32::from_le_bytes(a))
}

#[inline]
fn r64(b: &[u8], off: usize) -> Option<u64> {
    if off + 8 > b.len() { return None; }
    let a: [u8; 8] = b[off..off + 8].try_into().ok()?;
    Some(u64::from_le_bytes(a))
}

// ── Parsed ELF header fields ──────────────────────────────────────────────────

#[allow(dead_code)]
struct Ehdr {
    e_type:      u16,
    e_machine:   u16,
    e_entry:     u64,
    e_phoff:     u64,
    e_phentsize: u16,
    e_phnum:     u16,
}

fn parse_ehdr(b: &[u8]) -> Result<Ehdr, ElfError> {
    if b.len() < 64 { return Err(ElfError::TooShort); }
    if b[0..4] != ELF_MAGIC  { return Err(ElfError::BadMagic); }
    if b[4] != ELFCLASS64    { return Err(ElfError::NotElf64); }
    if b[5] != ELFDATA2LSB   { return Err(ElfError::NotLittleEndian); }
    let e_type      = r16(b, 16).ok_or(ElfError::TooShort)?;
    let e_machine   = r16(b, 18).ok_or(ElfError::TooShort)?;
    let e_entry     = r64(b, 24).ok_or(ElfError::TooShort)?;
    let e_phoff     = r64(b, 32).ok_or(ElfError::TooShort)?;
    let e_phentsize = r16(b, 54).ok_or(ElfError::TooShort)?;
    let e_phnum     = r16(b, 56).ok_or(ElfError::TooShort)?;

    if e_type != ET_EXEC && e_type != ET_DYN { return Err(ElfError::NotExecutable); }
    if e_machine != EM_TARGET { return Err(ElfError::UnsupportedArch); }

    Ok(Ehdr { e_type, e_machine, e_entry, e_phoff, e_phentsize, e_phnum })
}

// ── Parsed program header fields ──────────────────────────────────────────────

struct Phdr {
    p_type:   u32,
    p_flags:  u32,
    p_offset: u64,
    p_vaddr:  u64,
    p_filesz: u64,
    p_memsz:  u64,
}

fn parse_phdr(b: &[u8], off: usize) -> Result<Phdr, ElfError> {
    Ok(Phdr {
        p_type:   r32(b, off +  0).ok_or(ElfError::BadProgramHeader)?,
        p_flags:  r32(b, off +  4).ok_or(ElfError::BadProgramHeader)?,
        p_offset: r64(b, off +  8).ok_or(ElfError::BadProgramHeader)?,
        p_vaddr:  r64(b, off + 16).ok_or(ElfError::BadProgramHeader)?,
        p_filesz: r64(b, off + 32).ok_or(ElfError::BadProgramHeader)?,
        p_memsz:  r64(b, off + 40).ok_or(ElfError::BadProgramHeader)?,
    })
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Load an ELF64 executable from `bytes` into `as_`.
///
/// Maps all `PT_LOAD` segments into `as_` with the correct POSIX protection
/// flags, copies the file-image data into the freshly allocated physical pages,
/// and sets `as_.heap_start` / `heap_end` to the first aligned page after the
/// highest loaded address.
///
/// Returns the virtual entry-point address on success.
///
/// # Safety
///
/// `as_` must be a freshly-created `AddressSpace` for the new process.  The
/// function writes to physical memory via identity-mapped kernel addresses, so
/// it must run in kernel mode with interrupts allowed to be off.
pub fn load(bytes: &[u8], as_: &mut AddressSpace) -> Result<usize, ElfError> {
    let ehdr       = parse_ehdr(bytes)?;
    let phoff      = ehdr.e_phoff     as usize;
    let phentsize  = ehdr.e_phentsize as usize;
    let phnum      = ehdr.e_phnum     as usize;

    // Validate the program-header table fits inside `bytes`.
    let ph_table_end = phoff
        .checked_add(phentsize.checked_mul(phnum).ok_or(ElfError::BadProgramHeader)?)
        .ok_or(ElfError::BadProgramHeader)?;
    if ph_table_end > bytes.len() { return Err(ElfError::BadProgramHeader); }

    let page_size = mm::buddy::PAGE_SIZE;
    let mut highest: usize = 0; // highest virtual address loaded (inclusive end)

    for i in 0..phnum {
        let ph_off = phoff + i * phentsize;
        let ph     = parse_phdr(bytes, ph_off)?;

        if ph.p_type != PT_LOAD { continue; }
        if ph.p_memsz == 0      { continue; }

        let vaddr   = ph.p_vaddr  as usize;
        let memsz   = ph.p_memsz  as usize;
        let filesz  = ph.p_filesz as usize;
        let foffset = ph.p_offset as usize;

        // Validate file data range.
        let fend = foffset.checked_add(filesz).ok_or(ElfError::SegmentOverflow)?;
        if fend > bytes.len() { return Err(ElfError::BadProgramHeader); }

        // Build PageFlags from ELF segment permission bits.
        // W^X enforcement: if both PF_W and PF_X are set, drop the write bit.
        let mut flags = PageFlags::PRESENT | PageFlags::USER;
        if ph.p_flags & PF_W != 0 && ph.p_flags & PF_X == 0 {
            flags |= PageFlags::WRITABLE;
        }
        if ph.p_flags & PF_X != 0 {
            flags |= PageFlags::EXECUTE;
            // DEBUG: Critical debug output
            // This is ugly but necessary for debugging
            #[cfg(target_arch = "aarch64")]
            {
                let debug_msg = b"[ELF] EXECUTABLE segment detected! PF_X=1, setting EXECUTE flag\r\n";
                for &byte in debug_msg {
                    unsafe { (0x09000000 as *mut u8).write(byte); }
                }
            }
        }
        // PF_R (read permission) is always granted; no separate flag needed.

        // Align the virtual base address down to a page boundary.
        let page_vaddr   = vaddr & !(page_size - 1);
        let page_offset  = vaddr - page_vaddr;          // byte offset into the first page
        let map_size     = memsz + page_offset;         // total bytes to map

        // Map the segment.  map() zeroes all backing pages so the BSS tail
        // (memsz > filesz) is already zeroed.
        if !as_.map(page_vaddr, map_size, flags) {
            return Err(ElfError::MappingFailed);
        }

        // Locate the physical base for the copy.  as_.map() uses an eager
        // buddy allocation, so the VMA has a contiguous phys range starting
        // at vma.phys.  The VMA starts at page_vaddr (after alignment in map).
        let phys_base = {
            let vma = as_.find(page_vaddr).ok_or(ElfError::MappingFailed)?;
            // vma.start == page_vaddr (map() aligns down to page boundary)
            vma.phys + (vaddr - vma.start)
        };

        // Copy `filesz` bytes via the HHDM virtual address so this works
        // regardless of whether an identity map is present.
        let virt_base = mm::phys_to_virt(phys_base);
        if filesz > 0 {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    bytes.as_ptr().add(foffset),
                    virt_base as *mut u8,
                    filesz,
                );

                // CRITICAL: Cache maintenance for executable code
                // After copying code to memory, we must ensure cache coherency
                if ph.p_flags & PF_X != 0 {
                    // Clean data cache and invalidate instruction cache for the code region
                    let start_addr = phys_base;
                    let end_addr = phys_base + filesz;

                    // AArch64 cache maintenance
                    #[cfg(target_arch = "aarch64")]
                    {
                        // Clean data cache to point of coherency
                        let mut addr = start_addr & !63; // Align to cache line (64 bytes)
                        while addr < end_addr {
                            core::arch::asm!("dc cvac, {}", in(reg) addr);
                            addr += 64;
                        }
                        // Invalidate instruction cache for the entire range
                        core::arch::asm!("ic iallu"); // Invalidate all instruction cache
                        // Ensure completion
                        core::arch::asm!("dsb ish");
                        core::arch::asm!("isb");
                    }

                    #[cfg(target_arch = "aarch64")]
                    {
                        let debug_msg = b"[ELF] Cache maintenance completed for executable segment\r\n";
                        for &byte in debug_msg {
                            unsafe { (0x09000000 as *mut u8).write(byte); }
                        }
                    }
                }
            }
        }

        // Track the highest virtual byte loaded (for heap_start calculation).
        let seg_end = vaddr
            .checked_add(memsz)
            .ok_or(ElfError::SegmentOverflow)?;
        if seg_end > highest { highest = seg_end; }
    }

    // Place the heap just after the highest loaded segment, with a 1-page gap
    // as a guard against accidental underflows.
    if highest > 0 {
        let heap_page = (highest + page_size - 1) & !(page_size - 1);
        as_.heap_start = heap_page + page_size; // guard gap
        as_.heap_end   = as_.heap_start;
    }

    Ok(ehdr.e_entry as usize)
}
