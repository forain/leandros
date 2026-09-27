# lane/greeterleak — ~55 MiB per greeter-chain death (2026-09-24)

Machine: linux desktop, x86_64/KVM; aarch64 on the same box under TCG.
Base `fc5a6bc`. Branch `lane/greeterleak`, head `ccd0ebf` (pushed).

## Result: FIXED. It was user pages, not kernel heap or f2fs

The rounding tail of eager VMAs leaked. `AddressSpace::map` backs an n-page
VMA with one buddy block of `2^ceil(log2 n)` pages and maps only the first n.
Only `Drop` and `unmap_range` knew that order. When an eager VMA becomes
per-page tracked, the order is lost and each side frees exactly n pages. That
happens in `split_at` (mprotect sub-range, MAP_FIXED middle punch: ld.so
RELRO and overlays) and in fork's `clone_as` (read-only eager regions). The
tail pages, never mapped and owned by nobody, then leaked for the rest of the
boot. Every private file mmap takes the eager path (`sys_mmap` step 2). So
every library segment that cosmic-comp and the greeter map, split and fork
leaked up to half its block.

Fix: `vmm::free_eager_tail(phys, n)` returns `[n, 2^order)` as aligned
sub-blocks at both conversion points (`mm/src/vmm.rs` split_at,
`mm/src/cow.rs` clone_as).

## Accounting added (kept in the commit)

- `mm::buddy`: every block is charged to its caller's `file:line` through
  `#[track_caller]` plus a 1-byte-per-page site tag. A block freed in
  different pieces than it was allocated in is still credited to the right
  site.
- `mm::slab`: pages per class (the high-water mark, since slab never returns
  pages) and live objects per exact size.
- `/proc/kmemstat` prints all of it, plus `refused_frees`.
- `scripts/greeterstorm.py <arch> <deaths>`: boot, then settle, sample,
  SIGKILL the compositor, and repeat. Deltas per site go to
  `~/greeterstorm/<tag>-samples.jsonl`.

## Evidence

The base run was 15 deaths on x86_64/KVM, 100 s settle, and it completed.

- `free_pages` fell about 14.1k pages per death: 343819 → 162769 over 14 deaths.
- The entire loss sat at site `mm/src/vmm.rs:226`, the eager `map` block.
- Slab stayed flat: 1493 → 1514 pages over the run. Heap by exact size was
  flat. f2fs does not appear.
- After the 15th kill the chain did not respawn within 100 s. With only init
  and brush alive, `vmm.rs:226` still held 222k pages (867 MiB). That rules
  out a reap-timing window.

The fixed run was also 15 deaths on x86_64/KVM, and it completed.

- `free_pages` went from 358026 to 357667, **−26 pages per death**, and all
  15 respawns came back.
- The remaining residual is `vmm.rs:686` (lazy faults) at +27 per death,
  within the noise and execleak territory.

aarch64 under TCG, fixed: 4 deaths, −20 pages per death.

New memtest case `eager_split_frees_tail` (64 rounds of mmap 65 pages of a
file, then mprotect-split or fork, then munmap):

| arch | code | lost_pages | result |
|---|---|---|---|
| x86_64 | unfixed | 4041 (expected 64 × 63) | FAIL |
| x86_64 | fixed | 10 | PASS |
| aarch64 | unfixed | 4341 | FAIL |
| aarch64 | fixed | 12 | PASS |

## Overlap with execleak

This is not process teardown, but it inflates per-death numbers.

`memtest exit_frees_page_tables` on x86_64 measured 740 KiB per death
unfixed and 183 KiB per death fixed (aarch64 fixed: 256). Part of execleak's
"~100–160 pages per plain process death" residual was this tail: fork of a
process with eager file mappings.

No sched exit/reap code was touched.
