//! Memory allocation — program-break allocator with size-binned free lists.
//! Freed blocks are reused; memory is never returned to the OS.

use core::sync::atomic::{AtomicUsize, Ordering};
use crate::syscall::{nr, syscall1};

use crate::io::size_t;

// Current program break. 0 = not yet initialised.
static HEAP_PTR: AtomicUsize = AtomicUsize::new(0);

/// Set (or query) the program break.
unsafe fn brk(addr: usize) -> usize {
    syscall1(nr::BRK, addr) as usize
}

/// Initialise the heap by querying the current break.
unsafe fn heap_init() -> usize {
    let start = brk(0);
    HEAP_PTR.store(start, Ordering::Relaxed);
    start
}

/// Bytes of block (header included) are always a multiple of 16. The header
/// word holds the block's usable capacity (block size - 8).
const SMALL_MAX: usize = 4096;                 // exact-size bins up to here
const NBINS: usize = SMALL_MAX / 16 + 1;

struct FreeLists {
    bins: [usize; NBINS],   // head of singly linked list per block size / 16
    large: usize,           // single list for blocks > SMALL_MAX
}

static LOCK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static mut FREE: FreeLists = FreeLists { bins: [0; NBINS], large: 0 };

fn lock() {
    while LOCK.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        core::hint::spin_loop();
    }
}
fn unlock() { LOCK.store(false, Ordering::Release); }

/// Allocate `size` bytes. Returns a pointer, or NULL on OOM. Freed blocks are
/// reused: exact-size bins for small blocks, first fit (at most 2x oversize)
/// for large ones; fresh memory comes from the program break.
#[no_mangle]
pub unsafe extern "C" fn malloc(size: size_t) -> *mut u8 {
    if size == 0 { return core::ptr::null_mut(); }
    let alloc_size = match size.checked_add(8 + 15) {
        Some(v) => v & !15,
        None => { crate::errno::set_errno(crate::errno::ENOMEM); return core::ptr::null_mut(); }
    };
    lock();
    let free = &mut *core::ptr::addr_of_mut!(FREE);
    if alloc_size <= SMALL_MAX {
        let head = free.bins[alloc_size / 16];
        if head != 0 {
            free.bins[alloc_size / 16] = *((head + 8) as *const usize);
            unlock();
            return (head + 8) as *mut u8;
        }
    } else {
        let mut prev: *mut usize = &mut free.large;
        let mut cur = free.large;
        while cur != 0 {
            let blk = *(cur as *const usize) + 8;
            if blk >= alloc_size && blk <= alloc_size * 2 {
                *prev = *((cur + 8) as *const usize);
                unlock();
                return (cur + 8) as *mut u8;
            }
            prev = (cur + 8) as *mut usize;
            cur = *prev;
        }
    }
    let mut ptr = HEAP_PTR.load(Ordering::Relaxed);
    if ptr == 0 { ptr = heap_init(); }
    let new_ptr = ptr + alloc_size;
    let actual  = brk(new_ptr);
    if actual < new_ptr {
        unlock();
        crate::errno::set_errno(crate::errno::ENOMEM);
        return core::ptr::null_mut();
    }
    // Header = usable capacity, needed by realloc/free.
    *(ptr as *mut usize) = alloc_size - 8;
    HEAP_PTR.store(actual, Ordering::Relaxed);
    unlock();
    (ptr + 8) as *mut u8
}

/// Allocate `size` zero-initialised bytes.
#[no_mangle]
pub unsafe extern "C" fn calloc(nmemb: size_t, size: size_t) -> *mut u8 {
    let total = match nmemb.checked_mul(size) { Some(t) => t, None => {
        crate::errno::set_errno(crate::errno::ENOMEM);
        return core::ptr::null_mut();
    } };
    let p = malloc(total);
    if !p.is_null() { memset(p, 0, total); }
    p
}

/// Resize an allocation: in place when the block is already big enough,
/// otherwise allocate, copy and free the old block.
#[no_mangle]
pub unsafe extern "C" fn realloc(ptr: *mut u8, new_size: size_t) -> *mut u8 {
    if ptr.is_null() { return malloc(new_size); }
    let old_cap = *((ptr as usize - 8) as *const usize);
    if new_size <= old_cap { return ptr; }
    let new_ptr = malloc(new_size);
    if !new_ptr.is_null() {
        memcpy(new_ptr, ptr, old_cap);
        free(ptr);
    }
    new_ptr
}

