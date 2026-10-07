//! IRQ-safe allocation: a small preallocated pool that never blocks and never
//! calls into the buddy allocator after boot.
//!
//! Why: the slab and buddy allocators take plain spin locks. Nearly all kernel
//! code runs with IRQs masked, so an IRQ normally cannot interrupt a holder
//! on its own CPU — but the boot path and IRQ-window sections can, and then an
//! allocation from the interrupt handler spins on a lock its own CPU holds.
//! The pool's lock is only ever taken with IRQs masked, so that cannot happen.
//!
//! * One contiguous buddy block (256 KiB, order 6) carved at `init` into
//!   per-size-class free lists: 32 B ×512, 64 ×512, 128 ×256, 256 ×128,
//!   512 ×64, 1 KiB ×32, 2 KiB ×16, 4 KiB ×12 (≈ 216 KiB used). Every object
//!   is naturally aligned to its class size.
//! * `alloc` returns None when the class (and every larger class) is empty;
//!   it never falls back to anything that can block. Callers in IRQ context
//!   must handle failure. Each exhausted class logs once.
//! * `free` of a pool pointer is O(1) from any context; `owns` tells pool
//!   pointers apart (address range check).
//! * The global allocator uses it automatically for allocations made in hard
//!   IRQ context (`set_irq_query` installs the context test), falling back to
//!   the ordinary slab path only if the pool is exhausted — so behaviour can
//!   only improve over what IRQ-context allocation did before. `dealloc`
//!   returns pool pointers here whatever the context.
//!
//! Limits (document for the RM shim): ≤ 4 KiB per allocation from the pool;
//! ~216 KiB total; no growth. Larger interrupt-context allocations should be
//! preallocated by the driver in task context.

use core::sync::atomic::{AtomicBool, AtomicUsize, AtomicU32, Ordering};
use spin::Mutex;

const CLASSES: [(usize, usize); 8] = [
    (32, 512), (64, 512), (128, 256), (256, 128), (512, 64), (1024, 32), (2048, 16), (4096, 12),
];
const NCLASS: usize = CLASSES.len();
const POOL_ORDER: usize = 6; // 256 KiB

struct Lists {
    head: [usize; NCLASS],
    free: [u32; NCLASS],
}

static LISTS: Mutex<Lists> = Mutex::new(Lists { head: [0; NCLASS], free: [0; NCLASS] });
static BASE: AtomicUsize = AtomicUsize::new(0);
static END: AtomicUsize = AtomicUsize::new(0);
/// Class boundaries (start VA of each class region) for `free`.
static CLASS_START: [AtomicUsize; NCLASS + 1] = [const { AtomicUsize::new(0) }; NCLASS + 1];
static EXHAUSTED_WARNED: [AtomicBool; NCLASS] = [const { AtomicBool::new(false) }; NCLASS];
static FAILS: AtomicU32 = AtomicU32::new(0);
static IRQ_QUERY: AtomicUsize = AtomicUsize::new(0);

extern "C" {
    fn arch_interrupt_save() -> usize;
    fn arch_interrupt_restore(flags: usize);
    fn arch_serial_putc(c: u8);
}

fn log(s: &str) { for &b in s.as_bytes() { unsafe { arch_serial_putc(b) } } }

/// Carve the pool. Called from `mm::init_with_map` after the buddy allocator.
pub fn init() {
    let phys = match crate::buddy::alloc(POOL_ORDER) { Some(p) => p, None => return };
    let base = crate::phys_to_virt(phys);
    let mut l = LISTS.lock();
    let mut off = 0usize;
    for (i, &(sz, n)) in CLASSES.iter().enumerate() {
        off = (off + sz - 1) & !(sz - 1);
        CLASS_START[i].store(base + off, Ordering::Relaxed);
        let mut head = 0usize;
        for k in (0..n).rev() {
            let obj = base + off + k * sz;
            unsafe { (obj as *mut usize).write(head); }
            head = obj;
        }
        l.head[i] = head;
        l.free[i] = n as u32;
        off += n * sz;
    }
    CLASS_START[NCLASS].store(base + off, Ordering::Relaxed);
    debug_assert!(off <= crate::buddy::PAGE_SIZE << POOL_ORDER);
    BASE.store(base, Ordering::Relaxed);
    END.store(base + off, Ordering::Release);
}

/// Install the "am I in hard-IRQ context" test the global allocator uses
/// (`sched::irqctx::in_interrupt`; `mm` sits below `sched`).
pub fn set_irq_query(f: fn() -> bool) { IRQ_QUERY.store(f as usize, Ordering::Release); }

