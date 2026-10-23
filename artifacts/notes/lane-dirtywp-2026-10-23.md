# lane dirtywp — 2026-10-23 (openbugs L4.6)

## Problem
A MAP_SHARED f2fs page stays `F_DIRTY` for as long as any mapping holds it
(`writeback_done` cleaned only at `pageref <= 2`), because a writable PTE was
never write-protected again. Every msync/fsync/sync rewrote every mapped page
that had ever been stored to, whether or not it changed since.

## Design (no new reverse map)
The existing "reverse map" is the process walk that truncate already uses
(`sched::zap_backing_all`): visit every address space under its `busy`.
Write-back now follows Linux's `clear_page_dirty_for_io` + `page_mkclean`:

1. `pagecache::take_dirty` clears `F_DIRTY` and sets `F_WB` under `CACHE`, and
   reports whether any taken page has a mapper (`pageref > 2`: cache + pin + map).
2. Only then, per file key with mapped pages, `sched::wrprotect_cache_all(key, pgoffs)`
   → `AddressSpace::wrprotect_cached`: for shared, writable, file-backed lazy VMAs of
   that key, each present page whose `cache_pgoff` is in the set and is marked
   written is remapped read-only, its written bit cleared, and the range TLB-flushed
   (`tlb_flush_range`: `tlbi vaae1is` broadcast on aarch64, shootdown on x86).
3. The filesystem copies the page and writes it (`still_dirty` now checks `F_WB`).
4. A later store takes the existing first-write fault (`clean_file_page`) →
   `mark_dirty` sets `F_DIRTY` again and makes the PTE writable. A store that
   lands before step 2's flush is in the copy of step 3.
5. `writeback_done`: clears `F_WB`; on failure re-sets `F_DIRTY`.

Lock order: the walk runs with no filesystem lock and no CACHE lock held (same
rule as `zap_backing_all`); each AS `busy` is taken alone (lane aslock).
Pages with no mapper (fsync of a plain write(2)ed file) skip the walk.

## Guard
pcachetest `mmap_clean`: dirty 32 pages through a mapping, msync; a second msync
with no stores must write back < 8 pages (/proc/pcache `written_back` delta);
one store, third msync writes >= 1 page and the bytes read back.

## Implementation note
Instead of an `F_WB` bit (which broke `ndirty`/`DIRTY` bookkeeping and the
fsync-vs-background-writer overlap), `F_DIRTY` stays set during write-back and
each cached page carries `seq: u16` (in existing padding), bumped by every
`mark_dirty`. `take_dirty` records it; `writeback_done` cleans only if it has
not moved. Two concurrent write-backs of one page stay correct: whichever took
it after the last store is the one that cleans it.

## Result: FIXED
- pcachetest `mmap_clean` on main: FAIL "clean msync rewrote pages: 32";
  on the lane: PASS (second msync < 8 pages, the post-msync store written).
- pcachetest/memtest/f2fstest/vfstest/shmtest/scmtest green aarch64.
- full regress aarch64: 0/0 PASS.