/// Return a block to its free list.
#[no_mangle]
pub unsafe extern "C" fn free(ptr: *mut u8) {
    if ptr.is_null() { return; }
    let blk = ptr as usize - 8;
    let alloc_size = *(blk as *const usize) + 8;
    lock();
    let free = &mut *core::ptr::addr_of_mut!(FREE);
    let head: *mut usize = if alloc_size <= SMALL_MAX {
        &mut free.bins[alloc_size / 16]
    } else {
        &mut free.large
    };
    *((blk + 8) as *mut usize) = *head;
    *head = blk;
    unlock();
}

// ── Global Allocator ─────────────────────────────────────────────────────────

struct LibcAllocator;

unsafe impl core::alloc::GlobalAlloc for LibcAllocator {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        malloc(layout.size())
    }
    unsafe fn dealloc(&self, ptr: *mut u8, _layout: core::alloc::Layout) {
        free(ptr)
    }
}

#[global_allocator]
static ALLOCATOR: LibcAllocator = LibcAllocator;

// ── Core memory operations (also used by string.rs) ──────────────────────────

// These three are the libc's only bulk-copy primitives: every C program
// linked against this libc (DOOM, ports) and `string.rs` funnel through them.
// They used to be byte-at-a-time loops (`movb`/`ldrb` per byte, the compiler
// will not vectorise or idiom-replace a loop inside a function *named*
// memcpy), which capped a 256 KiB wl_shm frame copy at ~600 MB/s on the
// laptop (lane shmslow). x86_64 now uses the fast-string `rep movsq`/`rep stosq`
// plus a byte tail; other arches copy whole
// machine words with an unrolled loop.
//
// Do NOT move the word loops into helper functions with other names: LLVM's
// loop-idiom pass only stays away from functions called memcpy/memset, and a
// helper's loop would be rewritten into a call to memcpy -- i.e. into itself.

const W: usize = core::mem::size_of::<usize>();

/// Copy `n` bytes from `src` to `dst` (non-overlapping).
#[no_mangle]
pub unsafe extern "C" fn memcpy(dst: *mut u8, src: *const u8, n: size_t) -> *mut u8 {
    #[cfg(target_arch = "x86_64")]
    {
        // Quadwords then the tail: as fast as ERMSB `rep movsb` on hardware,
        // and 8x fewer iterations under TCG, which emulates string ops per
        // element (plain `rep movsb` there was slower than the byte loop).
        core::arch::asm!("rep movsq", "mov rcx, {t}", "rep movsb",
            t = in(reg) n & 7,
            inout("rcx") n >> 3 => _, inout("rdi") dst => _, inout("rsi") src => _,
            options(nostack, preserves_flags));
        return dst;
    }
    #[cfg(target_arch = "aarch64")]
    {
        // Inline asm, not read_unaligned: aarch64-unknown-none is built
        // +strict-align, so the compiler splits every unaligned word access
        // into bytes. EL0 Normal memory takes unaligned ldp/stp fine.
        core::arch::asm!(
            "cmp {n}, #64", "b.lo 2f",
            "1:",
            "ldp {a}, {b}, [{s}], #16", "ldp {c}, {d}, [{s}], #16",
            "stp {a}, {b}, [{d0}], #16", "stp {c}, {d}, [{d0}], #16",
            "ldp {a}, {b}, [{s}], #16", "ldp {c}, {d}, [{s}], #16",
            "stp {a}, {b}, [{d0}], #16", "stp {c}, {d}, [{d0}], #16",
            "sub {n}, {n}, #64", "cmp {n}, #64", "b.hs 1b",
            "2:", "cmp {n}, #8", "b.lo 3f",
            "ldr {a}, [{s}], #8", "str {a}, [{d0}], #8", "sub {n}, {n}, #8", "b 2b",
            "3:", "cbz {n}, 4f",
            "ldrb {a:w}, [{s}], #1", "strb {a:w}, [{d0}], #1", "sub {n}, {n}, #1", "b 3b",
            "4:",
            n = inout(reg) n => _, s = inout(reg) src => _, d0 = inout(reg) dst => _,
            a = out(reg) _, b = out(reg) _, c = out(reg) _, d = out(reg) _,
            options(nostack));
        return dst;
    }
    #[allow(unreachable_code)]
    {
        for i in 0..n { *dst.add(i) = *src.add(i); }
        dst
    }
}

