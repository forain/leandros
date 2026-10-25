//! Stand-in for tikv-jemallocator, patched over crates.io when building
//! ripgrep (scripts/build-all.sh). Upstream's musl build links jemalloc, whose
//! C build does not cross-compile with our zig toolchain, and the system
//! allocator is what every other static port here uses. Same public name, so
//! ripgrep's `#[global_allocator]` compiles untouched.
use std::alloc::{GlobalAlloc, Layout, System};

pub struct Jemalloc;

unsafe impl GlobalAlloc for Jemalloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 { System.alloc(l) }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) { System.dealloc(p, l) }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 { System.alloc_zeroed(l) }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 { System.realloc(p, l, n) }
}
