//! `request_firmware`: read a whole file by absolute path from the mounted
//! root filesystem, from kernel code, into a page list (no large contiguous
//! allocation: a 63 MB GSP image becomes ~15 k scattered 4 KiB pages).
//!
//! The filesystem lives above this crate (`f2fs-server` depends on
//! `drivers`), so the actual reader is a backend the kernel registers at init
//! ([`set_backend`], implemented in `kernel/src/firmware.rs` on top of the
//! f2fs mount: open / pread / close by port — no fd table, no user memory,
//! root credentials in a kernel thread, the caller's credentials in a
//! syscall). Paths are absolute and resolved against the real root, which
//! exists only after userspace init's `pivot_root`; before that every call
//! returns `FwError::NoRoot` ([`root_ready`] tells).
//!
//! Context: task context (a kernel thread or a syscall/ioctl), IRQs masked
//! as usual; never from IRQ handlers, never while holding a spinlock (the
//! read holds the f2fs mount lock and does block I/O).
//!
//! The data can be consumed three ways:
//! * [`Firmware::read_at`] / [`Firmware::chunks`]: copy out / iterate;
//! * [`Firmware::pages`]: the physical page list (e.g. to build GSP radix3
//!   tables directly; page `i` holds bytes `i*4096 ..`);
//! * [`Firmware::map`]: one contiguous kernel VA over the pages (`mm::kva::vmap`,
//!   write-back) — what RM's `nv_get_firmware` wants to return.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

const PAGE: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FwError {
    /// No backend registered or the real root is not mounted yet.
    NoRoot,
    /// Errno from the filesystem (negative, e.g. -2 ENOENT).
    Fs(i32),
    /// Out of memory for the page list / mapping.
    NoMemory,
    /// File larger than the caller's limit.
    TooBig,
}

/// The filesystem side, registered by the kernel crate.
pub struct FirmwareBackend {
    /// True once the real root filesystem is mounted at "/".
    pub root_ready: fn() -> bool,
    /// Size in bytes of the file at `path`.
    pub size: fn(path: &str) -> Result<u64, i32>,
    /// Stream the file: `sink(offset, bytes)` for consecutive chunks; return
    /// false from `sink` to stop early. Returns the bytes delivered.
    pub read: fn(path: &str, sink: &mut dyn FnMut(u64, &[u8]) -> bool) -> Result<u64, i32>,
}

static BACKEND: AtomicUsize = AtomicUsize::new(0);

/// Register the backend (kernel init, once).
pub fn set_backend(b: &'static FirmwareBackend) {
    BACKEND.store(b as *const FirmwareBackend as usize, Ordering::Release);
}

fn backend() -> Option<&'static FirmwareBackend> {
    let p = BACKEND.load(Ordering::Acquire);
    if p == 0 { None } else { Some(unsafe { &*(p as *const FirmwareBackend) }) }
}

/// True once `request_firmware` can find files on the real root.
pub fn root_ready() -> bool { backend().map_or(false, |b| (b.root_ready)()) }

/// Size of the file at `path`.
pub fn firmware_size(path: &str) -> Result<u64, FwError> {
    let b = backend().ok_or(FwError::NoRoot)?;
    if !(b.root_ready)() { return Err(FwError::NoRoot); }
    (b.size)(path).map_err(FwError::Fs)
}

/// Stream the file through `sink(offset, bytes)` without keeping it (no
/// allocation beyond the backend's 1 MiB bounce buffer). Returns its size.
pub fn read_firmware_chunked(path: &str, sink: &mut dyn FnMut(u64, &[u8])) -> Result<u64, FwError> {
    let b = backend().ok_or(FwError::NoRoot)?;
    if !(b.root_ready)() { return Err(FwError::NoRoot); }
    (b.read)(path, &mut |off, d| { sink(off, d); true }).map_err(FwError::Fs)
}

/// A loaded file: a list of 4 KiB physical pages plus the length. Freed on
/// drop (the mapping too, if `map` made one).
pub struct Firmware {
    len: usize,
    pages: Vec<u64>,
    /// Buddy blocks to free: (phys, order).
    blocks: Vec<(usize, usize)>,
    map_va: usize,
}

