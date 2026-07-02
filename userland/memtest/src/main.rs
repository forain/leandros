//! memtest — regression coverage for TODO.md Phase 6 (memory management):
//! fork()/CoW page isolation, mremap-grow content preservation, buddy
//! coalescing under alloc/free churn, and MAP_SHARED fork visibility.
//!
//! Each check prints "<name>: PASS" or "<name>: FAIL" to stdout (serial
//! console); `main` returns the number of failures as the exit code.

#![no_std]
#![no_main]

extern crate leandros_libc;
use leandros_libc::*;

const PROT_READ:     i32 = 1;
const PROT_WRITE:    i32 = 2;
const MAP_SHARED:    i32 = 0x01;
const MAP_PRIVATE:   i32 = 0x02;
const MAP_ANONYMOUS: i32 = 0x20;
const PAGE:          usize = 4096;

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let mut failures = 0;

    if !test_fork_cow_isolation() { failures += 1; }
    if !test_mremap_preserves_data() { failures += 1; }
    if !test_buddy_survives_churn() { failures += 1; }
    if !test_map_shared_fork_visibility() { failures += 1; }

    puts(b"--- memtest done ---\0".as_ptr());
    failures
}

/// fork() must give parent and child independent copies of a page each
/// already touched before the fork: the child's write must never become
/// visible in the parent (proves CoW promotion + refcounting, not just
/// "didn't crash").
unsafe fn test_fork_cow_isolation() -> bool {
    let name = b"fork_cow_isolation\0";
    let p = mmap(core::ptr::null_mut(), PAGE, PROT_READ | PROT_WRITE,
                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if p as isize == -1 { return report(name, false); }

    *p = 0xAA; // pre-fork touch, so this page is already faulted in

    let pid = fork();
    if pid == 0 {
        *p = 0xCC; // child's write must stay private
        exit(0);
    }

    let mut status: i32 = 0;
    wait4(pid, &mut status as *mut i32, 0, core::ptr::null_mut());

    let parent_sees = *p;
    munmap(p, PAGE);
    report(name, parent_sees == 0xAA)
}

/// mremap-grow must preserve the original `old_size` bytes of content at
/// the (possibly new) address.
unsafe fn test_mremap_preserves_data() -> bool {
    let name = b"mremap_preserves_data\0";
    let p = mmap(core::ptr::null_mut(), PAGE, PROT_READ | PROT_WRITE,
                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if p as isize == -1 { return report(name, false); }

    for i in 0..PAGE { *p.add(i) = (i % 256) as u8; }

    const MREMAP_MAYMOVE: i32 = 1;
    let new_size = PAGE * 3;
    let np = mremap(p, PAGE, new_size, MREMAP_MAYMOVE);
    if np as isize == -1 { return report(name, false); }

    let mut ok = true;
    for i in 0..PAGE {
        if *np.add(i) != (i % 256) as u8 { ok = false; break; }
    }

    munmap(np, new_size);
    report(name, ok)
}

/// A churn loop of varying mmap/munmap sizes must not permanently fragment
/// the physical allocator: a large allocation afterward must still succeed.
/// Exercises buddy coalescing-on-free.
unsafe fn test_buddy_survives_churn() -> bool {
    let name = b"buddy_survives_churn\0";
    let sizes = [PAGE, PAGE * 2, PAGE * 4, PAGE * 8, PAGE, PAGE * 16, PAGE * 2];

    for _round in 0..64 {
        for &sz in sizes.iter() {
            let p = mmap(core::ptr::null_mut(), sz, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if p as isize == -1 { return report(name, false); }
            munmap(p, sz);
        }
    }

    let big = mmap(core::ptr::null_mut(), PAGE * 256, PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    let ok = big as isize != -1;
    if ok { munmap(big, PAGE * 256); }
    report(name, ok)
}

/// MAP_SHARED|MAP_ANONYMOUS pages touched before a fork must stay genuinely
/// shared afterward: writes from either side must become visible to the
/// other, unlike private CoW pages.
unsafe fn test_map_shared_fork_visibility() -> bool {
    let name = b"map_shared_fork_visibility\0";
    let p = mmap(core::ptr::null_mut(), PAGE, PROT_READ | PROT_WRITE,
                 MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if p as isize == -1 { return report(name, false); }

    *p = 0x11; // pre-fork touch

    let pid = fork();
    if pid == 0 {
        let sees_parent_value = *p == 0x11;
        *p = if sees_parent_value { 0x22 } else { 0x33 };
        exit(0);
    }

    let mut status: i32 = 0;
    wait4(pid, &mut status as *mut i32, 0, core::ptr::null_mut());

    let parent_sees_child_write = *p == 0x22;
    munmap(p, PAGE);
    report(name, parent_sees_child_write)
}

unsafe fn report(name: &[u8], passed: bool) -> bool {
    write(STDOUT_FILENO, name.as_ptr(), name.len() - 1); // drop the NUL terminator
    if passed {
        write(STDOUT_FILENO, b": PASS\n".as_ptr(), 7);
    } else {
        write(STDOUT_FILENO, b": FAIL\n".as_ptr(), 7);
    }
    passed
}

