//! Staging: safe page allocator and the Linux-style indirection list.
//!
//! Entries are u64 words, page-aligned physical address in the high bits and
//! a flag in the low bits (Linux `kexec.h`): DESTINATION sets the copy
//! cursor, INDIRECTION chains to the next entry page, DONE ends the list,
//! SOURCE copies one 4 KiB page to the cursor and advances it.
//!
//! No source or indirection page may lie inside any destination range (or
//! the extra forbidden set), so the later forward copy can never overwrite
//! a page it has yet to read. Rejected pages stay allocated until the load
//! is finished.

use alloc::vec::Vec;

use super::{EFAULT, EIO, ENOMEM};
use crate::syscall::{read_fd_upto, sys_lseek_pub};

pub const IND_DESTINATION: u64 = 0x1;
pub const IND_INDIRECTION: u64 = 0x2;
pub const IND_DONE: u64 = 0x4;
pub const IND_SOURCE: u64 = 0x8;

const PAGE: usize = 4096;
const ENTRIES: usize = PAGE / 8 - 1; // last word of a page is the link

/// Does `[a, a+len)` intersect any of `ranges`?
pub fn overlaps(ranges: &[(u64, u64)], a: u64, len: u64) -> bool {
    ranges.iter().any(|&(s, e)| a < e && s < a + len)
}

pub struct Alloc {
    /// Ranges (start, end) no allocation may touch.
    pub forbidden: Vec<(u64, u64)>,
    /// Pages kept allocated while loading because they were forbidden.
    held: Vec<(usize, usize)>,
    /// Blocks (addr, order) the image owns.
    pub owned: Vec<(usize, usize)>,
}

impl Alloc {
    pub fn new(forbidden: Vec<(u64, u64)>) -> Self {
        Alloc { forbidden, held: Vec::new(), owned: Vec::new() }
    }

    /// A zeroed, owned block of 2^order pages clear of the forbidden set.
    /// `limit` (exclusive end) bounds the address via `alloc_below`.
    pub fn block(&mut self, order: usize, limit: Option<usize>) -> Result<usize, isize> {
        let len = (PAGE << order) as u64;
        loop {
            let a = match limit {
                Some(l) => mm::buddy::alloc_below(order, l),
                None => mm::buddy::alloc(order),
            }.ok_or(ENOMEM)?;
            if overlaps(&self.forbidden, a as u64, len) {
                self.held.push((a, order));
                continue;
            }
            unsafe { core::ptr::write_bytes(mm::phys_to_virt(a) as *mut u8, 0, PAGE << order); }
            self.owned.push((a, order));
            return Ok(a);
        }
    }

    /// Like [`block`], but the block must start at or above `min` (blocks
    /// below it are held until `release_held`).
    pub fn block_above(&mut self, order: usize, limit: Option<usize>, min: u64) -> Result<usize, isize> {
        let len = (PAGE << order) as u64;
        loop {
            let a = match limit {
                Some(l) => mm::buddy::alloc_below(order, l),
                None => mm::buddy::alloc(order),
            }.ok_or(ENOMEM)?;
            if (a as u64) < min || overlaps(&self.forbidden, a as u64, len) {
                self.held.push((a, order));
                continue;
            }
            unsafe { core::ptr::write_bytes(mm::phys_to_virt(a) as *mut u8, 0, PAGE << order); }
            self.owned.push((a, order));
            return Ok(a);
        }
    }

    /// Free the rejected pages (success path) .
    pub fn release_held(&mut self) {
        for (a, o) in self.held.drain(..) { mm::buddy::free(a, o); }
    }

    /// Free everything (failure path or unload).
    pub fn release_all(&mut self) {
        self.release_held();
        for (a, o) in self.owned.drain(..) { mm::buddy::free(a, o); }
    }
}

/// Builder for the indirection list.
pub struct List {
    pub head: u64,
    cur: usize, // phys of the entry page being filled
    idx: usize,
    pub sources: usize,
    pub ind_pages: usize,
}

impl List {
    pub fn new(a: &mut Alloc) -> Result<Self, isize> {
        let p = a.block(0, None)?;
        Ok(List { head: p as u64, cur: p, idx: 0, sources: 0, ind_pages: 1 })
    }