/// True when the global allocator should route to the pool.
#[inline]
pub fn in_irq_context() -> bool {
    let p = IRQ_QUERY.load(Ordering::Acquire);
    if p == 0 { return false; }
    let f: fn() -> bool = unsafe { core::mem::transmute(p) };
    f()
}

fn class_for(size: usize, align: usize) -> Option<usize> {
    let need = size.max(align).max(1);
    CLASSES.iter().position(|&(sz, _)| sz >= need)
}

/// Allocate from the pool. Any context; never blocks. `None` if no class of
/// at least `max(size, align)` bytes has a free object.
pub fn alloc(size: usize, align: usize) -> Option<*mut u8> {
    let first = class_for(size, align)?;
    if END.load(Ordering::Acquire) == 0 { return None; }
    let f = unsafe { arch_interrupt_save() };
    let mut l = LISTS.lock();
    let first_empty = l.head[first] == 0;
    let mut got = None;
    for c in first..NCLASS {
        let h = l.head[c];
        if h != 0 {
            l.head[c] = unsafe { (h as *const usize).read() };
            l.free[c] -= 1;
            got = Some(h as *mut u8);
            break;
        }
    }
    drop(l);
    unsafe { arch_interrupt_restore(f); }
    if first_empty && !EXHAUSTED_WARNED[first].swap(true, Ordering::Relaxed) {
        log("[ATOMIC-POOL] a size class ran out (IRQ-context allocation pressure)\n");
    }
    if got.is_none() { FAILS.fetch_add(1, Ordering::Relaxed); }
    got
}

/// True if `ptr` came from the pool.
#[inline]
pub fn owns(ptr: *mut u8) -> bool {
    let p = ptr as usize;
    p >= BASE.load(Ordering::Relaxed) && p < END.load(Ordering::Relaxed)
}

/// Return a pool object. Any context.
pub fn free(ptr: *mut u8) {
    let p = ptr as usize;
    let mut c = NCLASS;
    for i in 0..NCLASS {
        if p >= CLASS_START[i].load(Ordering::Relaxed) && p < CLASS_START[i + 1].load(Ordering::Relaxed) {
            c = i;
            break;
        }
    }
    if c == NCLASS { return; }
    let f = unsafe { arch_interrupt_save() };
    let mut l = LISTS.lock();
    unsafe { (p as *mut usize).write(l.head[c]); }
    l.head[c] = p;
    l.free[c] += 1;
    drop(l);
    unsafe { arch_interrupt_restore(f); }
}

/// Free objects per class, as `(class size, free, capacity)`.
pub fn census(emit: &mut dyn FnMut(usize, u32, usize)) {
    let f = unsafe { arch_interrupt_save() };
    let free = LISTS.lock().free;
    unsafe { arch_interrupt_restore(f); }
    for (i, &(sz, n)) in CLASSES.iter().enumerate() { emit(sz, free[i], n); }
}

/// Allocations that found the pool exhausted.
pub fn failures() -> u32 { FAILS.load(Ordering::Relaxed) }

// ── kmalloc-style C API (size header, for callers whose free has no size) ──

const HDR: usize = 16;

/// Allocate `size` bytes, 16-byte aligned. `atomic != 0`: never block —
/// pool first, then the slab only if its lock is free right now (any context,
/// IRQ handlers included). `atomic == 0`: ordinary allocation (task context).
/// NULL on failure. Free with `kfacil_kfree`.
#[no_mangle]
pub extern "C" fn kfacil_kmalloc(size: usize, atomic: u32) -> *mut u8 {
    let total = match size.checked_add(HDR) { Some(t) => t, None => return core::ptr::null_mut() };
    let raw = if atomic != 0 {
        alloc(total, 16).or_else(|| crate::slab::try_alloc(total))
    } else {
        crate::slab::alloc(total)
    };
    match raw {
        Some(p) => unsafe { (p as *mut usize).write(total); p.add(HDR) },
        None => core::ptr::null_mut(),
    }
}

/// Free memory from `kfacil_kmalloc`. Any context for pool memory; slab
/// memory is freed through the slab (spin lock) — fine in task context and
/// in IRQ context on this kernel (holders run with IRQs masked).
#[no_mangle]
pub unsafe extern "C" fn kfacil_kfree(ptr: *mut u8) {
    if ptr.is_null() { return; }
    let p = ptr.sub(HDR);
    if owns(p) { free(p); return; }
    let total = (p as *const usize).read();
    crate::slab::free(p, total);
}
