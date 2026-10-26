//! mmap tokens of NVIDIA sysmem buffers (L11.9): which physical ranges a DRM
//! mmap may name, and whether a requested `[addr, addr + len)` lies wholly
//! inside one live buffer. No kernel or RM dependency, so drivers/uapi-tests
//! includes this file verbatim and checks it on the host.
//!
//! A mapping takes two steps in `sys_mmap`: the DRM mmap ioctl (which sees the
//! address and the calling open, not the length) and the page pin (which sees
//! the address and the length, not the open). The ioctl calls [`Tokens::authorize`]
//! after its ownership check; the pin calls [`Tokens::claim`], which validates
//! the whole range against the buffer that contains it. A buffer freed between
//! the two steps (GEM_CLOSE on another thread) leaves an authorization with no
//! live buffer behind it: `claim` refuses it, rather than reporting "not ours"
//! (which would make `sys_mmap` fall back to an unpinned device mapping of
//! freed RAM of any length).

use alloc::collections::BTreeMap;

pub const PAGE: u64 = 4096;

/// Outstanding authorizations are bounded; past this many (ioctls whose
/// mmap never reached the pin) the oldest are dropped.
const MAX_AUTH: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    /// No live buffer contains the address and none was authorized there.
    NotOurs,
    /// Inside (or authorized for) one of our buffers, but the request is
    /// refused: misaligned, empty, overflowing, running past the buffer's
    /// end, or the buffer is gone.
    Bad,
    /// `pages` 4 KiB frames from `start` (the requested address).
    Ok { start: u64, pages: u64 },
}

pub struct Tokens {
    /// Live buffers: physical base -> (size in bytes, generation).
    live: BTreeMap<u64, (u64, u64)>,
    /// (address, buffer generation) the ioctl accepted and the pin has not
    /// claimed yet. The generation tells a buffer freed and its pages handed
    /// to a new buffer in between apart from the one the ioctl checked.
    auth: BTreeMap<(u64, u64), u32>,
    next_gen: u64,
}

impl Tokens {
    pub const fn new() -> Self {
        Tokens { live: BTreeMap::new(), auth: BTreeMap::new(), next_gen: 1 }
    }

    pub fn insert(&mut self, base: u64, size: u64) {
        self.next_gen += 1;
        self.live.insert(base, (size, self.next_gen));
    }

    pub fn remove(&mut self, base: u64) {
        self.live.remove(&base);
    }

    /// The live buffer `(base, size)` containing `addr`.
    pub fn containing(&self, addr: u64) -> Option<(u64, u64)> {
        self.containing_gen(addr).map(|(b, s, _)| (b, s))
    }

    fn containing_gen(&self, addr: u64) -> Option<(u64, u64, u64)> {
        let (&base, &(size, gen)) = self.live.range(..=addr).next_back()?;
        if addr - base < size { Some((base, size, gen)) } else { None }
    }

    /// Validate `[addr, addr + len)` (len rounded up to whole pages) against
    /// the live buffer containing `addr`.
    pub fn check(&self, addr: u64, len: u64) -> Range {
        let Some((base, size)) = self.containing(addr) else { return Range::NotOurs };
        if addr % PAGE != 0 || len == 0 { return Range::Bad; }
        let Some(rounded) = len.checked_add(PAGE - 1).map(|l| l & !(PAGE - 1)) else { return Range::Bad };
        let (Some(end), Some(buf_end)) = (addr.checked_add(rounded), base.checked_add(size)) else { return Range::Bad };
        if end > buf_end { return Range::Bad; }
        Range::Ok { start: addr, pages: rounded / PAGE }
    }

    /// The ioctl accepted `addr` for some open (ownership of the buffer
    /// containing it checked by the caller). False if no live buffer contains it.
    pub fn authorize(&mut self, addr: u64) -> bool {
        let Some((_, _, gen)) = self.containing_gen(addr) else { return false };
        if self.auth.len() >= MAX_AUTH {
            if let Some((&k, _)) = self.auth.iter().next() { self.auth.remove(&k); }
        }
        *self.auth.entry((addr, gen)).or_insert(0) += 1;
        true
    }

    /// The pin step: consume one authorization of `addr`, then validate the
    /// range. An authorized address with no live buffer behind it any more is
    /// `Bad`, never `NotOurs`, and so is one whose buffer was freed and its
    /// address handed to a new buffer since the ioctl checked it.
    pub fn claim(&mut self, addr: u64, len: u64) -> Range {
        let gen = self.containing_gen(addr).map(|(_, _, g)| g);
        // Prefer the authorization for the live buffer; else any for `addr`.
        let key = gen.map(|g| (addr, g)).filter(|k| self.auth.contains_key(k))
            .or_else(|| self.auth.range((addr, 0)..=(addr, u64::MAX)).next().map(|(&k, _)| k));
        let Some(key) = key else {
            // Not authorized by the ioctl (kernel-internal caller): range check only.
            return self.check(addr, len);
        };
        let n = self.auth.get_mut(&key).unwrap();
        *n -= 1;
        if *n == 0 { self.auth.remove(&key); }
        if Some(key.1) != gen { return Range::Bad; } // freed (and maybe reused) since the ioctl
        self.check(addr, len)
    }

    #[allow(dead_code)] // host tests
    pub fn pending(&self) -> usize {
        self.auth.values().map(|&n| n as usize).sum()
    }
}
