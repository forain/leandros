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
    if !test_exit_frees_page_tables() { failures += 1; }
    if !test_eager_split_frees_tail() { failures += 1; }
    if !test_file_private_lazy_content() { failures += 1; }
    if !test_file_private_sigbus_past_eof() { failures += 1; }
    if !test_file_private_no_leak() { failures += 1; }
    if !test_file_private_survives_unlink() { failures += 1; }
    if !test_file_private_map_cost() { failures += 1; }

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
/// return every page they populated.
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
    let mut failures = 0usize;
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
    close(fd);
    let after = free_ram();
    let lost_pages = before.saturating_sub(after) / PAGE;
    write(STDOUT_FILENO, b"  rounds=".as_ptr(), 9); print_dec(ROUNDS);
    write(STDOUT_FILENO, b" file_pages=".as_ptr(), 12); print_dec(len / PAGE);
    write(STDOUT_FILENO, b" lost_pages=".as_ptr(), 12); print_dec(lost_pages);
    write(STDOUT_FILENO, b" failures=".as_ptr(), 10); print_dec(failures);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);
    report(name, failures == 0 && lost_pages < 4 * ROUNDS)
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