    fn word(&self, i: usize) -> *mut u64 {
        (mm::phys_to_virt(self.cur) as *mut u64).wrapping_add(i)
    }

    fn push(&mut self, a: &mut Alloc, e: u64) -> Result<(), isize> {
        if self.idx == ENTRIES {
            let n = a.block(0, None)?;
            unsafe { self.word(ENTRIES).write(n as u64 | IND_INDIRECTION); }
            self.cur = n;
            self.idx = 0;
            self.ind_pages += 1;
        }
        unsafe { self.word(self.idx).write(e); }
        self.idx += 1;
        Ok(())
    }

    pub fn destination(&mut self, a: &mut Alloc, dest: u64) -> Result<(), isize> {
        self.push(a, dest | IND_DESTINATION)
    }

    pub fn source(&mut self, a: &mut Alloc, src: usize) -> Result<(), isize> {
        self.sources += 1;
        self.push(a, src as u64 | IND_SOURCE)
    }

    pub fn done(&mut self, a: &mut Alloc) -> Result<(), isize> {
        self.push(a, IND_DONE)
    }
}

/// Streams segments into source pages. A segment need not be page aligned,
/// and consecutive segments may share a page (the kernel's rodata and data
/// segments do): the open page is carried over and the second segment's bytes
/// are written into it, the gap between them staying zero.
pub struct Stager {
    cur: Option<(usize, u64)>, // (source page, destination page base)
    pos: usize,                // write position within `cur`
    next_dest: u64,            // destination the list cursor is at after the last source
}

impl Stager {
    pub fn new() -> Self { Stager { cur: None, pos: 0, next_dest: u64::MAX } }

    fn flush(&mut self, a: &mut Alloc, l: &mut List) -> Result<(), isize> {
        if let Some((p, d)) = self.cur.take() {
            l.source(a, p)?;
            self.next_dest = d + PAGE as u64;
        }
        self.pos = 0;
        Ok(())
    }

    /// Stage `filesz` bytes of `fd` at file offset `offset`, to be loaded at
    /// `paddr`. Segments must be fed in ascending `paddr` order.
    pub fn file(&mut self, a: &mut Alloc, l: &mut List, fd: usize, offset: u64, filesz: u64, paddr: u64)
        -> Result<(), isize>
    {
        if filesz == 0 { return Ok(()); }
        let r = sys_lseek_pub(fd, offset as usize, 0);
        if r < 0 { return Err(r); }
        let base = paddr & !0xfff;
        match self.cur {
            Some((_, d)) if d == base && self.pos <= (paddr & 0xfff) as usize => {}
            _ => {
                self.flush(a, l)?;
                if self.next_dest != base { l.destination(a, base)?; self.next_dest = base; }
            }
        }
        self.pos = (paddr & 0xfff) as usize;
        let mut dest = base; // base of the page `cur` represents
        if let Some((_, d)) = self.cur { dest = d; }
        const CHUNK: usize = 16 * PAGE;
        let mut buf = alloc::vec![0u8; CHUNK];
        let mut left = filesz as usize;
        while left > 0 {
            let want = left.min(CHUNK);
            let n = read_fd_upto(fd, &mut buf[..want]);
            if n < 0 { return Err(n); }
            if n as usize != want { return Err(EIO); }
            let mut off = 0;
            while off < want {
                let p = match self.cur {
                    Some((p, _)) => p,
                    None => { let p = a.block(0, None)?; self.cur = Some((p, dest)); p }
                };
                let take = (want - off).min(PAGE - self.pos);
                let dst = mm::phys_to_virt(p) as *mut u8;
                if dst.is_null() { return Err(EFAULT); }
                unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr().add(off), dst.add(self.pos), take); }
                off += take;
                self.pos += take;
                if self.pos == PAGE {
                    self.flush(a, l)?;
                    dest += PAGE as u64;
                }
            }
            left -= want;
        }
        Ok(())
    }

    /// Close the open page (call after the last segment of a destination).
    pub fn finish(&mut self, a: &mut Alloc, l: &mut List) -> Result<(), isize> { self.flush(a, l) }
}
