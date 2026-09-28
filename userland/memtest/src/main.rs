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
    if !test_stack_deep_recursion() { failures += 1; }
    if !test_stack_overflow_segv() { failures += 1; }
    if !test_prot_none_faults() { failures += 1; }
    if !test_mremap_nomove() { failures += 1; }
    if !test_el0_cache_maintenance() { failures += 1; }

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
    let w = mmap(core::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0);
    if w as isize != -1 { let _ = core::ptr::read_volatile(w); munmap(w, len); }
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
