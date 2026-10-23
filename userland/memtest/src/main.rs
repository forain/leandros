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
use leandros_libc::syscall::{nr, syscall2, syscall3, syscall4, syscall5};

const PROT_READ:     i32 = 1;
const PROT_WRITE:    i32 = 2;
const MAP_SHARED:    i32 = 0x01;
const MAP_PRIVATE:   i32 = 0x02;
const MAP_ANONYMOUS: i32 = 0x20;
const PAGE:          usize = 4096;

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    if argc >= 2 {
        let a = *argv.add(1);
        if *a == b'h' && *a.add(1) == b'o' && *a.add(2) == b'g' && *a.add(3) == 0 {
            // `memtest hog [MiB/s]` (default 32).
            let mut rate = 0usize;
            if argc >= 3 {
                let mut q = *argv.add(2);
                while (*q).is_ascii_digit() { rate = rate * 10 + (*q - b'0') as usize; q = q.add(1); }
            }
            memory_hog(if rate == 0 { 32 } else { rate });
        }
        // `memtest vmreuse`: only the VA-reuse and page-table-pruning checks.
        if *a == b'v' && *a.add(1) == b'm' && *a.add(2) == b'r' {
            let mut f = 0;
            if !test_mmap_reuses_freed_va() { f += 1; }
            if !test_munmap_frees_page_tables() { f += 1; }
            return f;
        }
        // `memtest mremap`: only the truncate / mremap / maps checks.
        if *a == b'm' && *a.add(1) == b'r' && *a.add(2) == b'e' {
            let mut f = 0;
            if !test_truncate_mapped_sigbus() { f += 1; }
            if !test_truncate_followups() { f += 1; }
            if !test_mremap_linux_semantics() { f += 1; }
            if !test_proc_maps_names() { f += 1; }
            if !test_mremap_preserves_data() { f += 1; }
            if !test_mremap_nomove() { f += 1; }
            if !test_stack_demand_paged() { f += 1; }
            if !test_proc_rss_file_written() { f += 1; }
            return f;
        }
        // `memtest mmlock`: only the lane-mmlock checks.
        if *a == b'm' && *a.add(1) == b'm' && *a.add(2) == b'l' {
            let mut f = 0;
            if !test_evdev_copy_unfaulted() { f += 1; }
            if !test_proc_reader_vs_busy() { f += 1; }
            if !test_teardown_irq_latency() { f += 1; }
            if !test_fork_smaps_irq_latency() { f += 1; }
            if !test_mincore_hole_huge_munmap() { f += 1; }
            return f;
        }
        // `memtest df [1|2|3]`: only the direction-flag check, or one part of it.
        if *a == b'd' && *a.add(1) == b'f' && *a.add(2) == 0 {
            let part = if argc >= 3 { (**argv.add(2)).wrapping_sub(b'0') as u32 } else { 0 };
            let mask = if (1..=3).contains(&part) { 1 << part } else { 0b1110 };
            return if df_check(mask) { 0 } else { 1 };
        }
    }
    let mut failures = 0;

    if !test_fork_cow_isolation() { failures += 1; }
    if !test_mremap_preserves_data() { failures += 1; }
    if !test_buddy_survives_churn() { failures += 1; }
    if !test_map_shared_fork_visibility() { failures += 1; }
    if !test_fill_most_of_ram() { failures += 1; }
    if !test_exit_frees_page_tables() { failures += 1; }
    if !test_eager_split_frees_tail() { failures += 1; }
    if !test_file_private_lazy_content() { failures += 1; }
    if !test_file_private_sigbus_past_eof() { failures += 1; }
    if !test_file_private_no_leak() { failures += 1; }
    if !test_file_private_survives_unlink() { failures += 1; }
    if !test_file_private_map_cost() { failures += 1; }
    if !test_proc_rss_anon() { failures += 1; }
    if !test_proc_rss_fork_cow() { failures += 1; }
    if !test_proc_rss_file_lazy() { failures += 1; }
    if !test_proc_rss_file_written() { failures += 1; }
    if !test_proc_pid_dir() { failures += 1; }
    if !test_slab_reclaims_empty_pages() { failures += 1; }
    if !test_stack_demand_paged() { failures += 1; }
    if !test_mincore_hole_huge_munmap() { failures += 1; }
    if !test_stack_deep_recursion() { failures += 1; }
    if !test_stack_overflow_segv() { failures += 1; }
    if !test_prot_none_faults() { failures += 1; }
    if !test_mremap_nomove() { failures += 1; }
    if !test_big_lazy_reservations() { failures += 1; }
    if !test_mmap_hint_is_only_a_hint() { failures += 1; }
    if !test_mmap_skips_occupied_ranges() { failures += 1; }
    if !test_mmap_fixed_noreplace() { failures += 1; }
    if !test_el0_cache_maintenance() { failures += 1; }
    if !test_direction_flag_kernel_entry() { failures += 1; }
    if !test_madvise_dontneed() { failures += 1; }
    if !test_pagecache_shared_across_processes() { failures += 1; }
    if !test_pagecache_cow_isolation() { failures += 1; }
    if !test_pagecache_write_truncate_unlink() { failures += 1; }
    if !test_pagecache_map_shared_unchanged() { failures += 1; }
    if !test_pagecache_memory_returns() { failures += 1; }
    if !test_mempolicy_single_node() { failures += 1; }
    if !test_mmap_reuses_freed_va() { failures += 1; }
    if !test_munmap_frees_page_tables() { failures += 1; }
    if !test_truncate_mapped_sigbus() { failures += 1; }
    if !test_truncate_followups() { failures += 1; }
    if !test_mremap_linux_semantics() { failures += 1; }
    if !test_proc_maps_names() { failures += 1; }
    if !test_evdev_copy_unfaulted() { failures += 1; }
    if !test_proc_reader_vs_busy() { failures += 1; }
    if !test_teardown_irq_latency() { failures += 1; }

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
    // Linux's own numbers (see userland/meminfo): 99 was x86_64-only, so on
    // aarch64 this read 0 and the RAM tests silently skipped themselves.
    #[cfg(target_arch = "x86_64")]
    const SYS_SYSINFO: usize = 99;
    #[cfg(target_arch = "aarch64")]
    const SYS_SYSINFO: usize = 179;
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

