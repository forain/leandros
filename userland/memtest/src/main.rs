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
    if !test_fill_most_of_ram() { failures += 1; }

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

/// Map and touch most of the guest's free RAM, check every page still holds
/// what was written, unmap it all, and check the memory came back.
///
/// This is the 4 GiB-guest regression test (TODO item 12). On x86-64/q35 a
/// guest above 2.75 GiB has RAM at physical 4 GiB and up, and the buddy hands
/// that out only once the low RAM is gone — so nothing short of filling
/// memory ever exercises a physical address that does not fit in 32 bits:
/// page-table entries, the HHDM zeroing of fresh frames, and the intrusive
/// free-list links on the way back. A pattern that names the page catches a
/// truncated address (two virtual pages aliasing one frame) as a mismatch
/// instead of as a corruption somewhere else later.
///
/// Chunks of 256 MiB (below the kernel's 512 MiB anonymous-mmap cap) are
/// demand-paged, so this is also the biggest page-fault storm in the suite:
/// ~0.9 M faults on a 4 GiB guest, a few seconds under KVM.
unsafe fn free_ram() -> usize {
    const SYS_SYSINFO: usize = 99;
    let mut si = [0u8; 112];
    if leandros_libc::syscall::syscall1(SYS_SYSINFO, si.as_mut_ptr() as usize) != 0 { return 0; }
    u64::from_le_bytes(si[40..48].try_into().unwrap()) as usize
}

/// Allocate, touch, verify and free a fixed slice of RAM repeatedly, printing
/// free memory after each round. This is the 4 GiB-guest regression test
/// (TODO item 12): on x86-64/q35 a guest above ~2.75 GiB has RAM at physical
/// >= 4 GiB, handed out only once low RAM is gone, so nothing short of
/// touching gigabytes ever exercises a >32-bit physical address — the
/// intrusive free-list links, the HHDM zeroing of fresh frames, the
/// page-table entries. A pattern that names each page turns a truncated
/// address (two VAs aliasing one frame) into a reported mismatch, not a
/// silent corruption. Iterating at a FIXED size and printing free RAM each
/// round separates a real allocator leak (free RAM trends down round over
/// round) from the desktop's concurrent growth (free RAM steady, ~noise).
unsafe fn test_fill_most_of_ram() -> bool {
    let name = b"fill_ram_no_leak\0";
    const CHUNK: usize = 256 << 20;   // 256 MiB per mmap (< the 512 MiB cap)
    const ROUNDS: usize = 6;
    const HEADROOM: usize = 768 << 20; // never fight the desktop into true OOM

    let free0 = free_ram();
    // How many chunks fit under free-minus-headroom, capped so the whole
    // round stays a few seconds under KVM.
    let want = free0.saturating_sub(HEADROOM) / CHUNK;
    let want = if want > 12 { 12 } else { want }; // <= 3 GiB touched per round
    if want == 0 {
        write(STDOUT_FILENO, b"  (too little free RAM; skipped)\n".as_ptr(), 34);
        return report(name, true);
    }

    let mut chunks = [core::ptr::null_mut::<u8>(); 12];
    let mut worst_bad = 0usize;
    let mut prev_free = 0usize;
    let mut last_delta = 0isize;

    for round in 0..ROUNDS {
        let mut mapped = 0;
        for i in 0..want {
            let p = mmap(core::ptr::null_mut(), CHUNK, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if p as isize == -1 { break; }
            chunks[i] = p; mapped += 1;
            let mut off = 0;
            while off < CHUNK {
                let tag = ((round as u64) << 48) | ((i as u64) << 40) | off as u64;
                *(p.add(off) as *mut u64) = tag;
                *(p.add(off + PAGE - 8) as *mut u64) = !tag;
                off += PAGE;
            }
        }
        for i in 0..mapped {
            let p = chunks[i];
            let mut off = 0;
            while off < CHUNK {
                let tag = ((round as u64) << 48) | ((i as u64) << 40) | off as u64;
                if *(p.add(off) as *const u64) != tag
                    || *(p.add(off + PAGE - 8) as *const u64) != !tag { worst_bad += 1; }
                off += PAGE;
            }
        }
        for i in 0..mapped { munmap(chunks[i], CHUNK); }
        let fa = free_ram();
        if round > 0 { last_delta = prev_free as isize - fa as isize; }
        prev_free = fa;
        write(STDOUT_FILENO, b"  round ".as_ptr(), 8); print_dec(round);
        write(STDOUT_FILENO, b": touched MiB=".as_ptr(), 14); print_dec(mapped * (CHUNK >> 20));
        write(STDOUT_FILENO, b" free_after MiB=".as_ptr(), 16); print_dec(fa >> 20);
        write(STDOUT_FILENO, b" bad=".as_ptr(), 5); print_dec(worst_bad);
        write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    }

    // A real munmap/free leak drops free RAM by a fixed large amount EVERY
    // identical round and never reaches steady state; the desktop's one-time
    // startup growth (which overlaps this test) tapers off. So the pass bar
    // is steady state, not an absolute floor: the last inter-round delta is
    // under 64 MiB, and no page ever read back wrong. `free0` is only used
    // to size the rounds.
    let _ = free0;
    let ok = worst_bad == 0 && last_delta < (64 << 20);
    report(name, ok)
}

unsafe fn print_dec(mut v: usize) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop { i -= 1; buf[i] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
    write(STDOUT_FILENO, buf.as_ptr().add(i), buf.len() - i);
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