/// Copy `n` bytes from `src` to `dst` (may overlap).
#[no_mangle]
pub unsafe extern "C" fn memmove(dst: *mut u8, src: *const u8, n: size_t) -> *mut u8 {
    if (dst as usize) <= (src as usize) || (dst as usize) >= (src as usize + n) {
        // A forward copy is safe when dst is below src: memcpy reads every
        // byte (or word) before it writes anything at a higher address than
        // it has read, for both `rep movsb` and the ascending word loop.
        memcpy(dst, src, n)
    } else {
        // Overlap with dst above src: copy descending, whole words first.
        // Volatile accesses keep the loop from being idiom-rewritten into a
        // call to memmove (this function).
        let mut i = n;
        while i >= W {
            i -= W;
            let v = core::ptr::read_volatile(src.add(i) as *const [u8; W]);
            core::ptr::write_volatile(dst.add(i) as *mut [u8; W], v);
        }
        while i > 0 { i -= 1; core::ptr::write_volatile(dst.add(i), core::ptr::read_volatile(src.add(i))); }
        dst
    }
}

/// Fill `n` bytes at `s` with byte value `c`.
#[no_mangle]
pub unsafe extern "C" fn memset(s: *mut u8, c: i32, n: size_t) -> *mut u8 {
    #[cfg(target_arch = "x86_64")]
    {
        core::arch::asm!("rep stosq", "mov rcx, {t}", "rep stosb",
            t = in(reg) n & 7,
            inout("rcx") n >> 3 => _, inout("rdi") s => _,
            in("rax") (c as u8 as u64).wrapping_mul(0x0101_0101_0101_0101),
            options(nostack, preserves_flags));
        return s;
    }
    #[cfg(target_arch = "aarch64")]
    {
        let v = (c as u8 as u64).wrapping_mul(0x0101_0101_0101_0101);
        core::arch::asm!(
            "cmp {n}, #64", "b.lo 2f",
            "1:",
            "stp {v}, {v}, [{p}], #16", "stp {v}, {v}, [{p}], #16",
            "stp {v}, {v}, [{p}], #16", "stp {v}, {v}, [{p}], #16",
            "sub {n}, {n}, #64", "cmp {n}, #64", "b.hs 1b",
            "2:", "cmp {n}, #8", "b.lo 3f",
            "str {v}, [{p}], #8", "sub {n}, {n}, #8", "b 2b",
            "3:", "cbz {n}, 4f",
            "strb {v:w}, [{p}], #1", "sub {n}, {n}, #1", "b 3b",
            "4:",
            n = inout(reg) n => _, p = inout(reg) s => _, v = in(reg) v,
            options(nostack));
        return s;
    }
    #[allow(unreachable_code)]
    {
        for i in 0..n { *s.add(i) = c as u8; }
        s
    }
}

/// Map memory into the process address space.
#[no_mangle]
pub unsafe extern "C" fn mmap(
    addr: *mut u8, len: size_t, prot: i32, flags: i32, fd: i32, offset: i64,
) -> *mut u8 {
    let r = crate::syscall::syscall6(
        nr::MMAP,
        addr as usize,
        len,
        prot as usize,
        flags as usize,
        fd as usize,
        offset as usize,
    );
    if r < 0 {
        crate::errno::set_errno(-r as i32);
        return (-1isize) as *mut u8;
    }
    r as *mut u8
}

/// Resize a mapping in place or (if it must move) to a new address,
/// preserving the overlapping content.
#[no_mangle]
pub unsafe extern "C" fn mremap(
    old_addr: *mut u8, old_size: size_t, new_size: size_t, flags: i32,
) -> *mut u8 {
    let r = crate::syscall::syscall5(
        nr::MREMAP,
        old_addr as usize,
        old_size,
        new_size,
        flags as usize,
        0,
    );
    if r < 0 {
        crate::errno::set_errno(-r as i32);
        return (-1isize) as *mut u8;
    }
    r as *mut u8
}

/// Unmap memory from the process address space.
#[no_mangle]
pub unsafe extern "C" fn munmap(addr: *mut u8, len: size_t) -> i32 {
    let r = crate::syscall::syscall2(nr::MUNMAP, addr as usize, len);
    if r < 0 {
        crate::errno::set_errno(-r as i32);
        -1
    } else {
        0
    }
}

/// Compare `n` bytes of `a` and `b`.
#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: size_t) -> i32 {
    for i in 0..n {
        let d = *a.add(i) as i32 - *b.add(i) as i32;
        if d != 0 { return d; }
    }
    0
}

/// Find byte `c` in the first `n` bytes of `s`.
#[no_mangle]
pub unsafe extern "C" fn memchr(s: *const u8, c: i32, n: size_t) -> *mut u8 {
    for i in 0..n {
        if *s.add(i) == c as u8 { return s.add(i) as *mut u8; }
    }
    core::ptr::null_mut()
}
