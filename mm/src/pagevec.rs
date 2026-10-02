//! `PageVec` — the per-VMA table of demand-faulted frames (`lazy_pages`).
//!
//! It used to be a dense `Vec<usize>` indexed by page, grown on each fault to
//! `page_idx + 1`. That made a sparse mapping pay for its *span*, not its
//! use: one touch at the top of a 2 GiB `MAP_NORESERVE` reservation (the size
//! SpiderMonkey reserves for JIT code) grew the vector to 512 Ki entries —
//! 4 MiB of kernel heap, and 8 MiB transiently while it doubled. Kernel
//! allocations that large come from physically contiguous buddy blocks, so
//! on a fragmented machine the fault path could fail an order-11 allocation
//! (and `Vec` aborts on allocation failure).
//!
//! `PageVec` keeps the same indexable interface but splits the table into
//! 512-slot chunks (2 MiB of mapping each). A chunk is an ordinary `Vec`
//! that starts empty (no allocation) and grows only up to the highest slot
//! written in it, so a small VMA costs exactly what the dense vector did and
//! a large sparse one pays for the chunks it touches, at most 4 KiB each.
//! The chunk directory is the only span-proportional part: 24 bytes per
//! 2 MiB of mapping (24 KiB for 2 GiB), sized by `resize`, which callers do
//! only up to the highest touched index.

extern crate alloc;
use alloc::vec::Vec;
use core::ops::{Index, IndexMut};

const CHUNK: usize = 512;

static ZERO: usize = 0;

#[derive(Clone, Default)]
pub struct PageVec {
    len: usize,
    /// `chunks[c][j]` is slot `c * CHUNK + j`; slots past a chunk's length
    /// (including every slot of an empty chunk) read as 0.
    chunks: Vec<Vec<usize>>,
}

impl PageVec {
    pub const fn new() -> Self { PageVec { len: 0, chunks: Vec::new() } }

    #[inline]
    pub fn len(&self) -> usize { self.len }

    #[inline]
    pub fn is_empty(&self) -> bool { self.len == 0 }

    /// `Some(&frame)` for `i < len` (0 for a never-written slot), else `None`.
    #[inline]
    pub fn get(&self, i: usize) -> Option<&usize> {
        if i >= self.len { return None; }
        Some(self.chunks[i / CHUNK].get(i % CHUNK).unwrap_or(&ZERO))
    }

    /// Grow or shrink to `n` slots. Only `0` is a valid fill value — every
    /// caller pads with "not present" — and growing allocates no chunk.
    pub fn resize(&mut self, n: usize, fill: usize) {
        debug_assert!(fill == 0);
        let _ = fill;
        if n < self.len && n % CHUNK != 0 {
            // Drop the cut-off tail of the last kept chunk, so a later regrow
            // reads zeros there as a fresh Vec would.
            if let Some(c) = self.chunks.get_mut(n / CHUNK) {
                c.truncate(n % CHUNK);
            }
        }
        self.chunks.resize_with((n + CHUNK - 1) / CHUNK, Vec::new);
        self.len = n;
    }

    /// Every slot in index order, absent ones as 0 — `Vec::iter` semantics.
    /// Walks the whole span; use [`present`](Self::present) to visit only
    /// faulted frames.
    pub fn iter(&self) -> impl Iterator<Item = &usize> + '_ {
        (0..self.len).map(move |i| self.get(i).unwrap_or(&ZERO))
    }

    /// `(index, frame)` of every non-zero slot, skipping empty chunks.
    pub fn present(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.chunks.iter().enumerate()
            .flat_map(|(ci, c)| c.iter().enumerate().map(move |(j, &p)| (ci * CHUNK + j, p)))
            .filter(|&(_, p)| p != 0)
    }

    /// Number of non-zero slots.
    pub fn count_present(&self) -> usize { self.present().count() }

    /// `Vec::split_off`: keep `[0, at)`, return `[at, len)` re-based at 0.
    pub fn split_off(&mut self, at: usize) -> PageVec {
        if at >= self.len { return PageVec::new(); }
        let mut tail = PageVec::new();
        tail.resize(self.len - at, 0);
        if at % CHUNK == 0 {
            // Chunk-aligned: move whole chunks, no copying.
            let moved = self.chunks.split_off(at / CHUNK);
            for (k, c) in moved.into_iter().enumerate() { tail.chunks[k] = c; }
        } else {
            let moving: Vec<(usize, usize)> = self.present().filter(|&(i, _)| i >= at).collect();
            for (i, p) in moving { tail[i - at] = p; }
        }
        self.resize(at, 0);
        tail
    }
}

impl From<Vec<usize>> for PageVec {
    fn from(v: Vec<usize>) -> Self {
        let mut pv = PageVec::new();
        pv.resize(v.len(), 0);
        for (i, p) in v.into_iter().enumerate() {
            if p != 0 { pv[i] = p; }
        }
        pv
    }
}

impl Index<usize> for PageVec {
    type Output = usize;
    #[inline]
    fn index(&self, i: usize) -> &usize {
        match self.get(i) {
            Some(p) => p,
            None => panic!("PageVec index {} out of range {}", i, self.len),
        }
    }
}

impl IndexMut<usize> for PageVec {
    /// Grows the slot's chunk up to the slot on first write (Vec growth
    /// within a chunk, capped at CHUNK slots).
    fn index_mut(&mut self, i: usize) -> &mut usize {
        if i >= self.len { panic!("PageVec index {} out of range {}", i, self.len); }
        let c = &mut self.chunks[i / CHUNK];
        let j = i % CHUNK;
        if c.len() <= j {
            if c.capacity() <= j {
                let want = (j + 1).next_power_of_two().min(CHUNK);
                c.reserve_exact(want - c.len());
            }
            c.resize(j + 1, 0);
        }
        &mut c[j]
    }
}