/// A process's death must return its page-table tree, not just its frames.
///
/// Until 2026-09-18 `AddressSpace::drop` freed the leaf frames and the root
/// and left every intermediate PDPT/PD/PT page allocated forever: ~50 pages
/// per `brush -c true`, ~1000 per greeter chain, unbounded across a boot.
/// Each child here maps 16 sparse 64 MiB regions and touches one page per
/// 2 MiB of each, so it owns ~512 page-table pages (2 MiB) that only the
/// tree walk can return; 32 such deaths that leaked would cost 64 MiB, a
/// tree that is freed costs the noise floor. The bar is the mean loss per
/// death, so a concurrent desktop's one-off allocation cannot fail it.
unsafe fn test_exit_frees_page_tables() -> bool {
    let name = b"exit_frees_page_tables\0";
    const REGION: usize = 64 << 20;
    const REGIONS: usize = 16;
    const DEATHS: usize = 32;
    let before = free_ram();
    let mut spawn_failures = 0usize;
    for _ in 0..DEATHS {
        let pid = fork();
        if pid == 0 {
            for _ in 0..REGIONS {
                let p = mmap(core::ptr::null_mut(), REGION, PROT_READ | PROT_WRITE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
                if p as isize == -1 { exit(3); }
                let mut off = 0usize;
                while off < REGION { *p.add(off) = 1; off += 2 << 20; }
            }
            exit(0);
        }
        if pid < 0 { spawn_failures += 1; continue; }
        let mut status: i32 = 0;
        wait4(pid, &mut status as *mut i32, 0, core::ptr::null_mut());
        if status != 0 { spawn_failures += 1; }
    }
    let after = free_ram();
    let lost = before.saturating_sub(after);
    let per_death = lost / DEATHS;
    write(STDOUT_FILENO, b"  deaths=".as_ptr(), 9); print_dec(DEATHS);
    write(STDOUT_FILENO, b" lost_kib_per_death=".as_ptr(), 20); print_dec(per_death >> 10);
    write(STDOUT_FILENO, b" child_failures=".as_ptr(), 16); print_dec(spawn_failures);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    // A leaked tree is >= 2 MiB per death. The reading is 0 on an idle system
    // since exit releases the address space before the parent's wait4 can
    // return (it was ~470 KiB while the last child's pages were still held);
    // the budget leaves room for a desktop allocating in the background.
    report(name, spawn_failures == 0 && per_death < (256 << 10))
}

/// Splitting an eager file mapping must give back its buddy block's tail.
///
/// A private file mmap is backed eagerly by one naturally aligned buddy
/// block rounded up to a power of two (65 pages -> a 128-page block), and only
/// the whole-VMA teardown knew that order. `mprotect` of a sub-range and
/// fork both convert the VMA to per-page tracking, and until 2026-09-24 that
/// conversion dropped the unmapped tail on the floor: 63 pages here per round,
/// and ~55 MiB per greeter-chain death (every library ld.so maps, RELROs and
/// forks). Each round maps 65 pages of this binary (past EOF reads as zeros),
/// splits it (mprotect on a middle page) or forks over it, then unmaps it; the
/// bar is 8 pages lost per round — the leak costs 63, and a live desktop's
/// own growth over the run stays under the bar.
unsafe fn test_eager_split_frees_tail() -> bool {
    let name = b"eager_split_frees_tail\0";
    #[cfg(target_arch = "x86_64")]
    const SYS_MPROTECT: usize = 10;
    #[cfg(target_arch = "aarch64")]
    const SYS_MPROTECT: usize = 226;
    const ROUNDS: usize = 64;
    const LEN: usize = 65 * PAGE;
    let fd = open(b"/bin/memtest\0".as_ptr(), 0 /* O_RDONLY */, 0);
    if fd < 0 { return report(name, false); }
    let before = free_ram();
    let mut failures = 0usize;
    for r in 0..ROUNDS {
        let p = mmap(core::ptr::null_mut(), LEN, PROT_READ, MAP_PRIVATE, fd, 0);
        if p as isize == -1 { failures += 1; continue; }
        // Page 0: every page wholly past this binary's end raises SIGBUS now
        // (private file mappings are demand-paged with Linux EOF semantics).
        let _ = core::ptr::read_volatile(p);
        if r % 2 == 0 {
            // split_at: a middle-page mprotect cuts the VMA in three.
            if leandros_libc::syscall::syscall3(SYS_MPROTECT, p as usize + PAGE * 32, PAGE,
                                                (PROT_READ | PROT_WRITE) as usize) != 0 {
                failures += 1;
            }
        } else {
            // clone_as: fork converts the parent's read-only eager VMA.
            let pid = fork();
            if pid == 0 { exit(0); }
            if pid < 0 { failures += 1; } else {
                let mut status: i32 = 0;
                wait4(pid, &mut status as *mut i32, 0, core::ptr::null_mut());
            }
        }
        munmap(p, LEN);
    }
    close(fd);
    let after = free_ram();
    let lost_pages = before.saturating_sub(after) / PAGE;
    write(STDOUT_FILENO, b"  rounds=".as_ptr(), 9); print_dec(ROUNDS);
    write(STDOUT_FILENO, b" lost_pages=".as_ptr(), 12); print_dec(lost_pages);
    write(STDOUT_FILENO, b" failures=".as_ptr(), 10); print_dec(failures);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    report(name, failures == 0 && lost_pages < 8 * ROUNDS)
}

const SEEK_SET: i32 = 0;
const SEEK_CUR: i32 = 1;
const SEEK_END: i32 = 2;

#[cfg(target_arch = "x86_64")]
const SYS_MPROTECT_NR: usize = 10;
#[cfg(target_arch = "aarch64")]
const SYS_MPROTECT_NR: usize = 226;

/// A multi-MiB f2fs file (several fault-around windows, a partial last
/// page), falling back to this binary.
unsafe fn open_big_file() -> i32 {
    let fd = open(b"/bin/brush\0".as_ptr(), 0, 0);
    if fd >= 0 { fd } else { open(b"/bin/memtest\0".as_ptr(), 0, 0) }
}

/// Size of the file behind `fd`, leaving its position where it was.
unsafe fn file_size(fd: i32) -> usize {
    let cur = lseek(fd, 0, SEEK_CUR);
    let end = lseek(fd, 0, SEEK_END);
    lseek(fd, cur, SEEK_SET);
    if end < 0 { 0 } else { end as usize }
}

/// Read exactly `len` bytes at `off` (loops over short reads).
unsafe fn pread_all(fd: i32, off: usize, dst: *mut u8, len: usize) -> bool {
    if lseek(fd, off as _, SEEK_SET) < 0 { return false; }
    let mut done = 0usize;
    while done < len {
        let r = read(fd, dst.add(done), len - done);
        if r <= 0 { return false; }
        done += r as usize;
    }
    true
}

fn now_ns() -> u64 {
    let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { clock_gettime(1 /* CLOCK_MONOTONIC */, &mut ts); }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// A MAP_PRIVATE file mapping (demand-paged since 2026-09-25) must read the
/// file's bytes, zero-fill the tail of the last page, leave the descriptor's
/// position alone, keep its own writes private (from the file and from a
/// fork child), and survive a sub-range mprotect split.
unsafe fn test_file_private_lazy_content() -> bool {
    let name = b"file_private_lazy_content\0";
    let fd = open_big_file();
    if fd < 0 { return report(name, false); }
    let size = file_size(fd);
    let len = (size + PAGE - 1) & !(PAGE - 1);
    let mut ok = size > 3 * PAGE;
    lseek(fd, 123, SEEK_SET);
    let p = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    if p as isize == -1 { close(fd); return report(name, false); }
    if lseek(fd, 0, SEEK_CUR) != 123 { ok = false; puts(b"  fd position moved\0".as_ptr()); }
    let buf = malloc(PAGE);
    // Walk backwards so the fault-around window does not pre-populate most pages.
    let mut pg = len / PAGE;
    while ok && pg > 0 {
        pg -= 1;
        let off = pg * PAGE;
        let n = if size - off < PAGE { size - off } else { PAGE };
        if !pread_all(fd, off, buf, n) { ok = false; break; }
        if memcmp(p.add(off), buf, n) != 0 {
            ok = false; puts(b"  content mismatch\0".as_ptr()); break;
        }
        for i in n..PAGE {
            if *p.add(off + i) != 0 { ok = false; puts(b"  tail not zero\0".as_ptr()); break; }
        }
    }
    let orig0 = *p;
    *p = 0x5A;
    *p.add(PAGE * 2) = 0x5B;
    let pid = fork();
    if pid == 0 {
        let seen = *p == 0x5A && *p.add(PAGE * 2) == 0x5B;
        *p = 0x77;
        exit(if seen && *p == 0x77 { 0 } else { 1 });
    }
    let mut status: i32 = 0;
    wait4(pid, &mut status as *mut i32, 0, core::ptr::null_mut());
    if status != 0 { ok = false; puts(b"  child saw wrong data\0".as_ptr()); }
    if *p != 0x5A { ok = false; puts(b"  child write leaked into parent\0".as_ptr()); }
    let mut b0 = 0u8;
    if !pread_all(fd, 0, &mut b0, 1) || b0 != orig0 {
        ok = false; puts(b"  private write reached the file\0".as_ptr());
    }
    // Split in the middle and re-verify a page on each side.
    if leandros_libc::syscall::syscall3(SYS_MPROTECT_NR, p as usize + PAGE, PAGE,
                                        PROT_READ as usize) != 0 { ok = false; }
    if !pread_all(fd, PAGE, buf, PAGE) || memcmp(p.add(PAGE), buf, PAGE) != 0 { ok = false; }
    if *p.add(PAGE * 2) != 0x5B { ok = false; }
    free(buf);
    munmap(p, len);
    close(fd);
    report(name, ok)
}

/// A page wholly past the end of the file raises SIGBUS (Linux semantics),
/// while the partial last page reads the file then zeros.
unsafe fn test_file_private_sigbus_past_eof() -> bool {
    let name = b"file_private_sigbus_past_eof\0";
    let fd = open(b"/bin/memtest\0".as_ptr(), 0, 0);
    if fd < 0 { return report(name, false); }
    let size = file_size(fd);
    let len = ((size + PAGE - 1) & !(PAGE - 1)) + 2 * PAGE;
    let p = mmap(core::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0);
    close(fd);
    if p as isize == -1 { return report(name, false); }
    let pid = fork();
    if pid == 0 {
        if size % PAGE != 0 && core::ptr::read_volatile(p.add(size)) != 0 { exit(3); }
        let _ = core::ptr::read_volatile(p.add(len - PAGE));
        exit(4); // must not get here
    }
    let mut status: i32 = 0;
    wait4(pid, &mut status as *mut i32, 0, core::ptr::null_mut());
    munmap(p, len);
    write(STDOUT_FILENO, b"  child_status=".as_ptr(), 15); print_dec(status as usize);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    report(name, status & 0x7f == 7)
}

/// Demand-paged private file mappings, touched, forked and unmapped, must
/// return every page they populated. The rounds run in a child so the page
/// tables each fresh mapping address costs (kept until exit, ~3 per 6 MiB
/// round) are returned too, and the bar can be tight.
unsafe fn test_file_private_no_leak() -> bool {
    let name = b"file_private_no_leak\0";
    const ROUNDS: usize = 32;
    let fd = open_big_file();
    if fd < 0 { return report(name, false); }
    let size = file_size(fd);
    let len = (size + PAGE - 1) & !(PAGE - 1);
    // Warm-up round so one-time allocations (registry, page tables) settle.
    // It touches every page: the shared page cache keeps a file's pages
    // while any process (here the shell running /bin/brush) maps it, and
    // those first-time fills are not a leak.
    let w = mmap(core::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0);
    if w as isize != -1 {
        let mut off = 0usize;
        while off < size { let _ = core::ptr::read_volatile(w.add(off)); off += PAGE; }
        munmap(w, len);
    }
    let before = free_ram();
    let worker = fork();
    if worker == 0 {
        let mut failures = 0i32;
        for r in 0..ROUNDS {
            let p = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
            if p as isize == -1 { failures += 1; continue; }
            let mut off = 0usize;
            while off < size { let _ = core::ptr::read_volatile(p.add(off)); off += PAGE; }
            *p.add(PAGE) = 1;
            if r % 2 == 1 {
                let pid = fork();
                if pid == 0 { *p = 2; exit(0); }
                if pid < 0 { failures += 1; } else {
                    let mut status: i32 = 0;
                    wait4(pid, &mut status as *mut i32, 0, core::ptr::null_mut());
                }
            }
            munmap(p, len);
        }
        exit(failures);
    }
    let mut status: i32 = -1;
    if worker > 0 { wait4(worker, &mut status as *mut i32, 0, core::ptr::null_mut()); }
    close(fd);
    let after = free_ram();
    let lost_pages = before.saturating_sub(after) / PAGE;
    write(STDOUT_FILENO, b"  rounds=".as_ptr(), 9); print_dec(ROUNDS);
    write(STDOUT_FILENO, b" file_pages=".as_ptr(), 12); print_dec(len / PAGE);
    write(STDOUT_FILENO, b" lost_pages=".as_ptr(), 12); print_dec(lost_pages);
    write(STDOUT_FILENO, b" worker_status=".as_ptr(), 15); print_dec(status as usize);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    report(name, status == 0 && lost_pages < 64)
}

/// A private mapping outlives its descriptor *and* the file's name: pages
/// first touched after close + unlink (and after other files reuse freed
/// blocks) must still read the original bytes — the inode is pinned.
unsafe fn test_file_private_survives_unlink() -> bool {
    let name = b"file_private_survives_unlink\0";
    const PAGES: usize = 40;
    let path = b"/root/.memtest-lazymmap\0";
    let other = b"/root/.memtest-lazymmap-2\0";
    let fd = open(path.as_ptr(), 0x40 | 0x2 | 0x200 /* O_CREAT|O_RDWR|O_TRUNC */, 0o600);
    if fd < 0 { return report(name, false); }
    let buf = malloc(PAGE);
    for pg in 0..PAGES {
        for i in 0..PAGE { *buf.add(i) = (pg * 7 + i % 251) as u8; }
        if write(fd, buf, PAGE) != PAGE as isize { close(fd); free(buf); return report(name, false); }
    }
    let p = mmap(core::ptr::null_mut(), PAGES * PAGE, PROT_READ, MAP_PRIVATE, fd, 0);
    close(fd);
    unlink(path.as_ptr());
    let mut ok = p as isize != -1;
    // Churn the freed-block pool: write and delete another file of the same size.
    let fd2 = open(other.as_ptr(), 0x40 | 0x2 | 0x200, 0o600);
    if fd2 >= 0 {
        memset(buf, 0xEE, PAGE);
        for _ in 0..PAGES { write(fd2, buf, PAGE); }
        close(fd2);
        unlink(other.as_ptr());
    }
    if ok {
        let mut pg = PAGES;
        while pg > 0 {
            pg -= 1;
            for i in (0..PAGE).step_by(97) {
                if *p.add(pg * PAGE + i) != (pg * 7 + i % 251) as u8 { ok = false; break; }
            }
            if !ok { break; }
        }
        munmap(p, PAGES * PAGE);
    }
    free(buf);
    report(name, ok)
}

/// mmap(2) of a big file must not cost a copy of the file: time it.
/// Reports map_us and the first-touch cost; fails only if the map fails.
unsafe fn test_file_private_map_cost() -> bool {
    let name = b"file_private_map_cost\0";
    let mut fd = open(b"/bin/cosmic-comp\0".as_ptr(), 0, 0);
    if fd < 0 { fd = open(b"/bin/memtest\0".as_ptr(), 0, 0); }
    if fd < 0 { return report(name, false); }
    let size = file_size(fd);
    let t0 = now_ns();
    let p = mmap(core::ptr::null_mut(), size, PROT_READ, MAP_PRIVATE, fd, 0);
    let t1 = now_ns();
    close(fd);
    if p as isize == -1 { return report(name, false); }
    let _ = core::ptr::read_volatile(p.add(size / 2));
    let t2 = now_ns();
    munmap(p, size);
    let t3 = now_ns();
    write(STDOUT_FILENO, b"  bytes=".as_ptr(), 8); print_dec(size);
    write(STDOUT_FILENO, b" map_us=".as_ptr(), 8); print_dec(((t1 - t0) / 1000) as usize);
    write(STDOUT_FILENO, b" touch_us=".as_ptr(), 10); print_dec(((t2 - t1) / 1000) as usize);
    write(STDOUT_FILENO, b" unmap_us=".as_ptr(), 10); print_dec(((t3 - t2) / 1000) as usize);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    report(name, true)
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



// ── /proc/<pid>/status, statm, smaps_rollup accounting (2026-09-26) ─────────

/// Read `path` (NUL-terminated) into `buf`; returns the byte count.
unsafe fn slurp(path: &[u8], buf: &mut [u8]) -> usize {
    let fd = open(path.as_ptr(), 0, 0);
    if fd < 0 { return 0; }
    let mut n = 0usize;
    while n < buf.len() {
        let r = read(fd, buf.as_mut_ptr().add(n), buf.len() - n);
        if r <= 0 { break; }
        n += r as usize;
    }
    close(fd);
    n
}

/// `/proc/<pid>/<file>\0` (pid 0 = self) into `out`.
fn proc_path(pid: i32, file: &[u8], out: &mut [u8; 64]) -> usize {
    let mut p = 0;
    for &b in b"/proc/" { out[p] = b; p += 1; }
    if pid == 0 {
        for &b in b"self" { out[p] = b; p += 1; }
    } else {
        let mut d = [0u8; 10]; let mut k = 0; let mut v = pid as u32;
        loop { d[k] = b'0' + (v % 10) as u8; k += 1; v /= 10; if v == 0 { break; } }
        while k > 0 { k -= 1; out[p] = d[k]; p += 1; }
    }
    out[p] = b'/'; p += 1;
    for &b in file { out[p] = b; p += 1; }
    out[p] = 0;
    p + 1
}

fn parse_num(s: &[u8]) -> Option<usize> {
    let mut v = 0usize; let mut any = false;
    for &b in s {
        if b.is_ascii_digit() { v = v * 10 + (b - b'0') as usize; any = true; }
        else if any { break; }
    }
    if any { Some(v) } else { None }
}

/// Value of `key` (e.g. b"RssAnon:") in a `Key: value kB` file of `pid`.
unsafe fn proc_kb(pid: i32, file: &[u8], key: &[u8]) -> Option<usize> {
    let mut path = [0u8; 64];
    proc_path(pid, file, &mut path);
    let mut buf = [0u8; 2048];
    let n = slurp(&path, &mut buf);
    for line in buf[..n].split(|&b| b == b'\n') {
        if line.starts_with(key) { return parse_num(&line[key.len()..]); }
    }
    None
}

/// Field `idx` (0-based) of `/proc/<pid>/statm`, in pages.
unsafe fn statm(pid: i32, idx: usize) -> Option<usize> {
    let mut path = [0u8; 64];
    proc_path(pid, b"statm", &mut path);
    let mut buf = [0u8; 128];
    let n = slurp(&path, &mut buf);
    buf[..n].split(|&b| b == b' ' || b == b'\n').filter(|f| !f.is_empty()).nth(idx).and_then(parse_num)
}

unsafe fn say_kb(label: &[u8], v: usize) {
    write(STDOUT_FILENO, label.as_ptr(), label.len());
    print_dec(v);
}

/// Mapping and touching N MiB of anonymous memory raises RssAnon (and VmRSS,
/// statm resident) by N MiB; unmapping takes it back off while VmHWM keeps
/// the peak. VmSize grows by the mapping as soon as it exists.
unsafe fn test_proc_rss_anon() -> bool {
    let name = b"proc_rss_anon\0";
    const MIB: usize = 32;
    const LEN: usize = MIB << 20;
    let a0 = proc_kb(0, b"status", b"RssAnon:").unwrap_or(usize::MAX);
    let s0 = proc_kb(0, b"status", b"VmSize:").unwrap_or(0);
    let p = mmap(core::ptr::null_mut(), LEN, PROT_READ | PROT_WRITE,
                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if p as isize == -1 || a0 == usize::MAX { return report(name, false); }
    let s1 = proc_kb(0, b"status", b"VmSize:").unwrap_or(0);
    let mut off = 0; while off < LEN { *p.add(off) = 1; off += PAGE; }
    let a1 = proc_kb(0, b"status", b"RssAnon:").unwrap_or(0);
    let rss1 = proc_kb(0, b"status", b"VmRSS:").unwrap_or(0);
    let res1 = statm(0, 1).unwrap_or(0) * 4;
    munmap(p, LEN);
    let a2 = proc_kb(0, b"status", b"RssAnon:").unwrap_or(0);
    let hwm = proc_kb(0, b"status", b"VmHWM:").unwrap_or(0);
    let grew = a1.saturating_sub(a0);
    say_kb(b"  rss_anon_kib before=", a0); say_kb(b" touched=", a1); say_kb(b" unmapped=", a2);
    say_kb(b" vmhwm=", hwm); say_kb(b" vmrss=", rss1); say_kb(b" statm_res_kib=", res1);
    say_kb(b" vmsize_delta=", s1.saturating_sub(s0));
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    let want = MIB << 10;
    report(name,
        grew >= want && grew <= want + 256              // +N MiB (a little stack/heap slack)
        && a2 <= a0 + 256                                 // and back off on munmap
        && hwm >= a1                                      // peak kept
        && s1.saturating_sub(s0) >= want                  // VmSize counts the whole VMA
        && res1 + 16 >= rss1 && rss1 + 16 >= res1)       // statm agrees with status
}

/// After fork the child's RSS includes every frame it still shares with the
/// parent (as on Linux), smaps_rollup reports them as shared with Pss about
/// half; the child's writes (CoW copies) leave its RSS unchanged and turn the
/// pages private. The parent reads the child's files by pid.
unsafe fn test_proc_rss_fork_cow() -> bool {
    let name = b"proc_rss_fork_cow\0";
    const LEN: usize = 16 << 20;
    let want = LEN >> 10;
    let p = mmap(core::ptr::null_mut(), LEN, PROT_READ | PROT_WRITE,
                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if p as isize == -1 { return report(name, false); }
    let mut off = 0; while off < LEN { *p.add(off) = 7; off += PAGE; }
    let mut go = [0i32; 2];
    let mut done = [0i32; 2];
    if syscall2(nr::PIPE2, go.as_mut_ptr() as usize, 0) != 0
        || syscall2(nr::PIPE2, done.as_mut_ptr() as usize, 0) != 0
    {
        return report(name, false);
    }
    let pid = fork();
    if pid == 0 {
        let mut b = [0u8; 1];
        let before = proc_kb(0, b"status", b"RssAnon:").unwrap_or(0);
        read(go[0], b.as_mut_ptr(), 1);
        let mut off = 0; while off < LEN { *p.add(off) = 9; off += PAGE; }
        let after = proc_kb(0, b"status", b"RssAnon:").unwrap_or(0);
        write(done[1], b.as_ptr(), 1);
        read(go[0], b.as_mut_ptr(), 1); // parent has looked; exit
        // CoW copies replace shared frames one for one: RSS must not move.
        exit(if before >= want && after + 64 >= before && after <= before + 64 { 0 } else { 1 });
    }
    if pid < 0 { return report(name, false); }
    let c_rss = proc_kb(pid, b"status", b"RssAnon:").unwrap_or(0);
    let c_rss_statm = statm(pid, 1).unwrap_or(0) * 4;
    let sh0 = proc_kb(pid, b"smaps_rollup", b"Shared_Dirty:").unwrap_or(0);
    let pss0 = proc_kb(pid, b"smaps_rollup", b"Pss:").unwrap_or(0);
    let rss0 = proc_kb(pid, b"smaps_rollup", b"Rss:").unwrap_or(0);
    let b = [1u8; 1];
    write(go[1], b.as_ptr(), 1);
    let mut r = [0u8; 1];
    read(done[0], r.as_mut_ptr(), 1);
    let pd1 = proc_kb(pid, b"smaps_rollup", b"Private_Dirty:").unwrap_or(0);
    let pss1 = proc_kb(pid, b"smaps_rollup", b"Pss:").unwrap_or(0);
    write(go[1], b.as_ptr(), 1);
    let mut status = 0i32;
    wait4(pid, &mut status, 0, core::ptr::null_mut());
    munmap(p, LEN);
    for fd in [go[0], go[1], done[0], done[1]] { close(fd); }
    say_kb(b"  child rss_anon_kib=", c_rss); say_kb(b" statm_res_kib=", c_rss_statm);
    say_kb(b" rollup rss=", rss0); say_kb(b" shared=", sh0); say_kb(b" pss=", pss0);
    say_kb(b" after_write private=", pd1); say_kb(b" pss=", pss1);
    say_kb(b" child_status=", status as usize);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    report(name,
        status == 0
        && c_rss >= want
        && sh0 >= want                   // the parent's frames, still shared
        && pss0 * 10 <= rss0 * 7         // shared frames count half
        && pd1 >= want                   // the child's own copies now
        && pss1 >= pss0 + want / 2 - 256)
}

/// A MAP_PRIVATE file mapping is demand-paged: mapping it adds nothing to
/// RssFile, touching 1 MiB of it adds 1 MiB.
unsafe fn test_proc_rss_file_lazy() -> bool {
    let name = b"proc_rss_file_lazy\0";
    const TOUCH: usize = 1 << 20;
    let fd = open_big_file();
    if fd < 0 { return report(name, false); }
    let size = file_size(fd) & !(PAGE - 1);
    if size < 2 * TOUCH { close(fd); return report(name, false); }
    let f0 = proc_kb(0, b"status", b"RssFile:").unwrap_or(usize::MAX);
    let p = mmap(core::ptr::null_mut(), size, PROT_READ, MAP_PRIVATE, fd, 0);
    close(fd);
    if p as isize == -1 || f0 == usize::MAX { return report(name, false); }
    let f1 = proc_kb(0, b"status", b"RssFile:").unwrap_or(0);
    let mut sum = 0u32;
    let mut off = 0; while off < TOUCH { sum = sum.wrapping_add(*p.add(off) as u32); off += PAGE; }
    let f2 = proc_kb(0, b"status", b"RssFile:").unwrap_or(0);
    munmap(p, size);
    let f3 = proc_kb(0, b"status", b"RssFile:").unwrap_or(0);
    say_kb(b"  rss_file_kib before=", f0); say_kb(b" mapped=", f1); say_kb(b" touched=", f2);
    say_kb(b" unmapped=", f3); say_kb(b" map_kib=", size >> 10); say_kb(b" sum=", sum as usize);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    let grew = f2.saturating_sub(f1);
    report(name,
        f1 <= f0 + 64                               // nothing read at mmap
        && grew >= TOUCH >> 10 && grew <= (TOUCH >> 10) + 64 // exactly what was touched (+1 fault-around window)
        && f3 <= f0 + 64)
}

/// A written page of a MAP_PRIVATE file mapping is the process's own copy:
/// it moves from RssFile to RssAnon (Linux's CoW'd private file page), both
/// for a user store and for a kernel store (read(2) into the mapping). The
/// file itself is untouched, the data written stays, and mprotect RO -> RW
/// does not make clean pages silently writable-uncounted.
unsafe fn test_proc_rss_file_written() -> bool {
    let name = b"proc_rss_file_written\0";
    const TOUCH: usize = 1 << 20;
    const WRITE: usize = 256 << 10;
    const KREAD: usize = 64 << 10;
    let fd = open_big_file();
    if fd < 0 { return report(name, false); }
    let size = file_size(fd) & !(PAGE - 1);
    if size < 2 * TOUCH { close(fd); return report(name, false); }
    let p = mmap(core::ptr::null_mut(), size, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    if p as isize == -1 { close(fd); return report(name, false); }
    let mut sum = 0u32;
    let mut off = 0; while off < TOUCH { sum = sum.wrapping_add(*p.add(off) as u32); off += PAGE; }
    let f1 = proc_kb(0, b"status", b"RssFile:").unwrap_or(0);
    let a1 = proc_kb(0, b"status", b"RssAnon:").unwrap_or(0);
    // User stores into the first 256 KiB.
    let mut off = 0; while off < WRITE { *p.add(off) = (*p.add(off)).wrapping_add(1); off += PAGE; }
    let f2 = proc_kb(0, b"status", b"RssFile:").unwrap_or(0);
    let a2 = proc_kb(0, b"status", b"RssAnon:").unwrap_or(0);
    // Kernel stores: read(2) the file's first 64 KiB into [512 KiB, 576 KiB).
    let kdst = p.add(512 << 10);
    let ok_read = pread_all(fd, 0, kdst, KREAD);
    let f3 = proc_kb(0, b"status", b"RssFile:").unwrap_or(0);
    let a3 = proc_kb(0, b"status", b"RssAnon:").unwrap_or(0);
    // mprotect RO then RW over a clean page, then write it: must not crash
    // and must count.
    let clean = p.add(768 << 10);
    let ok_mp = mprotect_raw(clean, PAGE, PROT_READ) == 0
        && mprotect_raw(clean, PAGE, PROT_READ | PROT_WRITE) == 0;
    *clean = (*clean).wrapping_add(1);
    let a4 = proc_kb(0, b"status", b"RssAnon:").unwrap_or(0);
    // The file still holds the original bytes; the mapping holds ours.
    let mut orig = [0u8; 1];
    let file_ok = pread_all(fd, 0, orig.as_mut_ptr(), 1) && *p == orig[0].wrapping_add(1);
    let mut first = [0u8; 16];
    let kread_ok = ok_read && pread_all(fd, 16, first.as_mut_ptr(), 16)
        && core::slice::from_raw_parts(kdst.add(16), 16) == &first[..];
    close(fd);
    munmap(p, size);
    say_kb(b"  file_kib touched=", f1); say_kb(b" anon_kib=", a1);
    say_kb(b" | user_write file=", f2); say_kb(b" anon=", a2);
    say_kb(b" | kernel_write file=", f3); say_kb(b" anon=", a3);
    say_kb(b" | mprotect_write anon=", a4); say_kb(b" file_ok=", file_ok as usize);
    say_kb(b" kread_ok=", kread_ok as usize); say_kb(b" sum=", sum as usize);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    let w = WRITE >> 10; let k = KREAD >> 10;
    report(name,
        file_ok && kread_ok && ok_mp
        && a2 >= a1 + w && a2 <= a1 + w + 64 && f1 >= f2 + w - 16 && f1 <= f2 + w + 64
        && a3 >= a2 + k && a3 <= a2 + k + 64 && f2 >= f3 + k - 16
        && a4 >= a3 + 4 && a4 <= a3 + 64)
}

/// `/proc` lists this process, `/proc/<pid>` opens as a directory listing
/// its files, and `openat(dirfd, "statm")` works — what bottom/procps do.
unsafe fn test_proc_pid_dir() -> bool {
    let name = b"proc_pid_dir\0";
    let me = getpid();
    let mut want = [0u8; 12]; let mut wl = 0;
    { let mut d = [0u8; 10]; let mut k = 0; let mut v = me as u32;
      loop { d[k] = b'0' + (v % 10) as u8; k += 1; v /= 10; if v == 0 { break; } }
      while k > 0 { k -= 1; want[wl] = d[k]; wl += 1; } }
    let find_in = |fd: i32, name: &[u8]| -> (bool, usize) {
        let mut buf = [0u8; 4096];
        let mut found = false; let mut entries = 0usize;
        loop {
            let n = syscall3(nr::GETDENTS64, fd as usize, buf.as_mut_ptr() as usize, buf.len());
            if n <= 0 { break; }
            let mut o = 0usize;
            while o < n as usize {
                let reclen = u16::from_le_bytes([buf[o + 16], buf[o + 17]]) as usize;
                let nm = &buf[o + 19..o + reclen];
                let nl = nm.iter().position(|&b| b == 0).unwrap_or(nm.len());
                if &nm[..nl] == name { found = true; }
                entries += 1;
                o += reclen;
            }
        }
        (found, entries)
    };
    let pfd = open(b"/proc\0".as_ptr(), 0, 0);
    let (in_proc, n_proc) = if pfd >= 0 { let r = find_in(pfd, &want[..wl]); close(pfd); r } else { (false, 0) };
    let mut dpath = [0u8; 20];
    dpath[..6].copy_from_slice(b"/proc/");
    dpath[6..6 + wl].copy_from_slice(&want[..wl]);
    let dfd = open(dpath.as_ptr(), 0, 0);
    let (has_statm, _) = if dfd >= 0 { find_in(dfd, b"statm") } else { (false, 0) };
    let mut rss_at = 0usize;
    if dfd >= 0 {
        let f = syscall4(nr::OPENAT, dfd as usize, b"statm\0".as_ptr() as usize, 0, 0);
        if f >= 0 {
            let mut b = [0u8; 128];
            let n = read(f as i32, b.as_mut_ptr(), b.len());
            if n > 0 {
                rss_at = b[..n as usize].split(|&c| c == b' ').nth(1).and_then(parse_num).unwrap_or(0);
            }
            close(f as i32);
        }
        close(dfd);
    }
    say_kb(b"  /proc entries=", n_proc); say_kb(b" self_listed=", in_proc as usize);
    say_kb(b" dir_lists_statm=", has_statm as usize); say_kb(b" openat_statm_rss_pages=", rss_at);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    report(name, in_proc && has_statm && rss_at > 0)
}

/// `/proc/kmemstat` figures: pages of slab class `class`, and the running
/// count of empty slab pages returned to the buddy allocator.
unsafe fn slab_figures(class: usize) -> (usize, usize) {
    static mut BUF: [u8; 32768] = [0; 32768];
    let buf = &mut *core::ptr::addr_of_mut!(BUF);
    let n = slurp(b"/proc/kmemstat\0", buf);
    let (mut pages, mut reclaimed) = (0, 0);
    for line in buf[..n].split(|&b| b == b'\n') {
        if let Some(r) = line.strip_prefix(b"slab_reclaimed_pages ") { reclaimed = parse_num(r).unwrap_or(0); }
        if let Some(r) = line.strip_prefix(b"slab ") {
            let mut f = r.split(|&b| b == b' ');
            if f.next().and_then(parse_num) == Some(class) {
                pages = f.next().and_then(parse_num).unwrap_or(0);
            }
        }
    }
    (pages, reclaimed)
}

/// A burst of kernel heap objects must not stay charged to the slab once
/// freed: a child maps 2000 256-KiB regions touching only the last page of
/// each (a 64-entry, 512-byte page vector per region: ~250 pages of the
/// 512-byte class) and exits. Afterwards the class holds at most a few
/// pages more than before (the empty-page reserve), and the reclaim counter
/// has moved. Until 2026-09-26 the slab never gave a page back.
unsafe fn test_slab_reclaims_empty_pages() -> bool {
    let name = b"slab_reclaims_empty_pages\0";
    const REGIONS: usize = 2000;
    const REGION: usize = 64 * PAGE;
    let (p0, r0) = slab_figures(512);
    let pid = fork();
    if pid == 0 {
        for _ in 0..REGIONS {
            let p = mmap(core::ptr::null_mut(), REGION, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if p as isize == -1 { exit(0); }
            *p.add(REGION - PAGE) = 1;
        }
        let (peak, _) = slab_figures(512);
        exit(if peak > 0 { ((peak / 50).min(200)) as i32 } else { 0 });
    }
    if pid < 0 { return report(name, false); }
    let mut status = 0i32;
    wait4(pid, &mut status, 0, core::ptr::null_mut());
    let (p1, r1) = slab_figures(512);
    let peak_approx = ((status >> 8) & 0xff) as usize * 50;
    say_kb(b"  slab512_pages before=", p0); say_kb(b" peak~", peak_approx);
    say_kb(b" after=", p1); say_kb(b" reclaimed_delta=", r1.saturating_sub(r0));
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    report(name, p1 <= p0 + 16 && r1 >= r0 + 100 && peak_approx >= p0 + 100)
}


/// `memtest hog`: the memory-pressure guard's test offender. Detaches into a
/// session of its own (off the serial login's, so init's guard may pick it)
/// and touches 16 MiB more anonymous memory every 500 ms (32 MiB/s: slow
/// enough for a guard that samples every 2 s and wants two low samples),
/// forever, printing its size; init's guard should SIGKILL exactly this
/// process.
unsafe fn memory_hog(rate_mib_s: usize) -> ! {
    if fork() != 0 { exit(0); }
    setsid();
    // 4 MiB steps, paced against the clock so the rate holds whatever the
    // touch itself costs.
    const STEP: usize = 4 << 20;
    let t0 = now_ns();
    let mut mib = 0usize;
    loop {
        let p = mmap(core::ptr::null_mut(), STEP, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if p as isize != -1 {
            let mut off = 0; while off < STEP { *p.add(off) = 1; off += PAGE; }
            mib += STEP >> 20;
        }
        if mib % 128 == 0 {
            write(STDOUT_FILENO, b"hog: pid ".as_ptr(), 9); print_dec(getpid() as usize);
            write(STDOUT_FILENO, b" holds ".as_ptr(), 7); print_dec(mib);
            write(STDOUT_FILENO, b" MiB\n".as_ptr(), 5);
        }
        let due_ns = (mib as u64) * 1_000_000_000 / rate_mib_s as u64;
        let el = now_ns().saturating_sub(t0);
        if due_ns > el { usleep(((due_ns - el) / 1000) as u32); }
    }
}


// ── Demand-paged main stack, guard, PROT_NONE (2026-09-26) ──────────────────

/// Recurse `depth` frames of a little over 1 KiB each, touching both ends of
/// every frame's buffer so every stack page on the way down is written.
#[inline(never)]
unsafe fn recurse(depth: usize) -> usize {
    let mut buf = [0u8; 1024];
    // Escape the whole buffer, or SROA shrinks the frame to the two bytes used.
    core::hint::black_box(&mut buf);
    core::ptr::write_volatile(buf.as_mut_ptr(), depth as u8);
    core::ptr::write_volatile(buf.as_mut_ptr().add(1023), depth as u8);
    if depth == 0 { return core::ptr::read_volatile(buf.as_ptr()) as usize; }
    let r = recurse(depth - 1);
    r + core::ptr::read_volatile(buf.as_ptr().add(1023)) as usize
}

/// Run `f` in a forked child and return its raw wait status.
unsafe fn in_child(f: unsafe fn() -> i32) -> i32 {
    let pid = fork();
    if pid == 0 { exit(f()); }
    if pid < 0 { return -1; }
    let mut status: i32 = 0;
    wait4(pid, &mut status as *mut i32, 0, core::ptr::null_mut());
    status
}

/// The main stack is demand-paged: untouched stack is not resident, and
/// touching 4 MiB of it raises RssAnon by about 4 MiB. With the eager stack
/// (before 2026-09-26) all 8 MiB were resident from exec, so the delta was 0.
unsafe fn test_stack_demand_paged() -> bool {
    let name = b"stack_demand_paged\0";
    unsafe fn child() -> i32 {
        let a0 = proc_kb(0, b"status", b"RssAnon:").unwrap_or(0);
        let r = recurse(4000); // ~4.2 MiB of stack
        let a1 = proc_kb(0, b"status", b"RssAnon:").unwrap_or(0);
        say_kb(b"  stack rss_anon_kib before=", a0); say_kb(b" after_4MiB_recursion=", a1);
        write(STDOUT_FILENO, b"\n".as_ptr(), 1);
        if r == usize::MAX { return 9; }
        if a1.saturating_sub(a0) >= 3900 { 0 } else { 1 }
    }
    let st = in_child(child);
    report(name, st == 0)
}

/// Recursing ~6.8 MiB deep on the 8 MiB main stack works.
unsafe fn test_stack_deep_recursion() -> bool {
    let name = b"stack_deep_recursion\0";
    unsafe fn child() -> i32 { if recurse(6500) == usize::MAX { 1 } else { 0 } }
    let st = in_child(child);
    write(STDOUT_FILENO, b"  deep_recursion_status=".as_ptr(), 24); print_dec(st as usize);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    report(name, st == 0)
}

/// Unbounded recursion runs off the bottom of the stack into the guard and
/// dies of SIGSEGV (not a hang, not a silent overwrite of another mapping).
unsafe fn test_stack_overflow_segv() -> bool {
    let name = b"stack_overflow_segv\0";
    unsafe fn child() -> i32 { if recurse(usize::MAX) == 7 { 2 } else { 3 } }
    let st = in_child(child);
    write(STDOUT_FILENO, b"  overflow_status=".as_ptr(), 18); print_dec(st as usize);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    report(name, st & 0x7f == 11)
}

unsafe fn mprotect_raw(p: *mut u8, len: usize, prot: i32) -> isize {
    syscall3(nr::MPROTECT, p as usize, len, prot as usize)
}

static mut PN: *mut u8 = core::ptr::null_mut();

/// PROT_NONE really is no access — the mechanism musl's pthread stack guard
/// pages rely on (the whole stack is mapped PROT_NONE, all but the guard then
/// mprotected RW). Reads and writes of an untouched PROT_NONE page, and of a
/// populated page mprotected to PROT_NONE (also across fork), raise SIGSEGV;
/// mprotect back to RW finds the data intact. Before 2026-09-26 a read got a
/// zero page and a write livelocked in the fault handler.
unsafe fn test_prot_none_faults() -> bool {
    let name = b"prot_none_faults\0";
    unsafe fn rd() -> i32 { core::ptr::read_volatile(PN) as i32 + 100 }
    unsafe fn wr() -> i32 { core::ptr::write_volatile(PN, 5); 100 }
    let fresh = mmap(core::ptr::null_mut(), 2 * PAGE, 0, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    let used = mmap(core::ptr::null_mut(), PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if fresh as isize == -1 || used as isize == -1 { return report(name, false); }
    PN = fresh;             let s1 = in_child(rd);
    PN = fresh.add(PAGE);   let s2 = in_child(wr);
    *used = 0xAB;
    let m1 = mprotect_raw(used, PAGE, 0);
    PN = used;              let s3 = in_child(rd);
                            let s4 = in_child(wr);
    let m2 = mprotect_raw(used, PAGE, PROT_READ | PROT_WRITE);
    let v = core::ptr::read_volatile(used);
    let m3 = mprotect_raw(fresh, 2 * PAGE, PROT_READ | PROT_WRITE);
    let z = core::ptr::read_volatile(fresh.add(PAGE));
    munmap(fresh, 2 * PAGE); munmap(used, PAGE);
    write(STDOUT_FILENO, b"  prot_none status".as_ptr(), 18);
    for s in [s1, s2, s3, s4] { write(STDOUT_FILENO, b" ".as_ptr(), 1); print_dec(s as usize); }
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    report(name, s1 & 0x7f == 11 && s2 & 0x7f == 11 && s3 & 0x7f == 11 && s4 & 0x7f == 11
        && m1 == 0 && m2 == 0 && m3 == 0 && v == 0xAB && z == 0)
}

/// mremap without MREMAP_MAYMOVE never moves: growing a range that does not
/// end its mapping is ENOMEM (content left in place), growing the last page
/// of a mapping with free room above succeeds in place, and an unmapped old
/// range is EFAULT. musl's pthread_getattr_np() probes the main stack this way.
unsafe fn test_mremap_nomove() -> bool {
    let name = b"mremap_nomove\0";
    let p = mmap(core::ptr::null_mut(), 4 * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if p as isize == -1 { return report(name, false); }
    *p.add(PAGE) = 7;
    let r1 = syscall5(nr::MREMAP, p as usize + PAGE, PAGE, 2 * PAGE, 0, 0);
    let still = core::ptr::read_volatile(p.add(PAGE));
    munmap(p.add(2 * PAGE), 2 * PAGE);
    let r2 = syscall5(nr::MREMAP, p as usize + PAGE, PAGE, 2 * PAGE, 0, 0);
    if r2 == p as isize + PAGE as isize { *p.add(2 * PAGE) = 1; } // the grown page is usable
    munmap(p, 4 * PAGE);
    let r3 = syscall5(nr::MREMAP, p as usize, PAGE, 2 * PAGE, 0, 0);
    say_kb(b"  mremap_nomove r1=", (-r1) as usize); say_kb(b" r2_inplace=", (r2 == p as isize + PAGE as isize) as usize);
    say_kb(b" r3=", (-r3) as usize);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    report(name, r1 == -12 && still == 7 && r2 == p as isize + PAGE as isize && r3 == -14)
}

/// Large demand-paged reservations cost nothing until touched (Firefox).
///
/// SpiderMonkey reserves ~2 GiB of JIT code space up front
/// (`mmap(NULL, 0x7FC00000, PROT_NONE, MAP_PRIVATE|MAP_ANONYMOUS|MAP_NORESERVE)`)
/// and commits pieces of it with MAP_FIXED; musl's ld.so maps a library's
/// whole span from its file before overlaying the segments. Both used to hit
/// the 512 MiB anonymous / 256 MiB file caps with EINVAL. Checks that the
/// mappings succeed, that touching the far end of a 4 GiB mapping raises
/// RssAnon by about two pages (no span-proportional allocation), and that a
/// 300 MiB private file mapping reads the file.
/// A non-MAP_FIXED address is a hint: one the kernel cannot use (above the
/// user address range, or not page aligned) must be ignored, not fail the
/// call. It used to be EINVAL, which made every random-address probe
/// SpiderMonkey's GC and mozjemalloc make across a 48-bit space fail.
unsafe fn test_mmap_hint_is_only_a_hint() -> bool {
    let name = b"mmap_hint_is_only_a_hint\0";
    let mut ok = true;
    for &hint in &[0xD445_E50C_9000usize, 0x8000_0000_0000, 0x7FFF_FFFF_F000, 0x1234_5678_9ABC] {
        let p = mmap(hint as *mut u8, 0x1000, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if p as isize == -1 { ok = false; continue; }
        core::ptr::write_volatile(p, 0x77);
        if core::ptr::read_volatile(p) != 0x77 { ok = false; }
        munmap(p, 0x1000);
    }
    // A usable hint is still honoured.
    let want = 0x5000_0000_0000usize as *mut u8;
    let p = mmap(want, 0x1000, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if p != want { ok = false; }
    if p as isize != -1 { munmap(p, 0x1000); }
    report(name, ok)
}

/// A mapping without MAP_FIXED never fails because the place the kernel
/// would pick is already taken: not for its own (unhinted) choice and not for
/// a hint. The kernel's unhinted addresses come from one system-wide counter
/// that only grows, so a reservation a process put just above it with a hint
/// (SpiderMonkey and mozjemalloc reserve at random addresses) used to be hit
/// a little later by that process's unhinted mmaps, which then failed with
/// ENOMEM until the counter had passed it. In Firefox that was the WebRender
/// thread mapping a virgl buffer: GL_OUT_OF_MEMORY, a WebRender device reset,
/// and a page that stopped repainting (lane ffpaint, 2026-10-14).
/// Anonymous, file-backed private and tmpfs shared mappings each take the
/// path this covers (the DRM device path uses the same placement).
unsafe fn test_mmap_skips_occupied_ranges() -> bool {
    let name = b"mmap_skips_occupied_ranges\0";
    const MAP_NORESERVE: i32 = 0x4000;
    const MIB: usize = 1 << 20;
    const RES: usize = 64 * MIB;
    // Where the kernel's next unhinted mapping would go.
    let probe = mmap(core::ptr::null_mut(), PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if probe as isize == -1 { return report(name, false); }
    munmap(probe, PAGE);
    // A reservation right in its path, placed by hint.
    let want = probe.add(16 * PAGE);
    let res = mmap(want, RES, 0, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    if res as isize == -1 { return report(name, false); }
    if res != want {
        // Something else already sat there; the setup did not happen.
        puts(b"  mmap_skips_occupied: reservation not at its hint, check skipped\0".as_ptr());
        munmap(res, RES);
        return report(name, true);
    }
    let in_res = |p: *mut u8, len: usize| {
        let (a, b) = (p as usize, p as usize + len);
        a < res as usize + RES && b > res as usize
    };
    let file = open(b"/bin/memtest\0".as_ptr(), 0, 0);
    let path = b"/tmp/memtest-occupied\0";
    let shm = open(path.as_ptr(), 0x40 | 0x2 | 0x200 /* O_CREAT|O_RDWR|O_TRUNC */, 0o600);
    if shm >= 0 {
        let page = [0x5Au8; PAGE];
        write(shm, page.as_ptr(), PAGE);
    }
    let mut ok = file >= 0 && shm >= 0;
    let mut maps: [(usize, usize); 96] = [(0, 0); 96];
    let mut n = 0;
    // 96 x 1 MiB walks the counter well past the 64 MiB reservation.
    for i in 0..96 {
        let (p, len) = match i % 3 {
            0 => (mmap(core::ptr::null_mut(), MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0), MIB),
            1 => (mmap(core::ptr::null_mut(), MIB, PROT_READ, MAP_PRIVATE, file, 0), MIB),
            _ => (mmap(core::ptr::null_mut(), PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, shm, 0), PAGE),
        };
        if p as isize == -1 {
            puts(b"  mmap_skips_occupied: an unhinted mmap failed\0".as_ptr());
            ok = false;
            break;
        }
        if in_res(p, len) {
            puts(b"  mmap_skips_occupied: an unhinted mmap landed in the reservation\0".as_ptr());
            ok = false;
        }
        maps[n] = (p as usize, len);
        n += 1;
    }
    // A hint inside the reservation is only a hint: placed elsewhere.
    let h = mmap(res.add(RES / 2), PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if h as isize == -1 || in_res(h, PAGE) {
        puts(b"  mmap_skips_occupied: a hint into the reservation was not moved\0".as_ptr());
        ok = false;
    } else {
        core::ptr::write_volatile(h, 0x33);
        if core::ptr::read_volatile(h) != 0x33 { ok = false; }
        munmap(h, PAGE);
    }
    for &(p, len) in &maps[..n] { munmap(p as *mut u8, len); }
    munmap(res, RES);
    if file >= 0 { close(file); }
    if shm >= 0 { close(shm); unlink(path.as_ptr()); }
    report(name, ok)
}

/// MAP_FIXED_NOREPLACE maps exactly at its address or fails with EEXIST; it
/// never unmaps what is there and never moves elsewhere. It used to be a plain
/// hint (now that a taken hint is placed elsewhere, that would hand back an
/// address the caller did not ask for).
unsafe fn test_mmap_fixed_noreplace() -> bool {
    let name = b"mmap_fixed_noreplace\0";
    const MAP_FIXED_NOREPLACE: i32 = 0x10_0000;
    const EEXIST: i32 = 17;
    let mut ok = true;
    let a = mmap(core::ptr::null_mut(), 2 * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if a as isize == -1 { return report(name, false); }
    core::ptr::write_volatile(a, 0x44);
    // Taken (the second page overlaps): EEXIST, and the old mapping survives.
    let p = mmap(a.add(PAGE), 2 * PAGE, PROT_READ | PROT_WRITE,
                 MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
    if p as isize != -1 || *__errno_location() != EEXIST {
        puts(b"  fixed_noreplace: an occupied address did not fail with EEXIST\0".as_ptr());
        if p as isize != -1 && p != a.add(PAGE) { munmap(p, 2 * PAGE); }
        ok = false;
    }
    if core::ptr::read_volatile(a) != 0x44 { ok = false; }
    // Free: exactly there.
    munmap(a, 2 * PAGE);
    let p = mmap(a, PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
    if p != a {
        puts(b"  fixed_noreplace: a free address was not honoured\0".as_ptr());
        ok = false;
    }
    if p as isize != -1 {
        core::ptr::write_volatile(p, 0x55);
        if core::ptr::read_volatile(p) != 0x55 { ok = false; }
        munmap(p, PAGE);
    }
    report(name, ok)
}

unsafe fn test_big_lazy_reservations() -> bool {
    let name = b"big_lazy_reservations\0";
    unsafe fn child() -> i32 {
        const MAP_NORESERVE: i32 = 0x4000;
        const MAP_FIXED: i32 = 0x10;
        const GIB: usize = 1 << 30;
        let rss0 = proc_kb(0, b"status", b"RssAnon:").unwrap_or(0);
        // The JIT reservation, and one committed 64 KiB chunk near its top.
        let jit = mmap(core::ptr::null_mut(), 0x7FC0_0000, 0,
                       MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
        if jit as isize == -1 { return 10; }
        let chunk = jit.add(0x7F00_0000);
        let c = mmap(chunk, 0x1_0000, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
        if c != chunk { return 11; }
        core::ptr::write_volatile(chunk.add(0x1_0000 - 1), 0x5A);
        if core::ptr::read_volatile(chunk.add(0x1_0000 - 1)) != 0x5A { return 12; }
        // A 4 GiB read-write mapping touched only at both ends.
        let big = mmap(core::ptr::null_mut(), 4 * GIB, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
        if big as isize == -1 { return 13; }
        core::ptr::write_volatile(big, 1);
        core::ptr::write_volatile(big.add(4 * GIB - 1), 2);
        if core::ptr::read_volatile(big) != 1 || core::ptr::read_volatile(big.add(4 * GIB - 1)) != 2 {
            return 14;
        }
        let rss1 = proc_kb(0, b"status", b"RssAnon:").unwrap_or(0);
        say_kb(b"  big_lazy rss_anon_kib before=", rss0);
        say_kb(b" after=", rss1);
        write(STDOUT_FILENO, b"\n".as_ptr(), 1);
        // Three touched pages (the chunk's last, and the two ends): allow slack
        // for the stack and allocator, but nothing near the spans' size.
        if rss1 > rss0 + 1024 { return 15; }
        if munmap(big, 4 * GIB) != 0 || munmap(jit, 0x7FC0_0000) != 0 { return 16; }
        // A private file mapping far past both the file and the old 256 MiB cap.
        let fd = open(b"/bin/memtest\0".as_ptr(), 0, 0);
        if fd < 0 { return 17; }
        let f = mmap(core::ptr::null_mut(), 300 << 20, PROT_READ, MAP_PRIVATE, fd, 0);
        close(fd);
        if f as isize == -1 { return 18; }
        let elf = core::ptr::read_volatile(f) == 0x7f && core::ptr::read_volatile(f.add(1)) == b'E';
        munmap(f, 300 << 20);
        if !elf { return 19; }
        100
    }
    let s = in_child(child);
    write(STDOUT_FILENO, b"  big_lazy status".as_ptr(), 17);
    write(STDOUT_FILENO, b" ".as_ptr(), 1); print_dec(s as usize);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    report(name, s & 0x7f == 0 && (s >> 8) & 0xff == 100)
}

/// EL0 cache maintenance on aarch64 (SCTLR_EL1.UCT/UCI/DZE), the sequence a
/// JIT runs: read CTR_EL0 for line sizes, write code, mprotect it RX, clean
/// the D-cache and invalidate the I-cache by VA, then call it. DC ZVA is
/// checked against DCZID_EL0, and DC CVAU on untouched pages of an RW and an
/// RX mapping must be served as reads (ISS.CM), not refused as writes.
#[cfg(target_arch = "aarch64")]
unsafe fn test_el0_cache_maintenance() -> bool {
    let name = b"el0_cache_maintenance\0";
    unsafe fn child() -> i32 {
        const PROT_EXEC: i32 = 4;
        let ctr: u64; let dczid: u64;
        core::arch::asm!("mrs {}, ctr_el0", out(reg) ctr);
        core::arch::asm!("mrs {}, dczid_el0", out(reg) dczid);
        if dczid & (1 << 4) != 0 { return 20; } // DZP: DC ZVA prohibited
        let zva = 4usize << (dczid & 0xf);
        let dline = 4usize << ((ctr >> 16) & 0xf);
        let iline = 4usize << (ctr & 0xf);
        if dline < 16 || iline < 16 || zva < 16 || zva > PAGE { return 21; }
        let p = mmap(core::ptr::null_mut(), 4 * PAGE, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if p as isize == -1 { return 22; }
        // DC ZVA zeroes exactly one block.
        for i in 0..PAGE { core::ptr::write_volatile(p.add(i), 0xFF); }
        core::arch::asm!("dc zva, {}", in(reg) p);
        for i in 0..zva { if core::ptr::read_volatile(p.add(i)) != 0 { return 23; } }
        if zva < PAGE && core::ptr::read_volatile(p.add(zva)) != 0xFF { return 24; }
        // DC CVAU / IC IVAU on an untouched RW page.
        core::arch::asm!("dc cvau, {0}", "dsb ish", "ic ivau, {0}", "dsb ish", "isb", in(reg) p.add(PAGE));
        // JIT: `mov w0, #42; ret`, made executable, flushed by line, called.
        let code = p.add(2 * PAGE) as *mut u32;
        core::ptr::write_volatile(code, 0x5280_0540);
        core::ptr::write_volatile(code.add(1), 0xd65f_03c0);
        if mprotect_raw(p.add(2 * PAGE), 2 * PAGE, PROT_READ | PROT_EXEC) != 0 { return 25; }
        let mut a = code as usize & !(dline - 1);
        while a < code as usize + 8 { core::arch::asm!("dc cvau, {}", in(reg) a); a += dline; }
        core::arch::asm!("dsb ish");
        let mut a = code as usize & !(iline - 1);
        while a < code as usize + 8 { core::arch::asm!("ic ivau, {}", in(reg) a); a += iline; }
        core::arch::asm!("dsb ish", "isb");
        let f: extern "C" fn() -> i32 = core::mem::transmute(code);
        if f() != 42 { return 26; }
        // Untouched page of the RX mapping: a read-permission CM fault.
        core::arch::asm!("dc cvau, {0}", "dsb ish", "ic ivau, {0}", "dsb ish", "isb", in(reg) p.add(3 * PAGE));
        munmap(p, 4 * PAGE);
        100
    }
    let s = in_child(child);
    write(STDOUT_FILENO, b"  cachemaint status".as_ptr(), 19);
    write(STDOUT_FILENO, b" ".as_ptr(), 1); print_dec(s as usize);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    report(name, s & 0x7f == 0 && (s >> 8) & 0xff == 100)
}

#[cfg(not(target_arch = "aarch64"))]
unsafe fn test_el0_cache_maintenance() -> bool { true }

// ── x86-64 RFLAGS.DF on kernel entry (2026-10-02) ───────────────────────────
//
// The SysV ABI only promises DF=0 at call boundaries. Inside musl's backward
// memmove (`std; rep movsb; cld`) it is 1, and a timer tick, a reschedule IPI
// or a page fault on the next destination page enters the kernel right there.
// Interrupt gates do not clear DF and SYSCALL clears only the FMASK bits, so
// the kernel has to `cld` itself: it is compiled with DF=0 assumed, and LLVM
// turns struct copies into `rep movsq`/`rep movsl`, which with DF=1 copy
// downwards — from below the source into below the destination. Firefox on
// x86_64 died a different way on every run (a jump to PC 0, a NULL read in
// libxul, Mesa's NIR compiler reading an instruction type that was not
// there); aarch64 has no direction flag and never crashed.
//
// Three ways in, each checked against results taken with DF=0:
//   * syscalls issued with DF=1 (pread, fstat, uname, a socketpair round trip);
//   * demand-paging faults taken with DF=1 (file-backed reads, CoW writes);
//   * processes spinning with DF=1 so that the timer preempts them in that
//     state, while this one keeps running syscalls and checks them.

#[cfg(target_arch = "x86_64")]
unsafe fn sys_df(n: usize, a0: usize, a1: usize, a2: usize, a3: usize) -> isize {
    let r: isize;
    core::arch::asm!("std", "syscall", "cld",
        inlateout("rax") n as isize => r,
        in("rdi") a0, in("rsi") a1, in("rdx") a2, in("r10") a3,
        lateout("rcx") _, lateout("r11") _, options(nostack));
    r
}

#[cfg(target_arch = "x86_64")]
unsafe fn sys_raw(n: usize, a0: usize, a1: usize, a2: usize, a3: usize) -> isize {
    let r: isize;
    core::arch::asm!("syscall",
        inlateout("rax") n as isize => r,
        in("rdi") a0, in("rsi") a1, in("rdx") a2, in("r10") a3,
        lateout("rcx") _, lateout("r11") _, options(nostack));
    r
}

/// The syscall checks, issued through `sys` (DF=1 or DF=0). Returns a
/// failure code, 0 when everything matched the DF=0 reference.
#[cfg(target_arch = "x86_64")]
unsafe fn df_syscall_round(sys: unsafe fn(usize, usize, usize, usize, usize) -> isize,
                           fd: i32, refbuf: *const u8, reflen: usize,
                           refstat: &[u8; 144], refuts: &[u8; 390]) -> i32 {
    const SYS_READ: usize = 0; const SYS_WRITE: usize = 1; const SYS_FSTAT: usize = 5;
    const SYS_PREAD64: usize = 17; const SYS_UNAME: usize = 63;
    let mut buf = [0u8; 8192];
    // Guard bytes on both sides of every output buffer: a backwards copy
    // writes below its destination.
    let mut st = [0x5Au8; 144 + 64];
    let mut uts = [0x5Au8; 390 + 64];
    for round in 0..64usize {
        let off = (round * 1237) % (reflen - 4096);
        let n = sys(SYS_PREAD64, fd as usize, buf.as_mut_ptr() as usize + 32, 4096, off);
        if n != 4096 { return 1; }
        if memcmp(buf.as_ptr().add(32), refbuf.add(off), 4096) != 0 { return 2; }
        for i in 0..32 { if buf[i] != 0 { return 3; } }
        for b in st.iter_mut() { *b = 0x5A; }
        if sys(SYS_FSTAT, fd as usize, st.as_mut_ptr() as usize + 32, 0, 0) != 0 { return 4; }
        // st_atime (bytes 72..88) may move between calls.
        if st[32..32 + 72] != refstat[..72] || st[32 + 88..32 + 144] != refstat[88..] { return 5; }
        if st[..32].iter().any(|&b| b != 0x5A) || st[176..].iter().any(|&b| b != 0x5A) { return 6; }
        for b in uts.iter_mut() { *b = 0x5A; }
        if sys(SYS_UNAME, uts.as_mut_ptr() as usize + 32, 0, 0, 0) != 0 { return 7; }
        if uts[32..32 + 390] != refuts[..] { return 8; }
        if uts[..32].iter().any(|&b| b != 0x5A) || uts[422..].iter().any(|&b| b != 0x5A) { return 9; }
        // A socketpair round trip: the net server's request/reply structs.
        let mut sv = [0i32; 2];
        if sys(53 /* socketpair */, 1 /* AF_UNIX */, 1 /* SOCK_STREAM */, 0, sv.as_mut_ptr() as usize) != 0 { return 10; }
        let msg = refbuf.add(off / 2);
        if sys(SYS_WRITE, sv[0] as usize, msg as usize, 1000, 0) != 1000 { return 11; }
        for b in buf[..1100].iter_mut() { *b = 0; }
        let mut got = 0usize;
        while got < 1000 {
            let r = sys(SYS_READ, sv[1] as usize, buf.as_mut_ptr() as usize + 32 + got, 1000 - got, 0);
            if r <= 0 { return 12; }
            got += r as usize;
        }
        if memcmp(buf.as_ptr().add(32), msg, 1000) != 0 { return 13; }
        close(sv[0]); close(sv[1]);
    }
    0
}

unsafe fn test_direction_flag_kernel_entry() -> bool { df_check(0b1110) }

#[cfg(target_arch = "x86_64")]
unsafe fn df_check(parts: u32) -> bool {
    let name = b"direction_flag_kernel_entry\0";
    let fd = open_big_file();
    if fd < 0 { return report(name, false); }
    let size = file_size(fd);
    let reflen = if size > 256 * 1024 { 256 * 1024 } else { size & !(PAGE - 1) };
    if reflen < 8 * PAGE { close(fd); return report(name, false); }
    let refbuf = malloc(reflen);
    if !pread_all(fd, 0, refbuf, reflen) { close(fd); return report(name, false); }
    let mut refstat = [0u8; 144];
    let mut refuts = [0u8; 390];
    if sys_raw(5, fd as usize, refstat.as_mut_ptr() as usize, 0, 0) != 0
        || sys_raw(63, refuts.as_mut_ptr() as usize, 0, 0, 0) != 0 {
        close(fd); return report(name, false);
    }
    let mut ok = true;

    // 1. Syscalls entered with DF=1.
    let c1 = if parts & 2 == 0 { 0 } else { df_syscall_round(sys_df, fd, refbuf, reflen, &refstat, &refuts) };
    if c1 != 0 { ok = false; write(STDOUT_FILENO, b"  syscall with DF=1: code ".as_ptr(), 26); print_dec(c1 as usize); write(STDOUT_FILENO, b"\n".as_ptr(), 1); }

    // 2. Page faults taken with DF=1: file-backed reads, then CoW writes in a
    //    fork child (the parent keeps the original bytes).
    let len = reflen;
    let p = if parts & 4 == 0 { core::ptr::null_mut() } else { mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0) };
    if p as isize == -1 { ok = false; } else if !p.is_null() {
        let mut pg = len / PAGE;
        while pg > 0 {
            pg -= 1;
            let a = p.add(pg * PAGE + 7);
            let v: u8;
            core::arch::asm!("std", "mov {v}, byte ptr [{a}]", "cld", a = in(reg) a, v = out(reg_byte) v, options(nostack));
            if v != *refbuf.add(pg * PAGE + 7) { ok = false; puts(b"  fault read with DF=1: wrong byte\0".as_ptr()); break; }
        }
        if memcmp(p, refbuf, len) != 0 { ok = false; puts(b"  fault read with DF=1: page content\0".as_ptr()); }
        let pid = fork();
        if pid == 0 {
            let mut bad = 0;
            for pg in 0..len / PAGE {
                let a = p.add(pg * PAGE + 11);
                core::arch::asm!("std", "mov byte ptr [{a}], {v}", "cld", a = in(reg) a, v = in(reg_byte) 0xC3u8, options(nostack));
            }
            for pg in 0..len / PAGE {
                for i in 0..PAGE {
                    let want = if i == 11 { 0xC3 } else { *refbuf.add(pg * PAGE + i) };
                    if *p.add(pg * PAGE + i) != want { bad = 1; }
                }
            }
            exit(bad);
        }
        let mut status: i32 = -1;
        wait4(pid, &mut status as *mut i32, 0, core::ptr::null_mut());
        if status != 0 { ok = false; puts(b"  CoW write with DF=1: child copy wrong\0".as_ptr()); }
        if memcmp(p, refbuf, len) != 0 { ok = false; puts(b"  CoW write with DF=1: parent changed\0".as_ptr()); }
        munmap(p, len);
    }

    // 3. Preempted with DF=1: spinners that never clear it, while this
    //    process runs the syscall checks with DF=0 on the same CPUs.
    let mut kids = [0i32; 6];
    for k in kids.iter_mut() {
        if parts & 8 == 0 { break; }
        let pid = fork();
        if pid == 0 {
            loop {
                core::arch::asm!("std", "mov ecx, 0x4000000", "2:", "dec ecx", "jnz 2b", "cld",
                                 out("ecx") _, options(nostack, nomem));
            }
        }
        *k = pid;
    }
    let t0 = now_ns();
    let mut c3 = 0;
    while parts & 8 != 0 && c3 == 0 && now_ns() - t0 < 3_000_000_000 {
        c3 = df_syscall_round(sys_raw, fd, refbuf, reflen, &refstat, &refuts);
    }
    for &k in kids.iter() {
        if k > 0 {
            sys_raw(62 /* kill */, k as usize, 9, 0, 0);
            let mut st: i32 = 0;
            wait4(k, &mut st as *mut i32, 0, core::ptr::null_mut());
        }
    }
    if c3 != 0 { ok = false; write(STDOUT_FILENO, b"  next to DF=1 spinners: code ".as_ptr(), 30); print_dec(c3 as usize); write(STDOUT_FILENO, b"\n".as_ptr(), 1); }

    free(refbuf);
    close(fd);
    report(name, ok)
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn df_check(_parts: u32) -> bool { true }

// ── madvise(MADV_DONTNEED) really drops pages (2026-10-02) ──────────────────
//
// It was a no-op. Scudo (Firefox's allocator on Alpine) releases idle
// secondary-cache blocks with MADV_DONTNEED and then serves calloc() from
// such a block without a memset, because on Linux a released private page
// reads back as zeros. Here it read back as the old contents: hash tables
// with live-looking slots and a NULL key, double frees ("Scudo ERROR:
// invalid chunk state"), a parent crash a few seconds into every busy page.
//
// Linux semantics checked: private anonymous pages read zero afterwards,
// private file pages read the file again (private writes discarded), shared
// anonymous pages keep their data, a fork child's DONTNEED does not touch the
// parent's copy, the pages are writable again, an unaligned address is
// EINVAL and an unmapped range ENOMEM.

#[cfg(target_arch = "x86_64")]
const SYS_MADVISE_NR: usize = 28;
#[cfg(target_arch = "aarch64")]
const SYS_MADVISE_NR: usize = 233;
const MADV_DONTNEED: usize = 4;

unsafe fn madvise_raw(p: *mut u8, len: usize, advice: usize) -> isize {
    syscall3(SYS_MADVISE_NR, p as usize, len, advice)
}

unsafe fn test_madvise_dontneed() -> bool {
    let name = b"madvise_dontneed\0";
    let mut ok = true;
    let say = |m: &[u8]| { puts(m.as_ptr()); };

    // 1. Private anonymous: the dropped pages read zero, the rest is kept.
    let n = 8;
    let a = mmap(core::ptr::null_mut(), n * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if a as isize == -1 { return report(name, false); }
    for i in 0..n * PAGE { *a.add(i) = 0xAB; }
    if madvise_raw(a.add(2 * PAGE), 3 * PAGE, MADV_DONTNEED) != 0 { ok = false; say(b"  anon: madvise failed\0"); }
    for pg in 0..n {
        let want = if (2..5).contains(&pg) { 0 } else { 0xAB };
        for i in 0..PAGE {
            if core::ptr::read_volatile(a.add(pg * PAGE + i)) != want { ok = false; say(b"  anon: wrong byte after DONTNEED\0"); break; }
        }
    }
    *a.add(3 * PAGE + 5) = 0x11;
    if core::ptr::read_volatile(a.add(3 * PAGE + 5)) != 0x11 { ok = false; say(b"  anon: page not writable again\0"); }

    // 2. Fork: the child's DONTNEED leaves the parent's copy alone.
    for i in 0..PAGE { *a.add(6 * PAGE + i) = 0xCD; }
    let pid = fork();
    if pid == 0 {
        if madvise_raw(a.add(6 * PAGE), PAGE, MADV_DONTNEED) != 0 { exit(2); }
        for i in 0..PAGE { if core::ptr::read_volatile(a.add(6 * PAGE + i)) != 0 { exit(3); } }
        exit(0);
    }
    let mut status: i32 = -1;
    wait4(pid, &mut status as *mut i32, 0, core::ptr::null_mut());
    if status != 0 { ok = false; say(b"  fork: child did not see zeros\0"); }
    for i in 0..PAGE { if *a.add(6 * PAGE + i) != 0xCD { ok = false; say(b"  fork: parent copy changed\0"); break; } }

    // 3. Errors: unaligned address, unmapped range.
    if madvise_raw(a.add(1), PAGE, MADV_DONTNEED) != -22 { ok = false; say(b"  unaligned not EINVAL\0"); }
    munmap(a, n * PAGE);
    if madvise_raw(a, PAGE, MADV_DONTNEED) != -12 { ok = false; say(b"  unmapped not ENOMEM\0"); }

    // 4. Shared anonymous: the data lives in the shared object and stays.
    let s = mmap(core::ptr::null_mut(), 2 * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if s as isize == -1 { ok = false; } else {
        for i in 0..2 * PAGE { *s.add(i) = 0x77; }
        if madvise_raw(s, 2 * PAGE, MADV_DONTNEED) != 0 { ok = false; say(b"  shared: madvise failed\0"); }
        for i in 0..2 * PAGE { if *s.add(i) != 0x77 { ok = false; say(b"  shared: data lost\0"); break; } }
        munmap(s, 2 * PAGE);
    }

    // 5. Private file mapping: a private write is discarded, the file's
    //    bytes come back.
    let fd = open_big_file();
    if fd < 0 { ok = false; } else {
        let p = mmap(core::ptr::null_mut(), 4 * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
        let buf = malloc(PAGE);
        if p as isize == -1 || !pread_all(fd, PAGE, buf, PAGE) { ok = false; } else {
            let orig = *p.add(PAGE + 9);
            *p.add(PAGE + 9) = orig ^ 0xFF;
            if madvise_raw(p.add(PAGE), PAGE, MADV_DONTNEED) != 0 { ok = false; say(b"  file: madvise failed\0"); }
            if memcmp(p.add(PAGE), buf, PAGE) != 0 { ok = false; say(b"  file: private write survived\0"); }
            munmap(p, 4 * PAGE);
        }
        free(buf);
        close(fd);
    }
    report(name, ok)
}

// ── Shared page cache (2026-10-02, lane ffmem) ───────────────────────────────
//
// Clean pages of private file mappings are shared between every process
// that maps the same file (mm/src/pagecache.rs). These check that sharing
// really happens, that it never lets one mapping's writes reach another, that
// file changes are seen by later mappings, and that the memory comes back.

#[cfg(target_arch = "x86_64")]
const SYS_FTRUNCATE_NR: usize = 77;
#[cfg(target_arch = "aarch64")]
const SYS_FTRUNCATE_NR: usize = 46;

fn pc_byte(seed: usize, pg: usize, i: usize) -> u8 { (seed + pg * 7 + i % 251) as u8 }

/// Create `path` with `pages` pages of a per-page pattern; returns an O_RDWR fd.
unsafe fn pc_make_file(path: &[u8], pages: usize, seed: usize) -> i32 {
    let fd = open(path.as_ptr(), 0x40 | 0x2 | 0x200 /* O_CREAT|O_RDWR|O_TRUNC */, 0o600);
    if fd < 0 { return fd; }
    let buf = malloc(PAGE);
    for pg in 0..pages {
        for i in 0..PAGE { *buf.add(i) = pc_byte(seed, pg, i); }
        if write(fd, buf, PAGE) != PAGE as isize { free(buf); close(fd); return -1; }
    }
    free(buf);
    fd
}

/// Does page `pg` of the mapping at `p` hold the pattern?
unsafe fn pc_page_ok(p: *mut u8, seed: usize, pg: usize) -> bool {
    for i in (0..PAGE).step_by(61) {
        if core::ptr::read_volatile(p.add(pg * PAGE + i)) != pc_byte(seed, pg, i) { return false; }
    }
    true
}

unsafe fn pc_touch(p: *mut u8, pages: usize) {
    for pg in 0..pages { let _ = core::ptr::read_volatile(p.add(pg * PAGE)); }
}

unsafe fn wait_status(pid: i32) -> i32 {
    let mut status: i32 = -1;
    if pid > 0 { wait4(pid, &mut status as *mut i32, 0, core::ptr::null_mut()); }
    status
}

/// A second process mapping a file the first already touched must not pay
/// for the pages again: they come from the page cache.
unsafe fn test_pagecache_shared_across_processes() -> bool {
    let name = b"pagecache_shared_across_processes\0";
    const PAGES: usize = 512;
    let path = b"/root/.memtest-pc-share\0";
    let fd = pc_make_file(path, PAGES, 3);
    if fd < 0 { return report(name, false); }
    let a = mmap(core::ptr::null_mut(), PAGES * PAGE, PROT_READ, MAP_PRIVATE, fd, 0);
    if a as isize == -1 { close(fd); unlink(path.as_ptr()); return report(name, false); }
    pc_touch(a, PAGES);
    let pid = fork();
    if pid == 0 {
        // A fresh mapping of its own, not the inherited one.
        let fd2 = open(path.as_ptr(), 0, 0);
        let before = free_ram();
        let b = mmap(core::ptr::null_mut(), PAGES * PAGE, PROT_READ, MAP_PRIVATE, fd2, 0);
        if b as isize == -1 { exit(2); }
        pc_touch(b, PAGES);
        let used = before.saturating_sub(free_ram()) / PAGE;
        write(STDOUT_FILENO, b"  second_mapping_new_pages=".as_ptr(), 27); print_dec(used);
        write(STDOUT_FILENO, b" of ".as_ptr(), 4); print_dec(PAGES);
        write(STDOUT_FILENO, b"\n".as_ptr(), 1);
        for pg in 0..PAGES { if !pc_page_ok(b, 3, pg) { exit(3); } }
        exit(if used < PAGES / 4 { 0 } else { 4 });
    }
    let status = wait_status(pid);
    let mut ok = status == 0;
    for pg in 0..PAGES { if !pc_page_ok(a, 3, pg) { ok = false; puts(b"  first mapping changed\0".as_ptr()); break; } }
    munmap(a, PAGES * PAGE);
    close(fd);
    unlink(path.as_ptr());
    write(STDOUT_FILENO, b"  child_status=".as_ptr(), 15); print_dec(status as usize);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    report(name, ok)
}

/// Writes to a shared clean page stay in the writer's mapping: another
/// mapping, a fresh mapping, read(2) and a fork child's sibling all keep the
/// file's bytes. A kernel store into a read-only mapping must not reach the
/// shared frame either.
unsafe fn test_pagecache_cow_isolation() -> bool {
    let name = b"pagecache_cow_isolation\0";
    const PAGES: usize = 16;
    let path = b"/root/.memtest-pc-cow\0";
    let fd = pc_make_file(path, PAGES, 11);
    if fd < 0 { return report(name, false); }
    let len = PAGES * PAGE;
    let m1 = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    let m2 = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    let ro = mmap(core::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0);
    if m1 as isize == -1 || m2 as isize == -1 || ro as isize == -1 {
        close(fd); unlink(path.as_ptr()); return report(name, false);
    }
    let mut ok = true;
    let say = |m: &[u8]| { puts(m.as_ptr()); };
    pc_touch(m1, PAGES); pc_touch(m2, PAGES); pc_touch(ro, PAGES);
    // 1. A user store in m1.
    *m1.add(3 * PAGE + 5) = 0xEE;
    if *m1.add(3 * PAGE + 5) != 0xEE { ok = false; say(b"  m1 store lost\0"); }
    if !pc_page_ok(m2, 11, 3) { ok = false; say(b"  m1 store visible in m2\0"); }
    if !pc_page_ok(ro, 11, 3) { ok = false; say(b"  m1 store visible in ro\0"); }
    // 2. A fork child stores into m2.
    let pid = fork();
    if pid == 0 { *m2.add(4 * PAGE + 9) = 0xDD; exit(if *m2.add(4 * PAGE + 9) == 0xDD { 0 } else { 1 }); }
    if wait_status(pid) != 0 { ok = false; say(b"  child store failed\0"); }
    if !pc_page_ok(m2, 11, 4) || !pc_page_ok(m1, 11, 4) { ok = false; say(b"  child store leaked\0"); }
    // 3. A kernel store (read(2)) into the read-only mapping: refused, or at
    //    least never visible to anyone else.
    let src = open(b"/bin/memtest\0".as_ptr(), 0, 0);
    let r = if src >= 0 { read(src, ro.add(6 * PAGE), 64) } else { -1 };
    if src >= 0 { close(src); }
    write(STDOUT_FILENO, b"  read_into_ro=".as_ptr(), 15); print_dec(r as usize);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    if !pc_page_ok(m1, 11, 6) || !pc_page_ok(m2, 11, 6) { ok = false; say(b"  kernel store into ro reached the shared page\0"); }
    // 4. read(2) into a writable mapping's clean page: private to it.
    let src = open(b"/bin/memtest\0".as_ptr(), 0, 0);
    if src >= 0 { read(src, m1.add(7 * PAGE), 64); close(src); }
    if !pc_page_ok(m2, 11, 7) { ok = false; say(b"  kernel store into m1 visible in m2\0"); }
    // 5. The file and a fresh mapping still hold the original bytes.
    let m3 = mmap(core::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0);
    if m3 as isize == -1 { ok = false; } else {
        for pg in 0..PAGES { if !pc_page_ok(m3, 11, pg) { ok = false; say(b"  fresh mapping sees a private store\0"); break; } }
        munmap(m3, len);
    }
    let mut b = 0u8;
    if !pread_all(fd, 3 * PAGE + 5, &mut b, 1) || b != pc_byte(11, 3, 5) { ok = false; say(b"  private store reached the file\0"); }
    munmap(m1, len); munmap(m2, len); munmap(ro, len);
    close(fd);
    unlink(path.as_ptr());
    report(name, ok)
}

/// A file's change is seen by mappings made after it: write(2), truncate
/// (pages past the new end raise SIGBUS), and pages of an unlinked file
/// still read through a mapping that outlives the name.
unsafe fn test_pagecache_write_truncate_unlink() -> bool {
    let name = b"pagecache_write_truncate_unlink\0";
    const PAGES: usize = 64;
    let path = b"/root/.memtest-pc-wt\0";
    let fd = pc_make_file(path, PAGES, 21);
    if fd < 0 { return report(name, false); }
    let len = PAGES * PAGE;
    let mut ok = true;
    let say = |m: &[u8]| { puts(m.as_ptr()); };
    let m1 = mmap(core::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0);
    if m1 as isize == -1 { close(fd); unlink(path.as_ptr()); return report(name, false); }
    pc_touch(m1, PAGES);
    // write(2) over page 2.
    let buf = malloc(PAGE);
    memset(buf, 0xA5, PAGE);
    lseek(fd, (2 * PAGE) as _, SEEK_SET);
    if write(fd, buf, PAGE) != PAGE as isize { ok = false; say(b"  write failed\0"); }
    let m2 = mmap(core::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0);
    if m2 as isize == -1 { ok = false; } else {
        if memcmp(m2.add(2 * PAGE), buf, PAGE) != 0 { ok = false; say(b"  new mapping missed write(2)\0"); }
        if !pc_page_ok(m2, 21, 3) { ok = false; say(b"  neighbour page wrong\0"); }
        munmap(m2, len);
    }
    // Truncate to 10 pages.
    if syscall2(SYS_FTRUNCATE_NR, fd as usize, 10 * PAGE) != 0 { ok = false; say(b"  ftruncate failed\0"); }
    let m3 = mmap(core::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0);
    if m3 as isize == -1 { ok = false; } else {
        if !pc_page_ok(m3, 21, 5) { ok = false; say(b"  page below new end wrong\0"); }
        let pid = fork();
        if pid == 0 { let _ = core::ptr::read_volatile(m3.add(20 * PAGE)); exit(4); }
        let st = wait_status(pid);
        if st & 0x7f != 7 { ok = false; say(b"  page past new end did not SIGBUS\0"); }
        munmap(m3, len);
    }
    // Unlink while mapped: m4 maps the 10 remaining pages, the name goes,
    // other files churn the freed blocks; untouched pages still read right.
    let m4 = mmap(core::ptr::null_mut(), 10 * PAGE, PROT_READ, MAP_PRIVATE, fd, 0);
    close(fd);
    unlink(path.as_ptr());
    let other = b"/root/.memtest-pc-wt2\0";
    let fd2 = pc_make_file(other, PAGES, 99);
    if fd2 >= 0 { close(fd2); unlink(other.as_ptr()); }
    if m4 as isize == -1 { ok = false; } else {
        let mut pg = 10;
        while pg > 0 { pg -= 1; if pg != 2 && !pc_page_ok(m4, 21, pg) { ok = false; say(b"  unlinked file page wrong\0"); break; } }
        munmap(m4, 10 * PAGE);
    }
    munmap(m1, len);
    free(buf);
    report(name, ok)
}

/// MAP_SHARED of an f2fs file keeps its old semantics (a copy made at map
/// time) and its stores never reach the page cache that private mappings
/// share: they, a fresh mapping and read(2) agree.
unsafe fn test_pagecache_map_shared_unchanged() -> bool {
    let name = b"pagecache_map_shared_unchanged\0";
    const PAGES: usize = 8;
    let path = b"/root/.memtest-pc-shared\0";
    let fd = pc_make_file(path, PAGES, 31);
    if fd < 0 { return report(name, false); }
    let len = PAGES * PAGE;
    let mut ok = true;
    let a = mmap(core::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0);
    let s = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if a as isize == -1 || s as isize == -1 { close(fd); unlink(path.as_ptr()); return report(name, false); }
    pc_touch(a, PAGES);
    for pg in 0..PAGES { if !pc_page_ok(s, 31, pg) { ok = false; puts(b"  shared mapping content wrong\0".as_ptr()); break; } }
    *s.add(PAGE + 1) = 0x42;
    if *s.add(PAGE + 1) != 0x42 { ok = false; }
    let mut b = 0u8;
    let file_b = if pread_all(fd, PAGE + 1, &mut b, 1) { b } else { ok = false; 0 };
    let c = mmap(core::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0);
    if c as isize == -1 { ok = false; } else {
        if *c.add(PAGE + 1) != file_b || *a.add(PAGE + 1) != file_b {
            ok = false; puts(b"  private mappings disagree with read(2)\0".as_ptr());
        }
        munmap(c, len);
    }
    munmap(a, len); munmap(s, len);
    close(fd);
    unlink(path.as_ptr());
    report(name, ok)
}

/// When the last mapping of a file goes, its cached pages are freed.
unsafe fn test_pagecache_memory_returns() -> bool {
    let name = b"pagecache_memory_returns\0";
    const PAGES: usize = 1024;
    let path = b"/root/.memtest-pc-ret\0";
    let fd = pc_make_file(path, PAGES, 41);
    if fd < 0 { return report(name, false); }
    close(fd);
    let before = free_ram();
    let pid = fork();
    if pid == 0 {
        let f = open(path.as_ptr(), 0, 0);
        let p = mmap(core::ptr::null_mut(), PAGES * PAGE, PROT_READ, MAP_PRIVATE, f, 0);
        if p as isize == -1 { exit(2); }
        close(f);
        pc_touch(p, PAGES);
        let ok = pc_page_ok(p, 41, PAGES - 1);
        // A second mapping in another child, while this one still holds it.
        let pid2 = fork();
        if pid2 == 0 { exit(if pc_page_ok(p, 41, 7) { 0 } else { 1 }); }
        let st = wait_status(pid2);
        exit(if ok && st == 0 { 0 } else { 3 });
    }
    let status = wait_status(pid);
    unlink(path.as_ptr());
    let after = free_ram();
    let lost = before.saturating_sub(after) / PAGE;
    write(STDOUT_FILENO, b"  file_pages=".as_ptr(), 13); print_dec(PAGES);
    write(STDOUT_FILENO, b" lost_pages=".as_ptr(), 12); print_dec(lost);
    write(STDOUT_FILENO, b" child_status=".as_ptr(), 14); print_dec(status as usize);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    report(name, status == 0 && lost < 64)
}


// ── NUMA memory policy on one node (lane ytfreeze, 2026-10-03) ─────────────
// libnuma (linked by libx265 <- libavcodec) probes get_mempolicy in its
// constructor; Linux on a one-node machine answers MPOL_DEFAULT.

unsafe fn test_mempolicy_single_node() -> bool {
    #[cfg(target_arch = "x86_64")]
    const NRS: (usize, usize, usize) = (239, 238, 237);
    #[cfg(target_arch = "aarch64")]
    const NRS: (usize, usize, usize) = (236, 237, 235);
    #[allow(non_snake_case)]
    let (GET, SET, MBIND) = NRS;
    use leandros_libc::syscall::syscall6;
    let mut ok = true;
    let mut mode: i32 = -1;
    let mut mask = [0xFFu64; 2];
    // Default policy: mode 0, empty mask.
    let r = syscall5(GET, &mut mode as *mut i32 as usize, mask.as_mut_ptr() as usize, 128, 0, 0);
    ok &= r == 0 && mode == 0 && mask == [0, 0];
    // MPOL_F_MEMS_ALLOWED: node 0 only.
    let r = syscall5(GET, &mut mode as *mut i32 as usize, mask.as_mut_ptr() as usize, 128, 0, 4);
    ok &= r == 0 && mask == [1, 0];
    // MPOL_F_NODE|MPOL_F_ADDR: the node of an address, 0.
    mode = -1;
    let r = syscall5(GET, &mut mode as *mut i32 as usize, 0, 0, &mode as *const i32 as usize, 3);
    ok &= r == 0 && mode == 0;
    // Unknown flag: EINVAL.
    ok &= syscall5(GET, &mut mode as *mut i32 as usize, 0, 0, 0, 0x100) as isize == -22;
    // BIND to node 0 accepted, to node 1 refused, back to DEFAULT.
    let n0: u64 = 1; let n1: u64 = 2;
    ok &= syscall3(SET, 2, &n0 as *const u64 as usize, 64) == 0;
    ok &= syscall3(SET, 2, &n1 as *const u64 as usize, 64) as isize == -22;
    ok &= syscall3(SET, 0, 0, 0) == 0;
    // mbind of a mapped, page-aligned range (this stack page) on node 0.
    let page = (mask.as_ptr() as usize) & !0xFFF;
    ok &= syscall6(MBIND, page, 4096, 2, &n0 as *const u64 as usize, 64, 0) == 0;
    report(b"mempolicy_single_node\0", ok)
}

/// A long-lived process that keeps mapping and unmapping must stay bounded.
///
/// Until 2026-10-14 every unhinted mmap took its address from a system-wide
/// bump cursor that never went back, so a process cycling 1-8 MiB buffers
/// (Firefox decoding video) walked an ever-higher window, paying fresh page
/// tables at every step: ~10 KiB of MemFree per cycle, back only at exit.
/// 5000 cycles of varying sizes, touched every 64 KiB, interleaved with a
/// MAP_FIXED remap of the just-freed range and an mremap grow; the bars are
/// the VA high-water mark (a few times the largest buffer, where the bump
/// walked ~22 GiB) and MemFree drift (the leak was ~50 MiB).
unsafe fn test_mmap_reuses_freed_va() -> bool {
    let name = b"mmap_reuses_freed_va\0";
    const MAP_FIXED: i32 = 0x10;
    const MREMAP_MAYMOVE: i32 = 1;
    const MIB: usize = 1 << 20;
    const ITERS: usize = 5000;
    const WARM: usize = 200;
    const STRIDE: usize = 64 << 10;
    let mut lo = usize::MAX;
    let mut hi = 0usize;
    let mut hi_warm = 0usize;
    let mut free_warm = 0usize;
    let mut bad = 0usize;
    let mut reused = 0usize;
    let mut last = 0usize;
    for i in 0..ITERS {
        // 1..=8 MiB plus 0..=4 pages, in an order that is not monotonic.
        let len = (1 + (i * 5) % 8) * MIB + ((i * 3) % 5) * PAGE;
        let p = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if p as isize == -1 { bad += 1; break; }
        let a = p as usize;
        if a == last { reused += 1; }
        last = a;
        lo = lo.min(a);
        hi = hi.max(a + len);
        let mut off = 0usize;
        while off < len { core::ptr::write_volatile(p.add(off), (i + off / STRIDE) as u8); off += STRIDE; }
        core::ptr::write_volatile(p.add(len - 1), 0xA5);
        if core::ptr::read_volatile(p) != i as u8 || core::ptr::read_volatile(p.add(len - 1)) != 0xA5 { bad += 1; }
        match i % 16 {
            // MAP_FIXED back over the range just freed: it must land exactly
            // there and read as fresh zeros.
            5 => {
                munmap(p, len);
                let q = mmap(p, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
                if q != p { bad += 1; if q as isize != -1 { munmap(q, len); } continue; }
                if core::ptr::read_volatile(q.add(len - 1)) != 0 { bad += 1; }
                core::ptr::write_volatile(q.add(len / 2), 7);
                munmap(q, len);
            }
            // mremap grow (may move): the content comes along.
            11 => {
                let q = mremap(p, len, len + MIB, MREMAP_MAYMOVE);
                if q as isize == -1 { bad += 1; munmap(p, len); continue; }
                hi = hi.max(q as usize + len + MIB);
                if core::ptr::read_volatile(q) != i as u8 || core::ptr::read_volatile(q.add(len - 1)) != 0xA5 { bad += 1; }
                core::ptr::write_volatile(q.add(len + MIB - 1), 1);
                munmap(q, len + MIB);
            }
            _ => { munmap(p, len); }
        }
        if i + 1 == WARM { hi_warm = hi; free_warm = free_ram(); }
    }
    // The same length, mapped right after it was freed, comes back in place.
    let a = mmap(core::ptr::null_mut(), 3 * MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    let mut same_again = false;
    if a as isize != -1 {
        munmap(a, 3 * MIB);
        let b = mmap(core::ptr::null_mut(), 3 * MIB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        same_again = b == a;
        if b as isize != -1 { munmap(b, 3 * MIB); }
    }
    let free_end = free_ram();
    let drift = free_warm.saturating_sub(free_end);
    let span = hi.saturating_sub(lo);
    say_kb(b"  vmreuse iters=", ITERS);
    say_kb(b" span_kib=", span >> 10);
    say_kb(b" hiwater_growth_kib=", hi.saturating_sub(hi_warm) >> 10);
    say_kb(b" memfree_drift_kib=", drift >> 10);
    say_kb(b" reused=", reused);
    say_kb(b" bad=", bad);
    say_kb(b" same_again=", same_again as usize);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    let ok = bad == 0 && same_again
        && span <= 1024 * MIB
        && hi.saturating_sub(hi_warm) <= 16 * MIB
        && drift < 4 * MIB;
    report(name, ok)
}

/// munmap must return the page-table pages a range leaves empty.
///
/// Each round maps 64 small regions 1 GiB apart at fresh addresses (one PD
/// and one PT apiece once touched), touches them and unmaps them; 40 rounds
/// at ever-new addresses held ~20 MiB of page tables until exit before
/// unmap_range freed empty tables, the bar is a tenth of that.
unsafe fn test_munmap_frees_page_tables() -> bool {
    let name = b"munmap_frees_page_tables\0";
    const GIB: usize = 1 << 30;
    const ROUNDS: usize = 40;
    const PER: usize = 64;
    const BASE: usize = 0x2000_0000_0000;
    let mut bad = 0usize;
    let mut free0 = 0usize;
    for r in 0..ROUNDS {
        if r == 1 { free0 = free_ram(); }
        let mut got = [0usize; PER];
        for k in 0..PER {
            let want = BASE + (r * PER + k) * GIB;
            let p = mmap(want as *mut u8, 2 * PAGE, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if p as isize == -1 { bad += 1; continue; }
            if p as usize != want { bad += 1; }
            core::ptr::write_volatile(p, k as u8);
            core::ptr::write_volatile(p.add(PAGE), r as u8);
            got[k] = p as usize;
        }
        for k in 0..PER {
            if got[k] == 0 { continue; }
            let p = got[k] as *mut u8;
            if core::ptr::read_volatile(p) != k as u8 { bad += 1; }
            munmap(p, 2 * PAGE);
        }
    }
    let lost = free0.saturating_sub(free_ram());
    say_kb(b"  ptprune rounds=", ROUNDS);
    say_kb(b" lost_kib=", lost >> 10);
    say_kb(b" bad=", bad);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    report(name, bad == 0 && lost < 2 << 20)
}

// ── Truncate under a mapping, mremap, /proc/<pid>/maps (lane mremap, 2026-10-19) ──

#[cfg(target_arch = "x86_64")]
const SYS_MEMFD_CREATE_NR: usize = 319;
#[cfg(target_arch = "aarch64")]
const SYS_MEMFD_CREATE_NR: usize = 279;
const MREMAP_MAYMOVE: usize = 1;
const MREMAP_FIXED: usize = 2;
const MREMAP_DONTUNMAP: usize = 4;

/// Termination signal of a child that runs `f` (0: it exited normally).
unsafe fn child_signal(f: impl FnOnce()) -> i32 {
    let pid = fork();
    if pid == 0 { f(); exit(0); }
    wait_status(pid) & 0x7f
}

/// Write `byte` over page `pg` of `fd` through write(2).
unsafe fn fd_fill_page(fd: i32, pg: usize, byte: u8) -> bool {
    let buf = malloc(PAGE);
    memset(buf, byte as i32, PAGE);
    let ok = lseek(fd, (pg * PAGE) as _, SEEK_SET) >= 0 && write(fd, buf, PAGE) == PAGE as isize;
    free(buf);
    ok
}

/// Shrinking a mapped file takes the pages past the new end from every
/// mapping: touching one afterwards is SIGBUS, for MAP_SHARED and for
/// MAP_PRIVATE (a written, copy-on-write page included), on f2fs and on a
/// memfd, as on Linux. The partial last page and everything below stay.
/// Until 2026-10-19 the mapper kept a detached frame and read stale data.
unsafe fn test_truncate_mapped_sigbus() -> bool {
    let name = b"truncate_mapped_sigbus\0";
    let say = |m: &[u8]| { puts(m.as_ptr()); };
    let mut ok = true;
    const PAGES: usize = 16;
    let len = PAGES * PAGE;
    let new_size = 4 * PAGE + 100;

    // f2fs: a shared and a private mapping, every page resident.
    let path = b"/root/.memtest-trunc\0";
    let fd = pc_make_file(path, PAGES, 41);
    if fd < 0 { return report(name, false); }
    let s = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    let p = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    if s as isize == -1 || p as isize == -1 { close(fd); unlink(path.as_ptr()); return report(name, false); }
    pc_touch(s, PAGES);
    pc_touch(p, PAGES);
    *p.add(12 * PAGE) = 0x99; // a private copy-on-write page past the new end
    if syscall2(SYS_FTRUNCATE_NR, fd as usize, new_size) != 0 { ok = false; say(b"  ftruncate failed\0"); }
    let sig_s = child_signal(|| { let _ = core::ptr::read_volatile(s.add(8 * PAGE)); });
    let sig_p = child_signal(|| { let _ = core::ptr::read_volatile(p.add(12 * PAGE)); });
    let sig_s5 = child_signal(|| { let _ = core::ptr::read_volatile(s.add(5 * PAGE)); });
    if sig_s != 7 { ok = false; say(b"  f2fs shared page past the end did not SIGBUS\0"); }
    if sig_p != 7 { ok = false; say(b"  f2fs private page past the end did not SIGBUS\0"); }
    if sig_s5 != 7 { ok = false; say(b"  f2fs page just past the partial last page did not SIGBUS\0"); }
    if !pc_page_ok(s, 41, 2) || !pc_page_ok(p, 41, 3) { ok = false; say(b"  page below the new end changed\0"); }
    // Partial last page: the file's bytes, then zeros.
    if *s.add(4 * PAGE + 99) != pc_byte(41, 4, 99) || *s.add(4 * PAGE + 100) != 0 {
        ok = false; say(b"  partial last page wrong\0");
    }
    // Growing back: the shared mapping sees the file's new (zero) bytes.
    if syscall2(SYS_FTRUNCATE_NR, fd as usize, len) != 0 { ok = false; }
    if child_signal(|| { if *s.add(8 * PAGE) != 0 { exit(3); } }) != 0 {
        ok = false; say(b"  regrown page not readable as zeros\0");
    }
    munmap(s, len); munmap(p, len);
    close(fd);
    unlink(path.as_ptr());

    // memfd (tmpfs VMO, frames mapped at mmap time).
    let mfd = syscall2(SYS_MEMFD_CREATE_NR, b"memtest-trunc\0".as_ptr() as usize, 0) as i32;
    if mfd < 0 { return report(name, false); }
    if syscall2(SYS_FTRUNCATE_NR, mfd as usize, len) != 0 { ok = false; }
    let m = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    if m as isize == -1 { close(mfd); return report(name, false); }
    for pg in 0..PAGES { *m.add(pg * PAGE) = 0x40 + pg as u8; }
    if syscall2(SYS_FTRUNCATE_NR, mfd as usize, new_size) != 0 { ok = false; say(b"  memfd ftruncate failed\0"); }
    if child_signal(|| { let _ = core::ptr::read_volatile(m.add(9 * PAGE)); }) != 7 {
        ok = false; say(b"  memfd page past the end did not SIGBUS\0");
    }
    if *m.add(2 * PAGE) != 0x42 { ok = false; say(b"  memfd page below the end changed\0"); }
    // Still the object's frame: write(2) shows through the mapping.
    if !fd_fill_page(mfd, 1, 0x5C) || *m.add(PAGE + 7) != 0x5C {
        ok = false; say(b"  memfd mapping detached from its object\0");
    }
    munmap(m, len);
    close(mfd);
    report(name, ok)
}

/// The truncate follow-ups of 2026-10-19: open(O_TRUNC) empties a mapped
/// file like ftruncate(0) does, so its mappings SIGBUS (f2fs and tmpfs); and
/// a memfd/tmpfs page a truncate took away is the object's page again once
/// the file grows back (zeros, shared with write(2)), not SIGBUS forever.
unsafe fn test_truncate_followups() -> bool {
    let name = b"truncate_followups\0";
    let say = |m: &[u8]| { puts(m.as_ptr()); };
    let mut ok = true;
    const PAGES: usize = 8;
    let len = PAGES * PAGE;
    let o_rdwr_trunc = 0x2 | 0x200;

    // f2fs and tmpfs: open(O_TRUNC) under a shared and a private mapping.
    for path in [&b"/root/.memtest-otrunc\0"[..], &b"/tmp/.memtest-otrunc\0"[..]] {
        let fd = pc_make_file(path, PAGES, 23);
        if fd < 0 { ok = false; say(b"  create failed\0"); continue; }
        let sm = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        let pm = mmap(core::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0);
        if sm as isize == -1 || pm as isize == -1 { close(fd); unlink(path.as_ptr()); ok = false; continue; }
        pc_touch(sm, PAGES);
        pc_touch(pm, PAGES);
        let fd2 = open(path.as_ptr(), o_rdwr_trunc, 0o600);
        if fd2 < 0 { ok = false; say(b"  O_TRUNC reopen failed\0"); }
        if child_signal(|| { let _ = core::ptr::read_volatile(sm.add(2 * PAGE)); }) != 7 {
            ok = false; say(b"  shared page after open(O_TRUNC) did not SIGBUS\0");
        }
        // (A tmpfs MAP_PRIVATE mapping is a copy made at mmap time.)
        if path[1] == b'r' && child_signal(|| { let _ = core::ptr::read_volatile(pm); }) != 7 {
            ok = false; say(b"  private page after open(O_TRUNC) did not SIGBUS\0");
        }
        // Grown back: zeros, and the shared mapping is the file again.
        if fd2 >= 0 && syscall2(SYS_FTRUNCATE_NR, fd2 as usize, len) != 0 { ok = false; }
        if child_signal(|| { if *sm.add(3 * PAGE + 5) != 0 { exit(3); } }) != 0 {
            ok = false; say(b"  regrown page after O_TRUNC not zeros\0");
        }
        if fd2 >= 0 { close(fd2); }
        munmap(sm, len); munmap(pm, len);
        close(fd);
        unlink(path.as_ptr());
    }

    // memfd: a page dropped by a shrink comes back when the file grows.
    let mfd = syscall2(SYS_MEMFD_CREATE_NR, b"memtest-regrow\0".as_ptr() as usize, 0) as i32;
    if mfd < 0 { return report(name, false); }
    if syscall2(SYS_FTRUNCATE_NR, mfd as usize, len) != 0 { ok = false; }
    let m = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    if m as isize == -1 { close(mfd); return report(name, false); }
    for pg in 0..PAGES { *m.add(pg * PAGE) = 0x30 + pg as u8; }
    if syscall2(SYS_FTRUNCATE_NR, mfd as usize, 2 * PAGE) != 0 { ok = false; }
    if child_signal(|| { let _ = core::ptr::read_volatile(m.add(5 * PAGE)); }) != 7 {
        ok = false; say(b"  memfd page past the end did not SIGBUS\0");
    }
    if syscall2(SYS_FTRUNCATE_NR, mfd as usize, len) != 0 { ok = false; }
    if child_signal(|| { if *m.add(5 * PAGE) != 0 { exit(3); } }) != 0 {
        ok = false; say(b"  regrown memfd page not readable as zeros\0");
    } else {
        // The refaulted page is the object's: a store shows in read(2)...
        *m.add(5 * PAGE + 9) = 0x77;
        let mut b = [0u8; 16];
        if !pread_all(mfd, 5 * PAGE, b.as_mut_ptr(), 16) || b[9] != 0x77 {
            ok = false; say(b"  regrown memfd page detached from its object\0");
        }
        // ...and write(2) shows through the mapping.
        if !fd_fill_page(mfd, 6, 0x5D) || *m.add(6 * PAGE + 3) != 0x5D {
            ok = false; say(b"  memfd write(2) not visible in the regrown mapping\0");
        }
    }
    if *m.add(PAGE) != 0x31 { ok = false; say(b"  memfd page below the end changed\0"); }
    munmap(m, len);
    close(mfd);
    report(name, ok)
}

/// mremap with Linux semantics: MREMAP_FIXED moves to exactly `new_addr`,
/// unmapping what was there; it needs MAYMOVE and may not overlap the old
/// range; the old range is gone after a move; MREMAP_DONTUNMAP leaves it
/// mapped and empty; and a grown memfd mapping still shares the object with
/// its other mappers. Until 2026-10-19 the FIXED target was a hint and the
/// grown memfd mapping a private copy.
unsafe fn test_mremap_linux_semantics() -> bool {
    let name = b"mremap_linux_semantics\0";
    let say = |m: &[u8]| { puts(m.as_ptr()); };
    let mut ok = true;
    let anon = |n: usize| mmap(core::ptr::null_mut(), n * PAGE, PROT_READ | PROT_WRITE,
                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);

    // FIXED onto a live mapping.
    let a = anon(2);
    let b = anon(4);
    if a as isize == -1 || b as isize == -1 { return report(name, false); }
    memset(a, 0x61, 2 * PAGE);
    memset(b, 0x62, 4 * PAGE);
    let r = syscall5(nr::MREMAP, a as usize, 2 * PAGE, 2 * PAGE, MREMAP_MAYMOVE | MREMAP_FIXED, b as usize);
    if r != b as isize { ok = false; say(b"  FIXED did not land on new_addr\0"); }
    else {
        if *b != 0x61 || *b.add(2 * PAGE - 1) != 0x61 { ok = false; say(b"  moved content wrong\0"); }
        if *b.add(2 * PAGE) != 0x62 { ok = false; say(b"  rest of the target mapping lost\0"); }
        if child_signal(|| { let _ = core::ptr::read_volatile(a); }) != 11 {
            ok = false; say(b"  old range still mapped after the move\0");
        }
    }
    // FIXED without MAYMOVE, and overlapping ranges: EINVAL.
    let e1 = syscall5(nr::MREMAP, b as usize, PAGE, PAGE, MREMAP_FIXED, b as usize + 8 * PAGE);
    let e2 = syscall5(nr::MREMAP, b as usize, 2 * PAGE, 2 * PAGE, MREMAP_MAYMOVE | MREMAP_FIXED, b as usize + PAGE);
    if e1 != -22 || e2 != -22 { ok = false; say(b"  bad FIXED requests not EINVAL\0"); }
    munmap(b, 4 * PAGE);

    // Grow-move of a private mapping keeps the content (pages move, no copy).
    let c = anon(2);
    let blocker = if c as isize != -1 {
        mmap(c.add(2 * PAGE), PAGE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS | 0x10_0000 /* NOREPLACE */, -1, 0)
    } else { c };
    if c as isize == -1 { return report(name, false); }
    memset(c, 0x63, 2 * PAGE);
    let q = syscall5(nr::MREMAP, c as usize, 2 * PAGE, 6 * PAGE, MREMAP_MAYMOVE, 0) as *mut u8;
    if (q as isize) < 0 { ok = false; say(b"  grow-move failed\0"); }
    else {
        if *q != 0x63 || *q.add(2 * PAGE - 1) != 0x63 || *q.add(5 * PAGE) != 0 { ok = false; say(b"  grow-move content wrong\0"); }
        munmap(q, 6 * PAGE);
    }
    if blocker as isize != -1 { munmap(blocker, PAGE); }

    // DONTUNMAP: the pages move, the old range stays mapped and reads zero.
    let d = anon(2);
    if d as isize == -1 { return report(name, false); }
    memset(d, 0x64, 2 * PAGE);
    let t = syscall5(nr::MREMAP, d as usize, 2 * PAGE, 2 * PAGE, MREMAP_MAYMOVE | MREMAP_DONTUNMAP, 0) as *mut u8;
    if (t as isize) < 0 { ok = false; say(b"  DONTUNMAP failed\0"); }
    else {
        if *t != 0x64 || *t.add(PAGE) != 0x64 { ok = false; say(b"  DONTUNMAP content wrong\0"); }
        if child_signal(|| { if core::ptr::read_volatile(d) != 0 { exit(3); } }) != 0 {
            ok = false; say(b"  DONTUNMAP old range not mapped and empty\0");
        }
        munmap(t, 2 * PAGE);
    }
    munmap(d, 2 * PAGE);

    // memfd: grow a 2-page mapping to 8 pages after the file grew (what a
    // Wayland compositor does on wl_shm_pool.resize); it must keep sharing
    // the object with the client's mapping both ways.
    let mfd = syscall2(SYS_MEMFD_CREATE_NR, b"memtest-mremap\0".as_ptr() as usize, 0) as i32;
    if mfd < 0 { return report(name, false); }
    syscall2(SYS_FTRUNCATE_NR, mfd as usize, 2 * PAGE);
    let m1 = mmap(core::ptr::null_mut(), 2 * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    if m1 as isize == -1 { close(mfd); return report(name, false); }
    *m1 = 0x11;
    // Something right after it, so the grow has to move.
    let block2 = mmap(m1.add(2 * PAGE), PAGE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS | 0x10_0000, -1, 0);
    syscall2(SYS_FTRUNCATE_NR, mfd as usize, 8 * PAGE);
    let client = mmap(core::ptr::null_mut(), 8 * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    let m2 = syscall5(nr::MREMAP, m1 as usize, 2 * PAGE, 8 * PAGE, MREMAP_MAYMOVE, 0) as *mut u8;
    if client as isize == -1 || (m2 as isize) < 0 { ok = false; say(b"  memfd grow failed\0"); }
    else {
        *client.add(6 * PAGE + 3) = 0x77;   // client writes the grown part
        *m2.add(PAGE + 5) = 0x78;           // the grown mapping writes the old part
        if *m2 != 0x11 { ok = false; say(b"  memfd grow lost content\0"); }
        if *m2.add(6 * PAGE + 3) != 0x77 { ok = false; say(b"  grown memfd tail not shared\0"); }
        if *client.add(PAGE + 5) != 0x78 { ok = false; say(b"  grown memfd mapping turned private\0"); }
        let mut byte = 0u8;
        if !pread_all(mfd, 6 * PAGE + 3, &mut byte, 1) || byte != 0x77 { ok = false; say(b"  memfd file misses the store\0"); }
        munmap(m2, 8 * PAGE);
        munmap(client, 8 * PAGE);
    }
    if block2 as isize != -1 { munmap(block2, PAGE); }
    close(mfd);

    // A private file mapping moved and grown still reads the file.
    let fd = open(b"/bin/memtest\0".as_ptr(), 0, 0);
    if fd >= 0 && file_size(fd) > 8 * PAGE {
        let f = mmap(core::ptr::null_mut(), 2 * PAGE, PROT_READ, MAP_PRIVATE, fd, 0);
        let blk = mmap(f.add(2 * PAGE), PAGE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS | 0x10_0000, -1, 0);
        let _ = core::ptr::read_volatile(f);
        let g = syscall5(nr::MREMAP, f as usize, 2 * PAGE, 6 * PAGE, MREMAP_MAYMOVE, 0) as *mut u8;
        let buf = malloc(PAGE);
        if (g as isize) < 0 { ok = false; say(b"  file mapping grow failed\0"); }
        else {
            for pg in [0usize, 5] {
                if !pread_all(fd, pg * PAGE, buf, PAGE) || memcmp(g.add(pg * PAGE), buf, PAGE) != 0 {
                    ok = false; say(b"  moved file mapping content wrong\0"); break;
                }
            }
            munmap(g, 6 * PAGE);
        }
        free(buf);
        if blk as isize != -1 { munmap(blk, PAGE); }
    }
    if fd >= 0 { close(fd); }
    report(name, ok)
}

/// Does `hay` contain `needle`?
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || hay.windows(needle.len()).any(|w| w == needle)
}

/// `/proc/<pid>/maps` lists every mapping, named as on Linux, and is never
/// cut short: the executable and file mappings by path, a memfd as
/// `/memfd:<name> (deleted)`, `[stack]` and `[heap]`; another process's
/// file reads the same way; `smaps` carries the same names. It used to read
/// as 0 bytes (a stub), and smaps named nothing but the heap and stack.
unsafe fn test_proc_maps_names() -> bool {
    let name = b"proc_maps_names\0";
    let say = |m: &[u8]| { puts(m.as_ptr()); };
    let mut ok = true;
    // A heap, a file mapping, a memfd mapping, and many small mappings.
    let cur = leandros_libc::syscall::syscall1(nr::BRK, 0) as usize;
    leandros_libc::syscall::syscall1(nr::BRK, cur + 4 * PAGE);
    let path = b"/root/.memtest-maps\0";
    let fd = pc_make_file(path, 2, 5);
    let f = mmap(core::ptr::null_mut(), 2 * PAGE, PROT_READ, MAP_PRIVATE, fd, 0);
    let mfd = syscall2(SYS_MEMFD_CREATE_NR, b"memtest-maps\0".as_ptr() as usize, 0) as i32;
    syscall2(SYS_FTRUNCATE_NR, mfd as usize, PAGE);
    let m = mmap(core::ptr::null_mut(), PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    const MANY: usize = 3000;
    let many = mmap(core::ptr::null_mut(), MANY * 2 * PAGE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if many as isize != -1 {
        // Every other page made PROT_NONE: MANY + 1 separate mappings.
        for i in 0..MANY {
            leandros_libc::syscall::syscall3(SYS_MPROTECT_NR, many as usize + (2 * i + 1) * PAGE, PAGE, 0);
        }
    }
    let cap = 4 << 20;
    let buf = malloc(cap);
    let all = core::slice::from_raw_parts_mut(buf, cap);
    let n = slurp(b"/proc/self/maps\0", all);
    let text = &all[..n];
    let lines = text.iter().filter(|&&b| b == b'\n').count();
    write(STDOUT_FILENO, b"  maps_bytes=".as_ptr(), 13); print_dec(n);
    write(STDOUT_FILENO, b" lines=".as_ptr(), 7); print_dec(lines);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    if lines < MANY { ok = false; say(b"  maps cut short\0"); }
    for (want, what) in [(&b"/bin/memtest"[..], &b"  no executable path\0"[..]),
                         (b"/root/.memtest-maps", b"  no file mapping path\0"),
                         (b"/memfd:memtest-maps (deleted)", b"  no memfd name\0"),
                         (b"[stack]", b"  no [stack]\0"),
                         (b"[heap]", b"  no [heap]\0")] {
        if !contains(text, want) { ok = false; say(what); }
    }
    // The first line has Linux's shape: "start-end perms offset dev inode".
    let first = text.split(|&b| b == b'\n').next().unwrap_or(b"");
    let fields = first.split(|&b| b == b' ').filter(|w| !w.is_empty()).count();
    if fields < 5 || !first.contains(&b'-') { ok = false; say(b"  malformed line\0"); }
    // Another process's maps and smaps.
    let child = fork();
    if child == 0 {
        let ts = [1usize, 0usize];
        loop { syscall2(nr::NANOSLEEP, ts.as_ptr() as usize, 0); }
    }
    let mut pp = [0u8; 64];
    proc_path(child, b"maps", &mut pp);
    let n2 = slurp(&pp, all);
    if n2 == 0 || !contains(&all[..n2], b"/root/.memtest-maps") { ok = false; say(b"  other process maps empty or unnamed\0"); }
    proc_path(child, b"smaps", &mut pp);
    let n3 = slurp(&pp, all);
    if !contains(&all[..n3], b"/bin/memtest") || !contains(&all[..n3], b"Rss:") { ok = false; say(b"  smaps unnamed\0"); }
    syscall2(nr::KILL, child as usize, 9);
    wait_status(child);
    free(buf);
    if many as isize != -1 { munmap(many, MANY * 2 * PAGE); }
    munmap(f, 2 * PAGE); munmap(m, PAGE);
    close(fd); close(mfd);
    unlink(path.as_ptr());
    leandros_libc::syscall::syscall1(nr::BRK, cur);
    report(name, ok)
}

// ── lane mmlock (2026-10-19): device copies, /proc observers, teardown ──────

unsafe fn raw_ioctl(fd: i32, cmd: usize, arg: usize) -> isize {
    syscall3(nr::IOCTL, fd as usize, cmd, arg) as isize
}

/// Run `f` in a fork child; true iff the child exited normally with 0 (a
/// child killed by a signal — the kernel's answer to a bad pointer inside a
/// raw kernel copy — is a failure).
unsafe fn in_child_rc(f: unsafe fn(usize) -> i32, arg: usize) -> (bool, i32) {
    let pid = fork();
    if pid == 0 { exit(f(arg)); }
    let mut st: i32 = 0;
    wait4(pid, &mut st as *mut i32, 0, core::ptr::null_mut());
    ((st & 0x7f) == 0 && ((st >> 8) & 0xff) == 0, st)
}

/// `EVIOCGID` (`_IOR('E', 0x02, struct input_id)`): an 8-byte copy-out by
/// the evdev server, through the pid the VFS hands it.
const EVIOCGID: usize = (2 << 30) | (8 << 16) | (0x45 << 8) | 0x02;

unsafe fn open_evdev() -> i32 {
    for p in [b"/dev/input/event0\0", b"/dev/input/event1\0"] {
        let fd = open(p.as_ptr(), 0x800 /* O_NONBLOCK */, 0);
        if fd >= 0 { return fd; }
    }
    -1
}

/// Child body: EVIOCGID into the page at `arg`; 0 iff the kernel said EFAULT.
unsafe fn evdev_efault_child(arg: usize) -> i32 {
    let fd = open_evdev();
    if fd < 0 { return 3; }
    let r = raw_ioctl(fd, EVIOCGID, arg + 64);
    if r == -14 { 0 } else if r >= 0 { 1 } else { 2 }
}

/// L1.11: evdev/DRM copy to and from user memory.
///
/// Destinations that are valid but not yet faulted (fresh anonymous pages, a
/// private file mapping) must be filled; destinations no access can make
/// valid (PROT_NONE, read-only) must answer EFAULT. Before the fix the copy
/// was a raw `copy_nonoverlapping` outside the fault-tolerant copy, so the
/// second case killed the caller with SIGSEGV.
unsafe fn test_evdev_copy_unfaulted() -> bool {
    let name = b"evdev_copy_unfaulted\0";
    let fd = open_evdev();
    if fd < 0 { puts(b"  no /dev/input/event*: skipped\0".as_ptr()); return report(name, true); }
    let mut want = [0u8; 8];
    let mut ok = raw_ioctl(fd, EVIOCGID, want.as_mut_ptr() as usize) >= 0 && want != [0u8; 8];
    // Fresh anonymous memory: the target page is not present.
    let p = mmap(core::ptr::null_mut(), 16 * PAGE, PROT_READ | PROT_WRITE,
                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if p as isize == -1 { close(fd); return report(name, false); }
    for k in [0usize, 5, 15] {
        let dst = p.add(k * PAGE + PAGE - 4); // straddles into the next page unless last
        let dst = if k == 15 { p.add(k * PAGE + 100) } else { dst };
        if raw_ioctl(fd, EVIOCGID, dst as usize) < 0 { ok = false; puts(b"  anon: ioctl failed\0".as_ptr()); continue; }
        for i in 0..8 { if *dst.add(i) != want[i] { ok = false; } }
    }
    munmap(p, 16 * PAGE);
    // A private file mapping, never touched: the copy faults the file page in
    // and then breaks copy-on-write.
    let ffd = open_big_file();
    if ffd >= 0 {
        let len = (file_size(ffd) & !(PAGE - 1)).min(8 * PAGE);
        let q = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_PRIVATE, ffd, 0);
        if q as isize != -1 && len >= 4 * PAGE {
            let dst = q.add(3 * PAGE + 8);
            if raw_ioctl(fd, EVIOCGID, dst as usize) < 0 { ok = false; puts(b"  file: ioctl failed\0".as_ptr()); }
            for i in 0..8 { if *dst.add(i) != want[i] { ok = false; } }
            munmap(q, len);
        }
        close(ffd);
    }
    close(fd);
    // Bad destinations: EFAULT, not a dead process.
    for (prot, label) in [(0, b"  PROT_NONE\0"), (PROT_READ, b"  PROT_READ\0")] {
        let b = mmap(core::ptr::null_mut(), PAGE, prot, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if b as isize == -1 { ok = false; continue; }
        let (good, st) = in_child_rc(evdev_efault_child, b as usize);
        if !good {
            ok = false;
            puts(label.as_ptr());
            say_kb(b"    child status=", st as usize);
            write(STDOUT_FILENO, b" (want exit 0 = EFAULT)\n".as_ptr(), 24);
        }
        munmap(b, PAGE);
    }
    report(name, ok)
}

/// Child body for the observer test: churn the address space (faults and
/// big munmaps, each holding its `busy` flag) until killed.
unsafe fn as_churn_child(_arg: usize) -> i32 {
    const SZ: usize = 64 << 20;
    loop {
        let p = mmap(core::ptr::null_mut(), SZ, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if p as isize == -1 { continue; }
        let mut o = 0; while o < SZ { core::ptr::write_volatile(p.add(o), 1); o += PAGE; }
        munmap(p, SZ);
    }
}

/// L1.3: `/proc/<pid>/smaps_rollup`, `smaps` and `status` of a process that
/// keeps its address space busy must keep answering, promptly.
unsafe fn test_proc_reader_vs_busy() -> bool {
    let name = b"proc_reader_vs_busy\0";
    let pid = fork();
    if pid == 0 { exit(as_churn_child(0)); }
    let mut buf = [0u8; 4096];
    let files: [&[u8]; 3] = [b"smaps_rollup", b"smaps", b"status"];
    let t_end = now_ns() + 3_000_000_000;
    let (mut reads, mut empty, mut worst) = (0usize, 0usize, 0u64);
    while now_ns() < t_end {
        let mut path = [0u8; 64];
        proc_path(pid, files[reads % 3], &mut path);
        let t0 = now_ns();
        let n = slurp(&path, &mut buf);
        let dt = now_ns() - t0;
        worst = worst.max(dt);
        if n == 0 { empty += 1; }
        reads += 1;
    }
    syscall2(nr::KILL, pid as usize, 9);
    let mut st: i32 = 0;
    wait4(pid, &mut st as *mut i32, 0, core::ptr::null_mut());
    say_kb(b"  observer reads=", reads);
    say_kb(b" empty=", empty);
    say_kb(b" worst_us=", (worst / 1000) as usize);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    report(name, reads >= 30 && empty == 0 && worst < 1_000_000_000)
}

/// `late_tick_max_ms` from `/proc/sysstat`'s `# mm` line (reading resets
/// it); None on a kernel without the line.
unsafe fn late_tick_max_ms() -> Option<usize> {
    let mut buf = [0u8; 512];
    let n = slurp(b"/proc/sysstat\0", &mut buf);
    let key = b"late_tick_max_ms ";
    let line = buf[..n].split(|&b| b == b'\n').find(|l| l.starts_with(b"# mm "))?;
    let at = line.windows(key.len()).position(|w| w == key)?;
    parse_num(&line[at + key.len()..])
}

/// Shared page of the teardown test: [0] stop, [1] window open, [2..] max
/// gap per spinner (ns), [64] victim's exit start (ns).
unsafe fn spinner(shared: *mut u64, slot: usize) -> ! {
    let mut prev = now_ns();
    loop {
        if core::ptr::read_volatile(shared) != 0 { exit(0); }
        let t = now_ns();
        let gap = t - prev;
        prev = t;
        if core::ptr::read_volatile(shared.add(1)) != 0 && gap > core::ptr::read_volatile(shared.add(2 + slot)) {
            core::ptr::write_volatile(shared.add(2 + slot), gap);
        }
    }
}

/// L1.6: tearing down a multi-GiB address space must not hold its CPU with
/// IRQs masked. A victim touches half of free RAM (at most 10 GiB) and exits while 8 spinners compete
/// for 4 CPUs: whichever spinner shares the victim's CPU used to wait out
/// the whole teardown; with reschedule points it gets its turn at the next
/// tick.
unsafe fn test_teardown_irq_latency() -> bool {
    let name = b"teardown_irq_latency\0";
    const CHUNK: usize = 256 << 20;
    let free = free_ram();
    let chunks = (free / 2 / CHUNK).min(40);
    if chunks < 2 { puts(b"  not enough RAM: skipped\0".as_ptr()); return report(name, true); }
    let shared = mmap(core::ptr::null_mut(), PAGE, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0) as *mut u64;
    if shared as isize == -1 { return report(name, false); }
    const SPIN: usize = 8;
    let mut sp = [0i32; SPIN];
    for i in 0..SPIN { let p = fork(); if p == 0 { spinner(shared, i); } sp[i] = p; }
    let _ = late_tick_max_ms(); // reset
    let victim = fork();
    if victim == 0 {
        for _ in 0..chunks {
            let p = mmap(core::ptr::null_mut(), CHUNK, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if p as isize == -1 { exit(1); }
            let mut o = 0; while o < CHUNK { core::ptr::write_volatile(p.add(o), 1); o += PAGE; }
        }
        core::ptr::write_volatile(shared.add(64), now_ns());
        core::ptr::write_volatile(shared.add(1), 1);
        exit(0);
    }
    let mut st: i32 = 0;
    wait4(victim, &mut st as *mut i32, 0, core::ptr::null_mut());
    let t_reaped = now_ns();
    let late = late_tick_max_ms();
    core::ptr::write_volatile(shared.add(1), 0);
    core::ptr::write_volatile(shared, 1);
    for i in 0..SPIN { wait4(sp[i], core::ptr::null_mut(), 0, core::ptr::null_mut()); }
    let t_exit = core::ptr::read_volatile(shared.add(64));
    let teardown = if t_exit == 0 { 0 } else { t_reaped.saturating_sub(t_exit) };
    let mut worst = 0u64;
    for i in 0..SPIN { worst = worst.max(core::ptr::read_volatile(shared.add(2 + i))); }
    munmap(shared as *mut u8, PAGE);
    say_kb(b"  teardown mib=", chunks * 256);
    say_kb(b" exit_to_reap_ms=", (teardown / 1_000_000) as usize);
    say_kb(b" worst_spinner_gap_ms=", (worst / 1_000_000) as usize);
    match late {
        Some(l) => say_kb(b" late_tick_max_ms=", l),
        None => { write(STDOUT_FILENO, b" late_tick_max_ms=?".as_ptr(), 19); }
    }
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    let victim_ok = (st & 0x7f) == 0 && ((st >> 8) & 0xff) == 0;
    // The bars: no busy CPU went more than 250 ms without its tick (the
    // kernel's own measure of an IRQ-masked stretch; host load on the vCPU
    // reads as late ticks too, hence the margin), and no spinner waited
    // out most of the teardown.
    report(name, victim_ok && t_exit != 0 && late.map_or(true, |l| l <= 250)
                 && worst < 100_000_000.max(teardown / 2))
}

/// L1.13: fork() of a big resident address space and a /proc/self/smaps read
/// must not hold the AS lock (IRQs masked) for the whole O(pages) walk.
unsafe fn test_fork_smaps_irq_latency() -> bool {
    let name = b"fork_smaps_irq_latency\0";
    let free = free_ram();
    let len = (free / 4).min(1 << 30) & !(PAGE - 1);
    if len < (128 << 20) { puts(b"  not enough RAM: skipped\0".as_ptr()); return report(name, true); }
    let shared = mmap(core::ptr::null_mut(), PAGE, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0) as *mut u64;
    if shared as isize == -1 { return report(name, false); }
    let p = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if p as isize == -1 { return report(name, false); }
    let mut o = 0; while o < len { core::ptr::write_volatile(p.add(o), 1); o += PAGE; }
    const SPIN: usize = 8;
    let mut sp = [0i32; SPIN];
    for i in 0..SPIN { let c = fork(); if c == 0 { spinner(shared, i); } sp[i] = c; }
    let _ = late_tick_max_ms(); // reset
    core::ptr::write_volatile(shared.add(1), 1);
    let mut fork_max = 0u64;
    let mut ok = true;
    for _ in 0..5 {
        let t0 = now_ns();
        let c = fork();
        if c == 0 { exit(0); }
        let dt = now_ns() - t0;
        if dt > fork_max { fork_max = dt; }
        if c < 0 { ok = false; continue; }
        wait4(c, core::ptr::null_mut(), 0, core::ptr::null_mut());
    }
    let sbuf = mmap(core::ptr::null_mut(), 1 << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    let mut smaps_max = 0u64;
    if sbuf as isize != -1 {
        let sl = core::slice::from_raw_parts_mut(sbuf, 1 << 20);
        for _ in 0..5 {
            let t0 = now_ns();
            let n = slurp(b"/proc/self/smaps\0", sl);
            let dt = now_ns() - t0;
            if dt > smaps_max { smaps_max = dt; }
            if n == 0 { ok = false; }
        }
    }
    let late = late_tick_max_ms();
    core::ptr::write_volatile(shared.add(1), 0);
    core::ptr::write_volatile(shared, 1);
    for i in 0..SPIN { wait4(sp[i], core::ptr::null_mut(), 0, core::ptr::null_mut()); }
    let mut worst = 0u64;
    for i in 0..SPIN { worst = worst.max(core::ptr::read_volatile(shared.add(2 + i))); }
    munmap(p, len);
    say_kb(b"  fork mib=", len >> 20);
    say_kb(b" fork_ms_max=", (fork_max / 1_000_000) as usize);
    say_kb(b" smaps_ms_max=", (smaps_max / 1_000_000) as usize);
    say_kb(b" worst_spinner_gap_ms=", (worst / 1_000_000) as usize);
    match late {
        Some(l) => say_kb(b" late_tick_max_ms=", l),
        None => { write(STDOUT_FILENO, b" late_tick_max_ms=?".as_ptr(), 19); }
    }
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    // 50 ms: a 1 GiB single-hold fork measured late_tick_max_ms=70 (HVF) unfixed.
    report(name, ok && late.map_or(true, |l| l <= 50))
}

// ── mincore holes and chunked munmap (lane mmrest, 2026-10-20) ──

#[cfg(target_arch = "x86_64")]
const SYS_MINCORE_NR: usize = 27;
#[cfg(target_arch = "aarch64")]
const SYS_MINCORE_NR: usize = 232;

/// A 48 MiB private mapping is touched, then partly munmapped in pieces that
/// cross the 8 MiB chunk size of the kernel's chunked munmap. mincore over a
/// range with a hole must fail ENOMEM and still have written the bytes before
/// the hole; the pages around the munmapped ranges keep their data; the full
/// munmap gives every page back.
unsafe fn test_mincore_hole_huge_munmap() -> bool {
    let name = b"mincore_hole_huge_munmap\0";
    const MIB: usize = 1 << 20;
    const LEN: usize = 48 * MIB;
    let pages = LEN / PAGE;
    let mut bad = 0usize;
    let free0 = free_ram();
    let p = mmap(core::ptr::null_mut(), LEN, PROT_READ | PROT_WRITE,
                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) as *mut u8;
    if p as isize == -1 { return report(name, false); }
    for i in 0..pages { core::ptr::write_volatile(p.add(i * PAGE), (i % 251) as u8 + 1); }
    let vec = mmap(core::ptr::null_mut(), pages, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) as *mut u8;
    if vec as isize == -1 { munmap(p as _, LEN); return report(name, false); }
    // The kernel copier does not fault in lazy user pages: touch the vec first.
    for i in 0..pages { core::ptr::write_volatile(vec.add(i), 0); }
    // 1) whole range resident.
    let r = syscall3(SYS_MINCORE_NR, p as usize, LEN, vec as usize) as isize;
    let mut res = 0usize;
    for i in 0..pages { if *vec.add(i) & 1 != 0 { res += 1; } }
    if r != 0 || res != pages { bad += 1; }
    // 2) hole at page 3000 (past several 256-page mincore holds).
    const HOLE: usize = 3000;
    munmap(p.add(HOLE * PAGE) as _, 4 * PAGE);
    for i in 0..pages { *vec.add(i) = 0; }
    let r = syscall3(SYS_MINCORE_NR, p as usize, LEN, vec as usize) as isize;
    if r != -12 { bad += 2; }
    let mut before = 0usize;
    for i in 0..HOLE { if *vec.add(i) & 1 != 0 { before += 1; } }
    if before != HOLE { bad += 4; }
    // 3) munmap 20 MiB starting off a chunk boundary (crosses 3 chunks).
    // Placed after the page-3000 hole so the sentinels below stay mapped.
    let cut = 20 * MIB + 3 * PAGE;
    munmap(p.add(cut) as _, 20 * MIB);
    let r = syscall3(SYS_MINCORE_NR, p as usize + cut, PAGE, vec as usize) as isize;
    if r != -12 { bad += 8; }
    for &i in &[0usize, HOLE - 1, HOLE + 4, cut / PAGE - 1, (cut + 20 * MIB) / PAGE, pages - 1] {
        if core::ptr::read_volatile(p.add(i * PAGE)) != (i % 251) as u8 + 1 { bad += 16; }
    }
    // 4) the rest, one call over holes (munmap of unmapped parts is fine).
    if munmap(p as _, LEN) != 0 { bad += 32; }
    let r = syscall3(SYS_MINCORE_NR, p as usize + LEN - PAGE, PAGE, vec as usize) as isize;
    if r != -12 { bad += 64; }
    munmap(vec as _, pages);
    let lost = free0.saturating_sub(free_ram());
    say_kb(b"  mincore/munmap lost_kib=", lost >> 10);
    say_kb(b" bad=", bad);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    report(name, bad == 0 && lost < 2 * MIB)
}