/// Load the whole file at `path` (absolute, on the real root). `max_len`
/// guards against surprises (`usize::MAX` = no limit). Task context.
pub fn request_firmware(path: &str, max_len: usize) -> Result<Firmware, FwError> {
    let b = backend().ok_or(FwError::NoRoot)?;
    if !(b.root_ready)() { return Err(FwError::NoRoot); }
    let size = (b.size)(path).map_err(FwError::Fs)? as usize;
    if size > max_len { return Err(FwError::TooBig); }
    let npages = (size + PAGE - 1) / PAGE;
    let mut fw = Firmware { len: size, pages: Vec::new(), blocks: Vec::new(), map_va: 0 };
    fw.pages.try_reserve_exact(npages).map_err(|_| FwError::NoMemory)?;
    // 64 KiB blocks where possible (fewer buddy calls), 4 KiB otherwise.
    while fw.pages.len() < npages {
        let left = npages - fw.pages.len();
        let (phys, order) = match if left >= 16 { mm::buddy::alloc(4).map(|p| (p, 4)) } else { None } {
            Some(x) => x,
            None => match mm::buddy::alloc(0) { Some(p) => (p, 0), None => return Err(FwError::NoMemory) },
        };
        fw.blocks.push((phys, order));
        for i in 0..(1usize << order) { fw.pages.push((phys + i * PAGE) as u64); }
    }
    let pages = &fw.pages;
    let got = (b.read)(path, &mut |off, data| {
        let mut done = 0usize;
        while done < data.len() {
            let pos = off as usize + done;
            if pos >= size { return false; }
            let in_pg = pos % PAGE;
            let take = (PAGE - in_pg).min(data.len() - done).min(size - pos);
            let dst = mm::phys_to_virt(pages[pos / PAGE] as usize) + in_pg;
            unsafe { core::ptr::copy_nonoverlapping(data.as_ptr().add(done), dst as *mut u8, take); }
            done += take;
        }
        true
    }).map_err(FwError::Fs)?;
    if (got as usize) < size { return Err(FwError::Fs(-5)); }
    Ok(fw)
}

impl Firmware {
    /// File length in bytes.
    pub fn len(&self) -> usize { self.len }
    pub fn is_empty(&self) -> bool { self.len == 0 }
    /// Physical address of every 4 KiB page, in file order.
    pub fn pages(&self) -> &[u64] { &self.pages }

    /// Copy `buf.len()` bytes from file offset `off` (clamped at EOF).
    /// Returns the bytes copied.
    pub fn read_at(&self, off: usize, buf: &mut [u8]) -> usize {
        if off >= self.len { return 0; }
        let n = buf.len().min(self.len - off);
        let mut done = 0;
        while done < n {
            let pos = off + done;
            let pg = pos / PAGE;
            let in_pg = pos % PAGE;
            let take = (PAGE - in_pg).min(n - done);
            let src = mm::phys_to_virt(self.pages[pg] as usize) + in_pg;
            unsafe { core::ptr::copy_nonoverlapping(src as *const u8, buf.as_mut_ptr().add(done), take); }
            done += take;
        }
        n
    }

    /// The file as consecutive slices of at most one page each.
    pub fn chunks(&self) -> impl Iterator<Item = &[u8]> + '_ {
        let len = self.len;
        self.pages.iter().enumerate().map(move |(i, &p)| {
            let n = (len - i * PAGE).min(PAGE);
            unsafe { core::slice::from_raw_parts(mm::phys_to_virt(p as usize) as *const u8, n) }
        })
    }

    /// Map the pages contiguously (write-back) and return the base VA; the
    /// mapping lives until drop. Idempotent.
    pub fn map(&mut self) -> Option<usize> {
        if self.map_va != 0 { return Some(self.map_va); }
        if self.pages.is_empty() { return None; }
        self.map_va = mm::kva::vmap(&self.pages, mm::kva::CacheMode::WriteBack)?;
        Some(self.map_va)
    }

    /// Copy into one contiguous Vec (only sensible for small files).
    pub fn to_vec(&self) -> Option<Vec<u8>> {
        let mut v = Vec::new();
        v.try_reserve_exact(self.len).ok()?;
        for c in self.chunks() { v.extend_from_slice(c); }
        Some(v)
    }
}

impl Drop for Firmware {
    fn drop(&mut self) {
        if self.map_va != 0 { mm::kva::vunmap(self.map_va); }
        for &(p, o) in &self.blocks { mm::buddy::free(p, o); }
    }
}

// ── C ABI ────────────────────────────────────────────────────────────────────

/// Load `path` (NUL-terminated, absolute). On success returns an opaque
/// handle and stores the length and a contiguous kernel mapping of the data
/// in `*len_out` / `*data_out`. NULL on failure. Task context.
#[no_mangle]
pub unsafe extern "C" fn kfacil_request_firmware(path: *const u8, len_out: *mut usize, data_out: *mut *const u8) -> *mut Firmware {
    if path.is_null() { return core::ptr::null_mut(); }
    let mut n = 0;
    while n < 1024 && *path.add(n) != 0 { n += 1; }
    let p = match core::str::from_utf8(core::slice::from_raw_parts(path, n)) { Ok(s) => s, Err(_) => return core::ptr::null_mut() };
    let mut fw = match request_firmware(p, usize::MAX) { Ok(f) => f, Err(_) => return core::ptr::null_mut() };
    let va = if fw.is_empty() { 0 } else { match fw.map() { Some(v) => v, None => return core::ptr::null_mut() } };
    if !len_out.is_null() { *len_out = fw.len(); }
    if !data_out.is_null() { *data_out = va as *const u8; }
    alloc::boxed::Box::into_raw(alloc::boxed::Box::new(fw))
}

/// Release a handle from `kfacil_request_firmware` (unmaps, frees pages).
#[no_mangle]
pub unsafe extern "C" fn kfacil_release_firmware(fw: *mut Firmware) {
    if !fw.is_null() { drop(alloc::boxed::Box::from_raw(fw)); }
}
