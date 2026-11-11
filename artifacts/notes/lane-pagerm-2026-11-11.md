# lane/pagerm 2026-11-11 — PARTIAL (premise partly refuted)

- Premise "no write-protect on writeback" REFUTED: msync/fsync/sync already write-protect via `sched::wrprotect_cache_all` (walk of every address space) and re-dirty on write fault (`mark_dirty`, seq check). A full rmap was not built.
- Real bug: the background flusher only wrote pages NO mapping held (`pc_flush_dirty(None,false)`), so stores through a long-lived MAP_SHARED mapping stayed dirty until msync/munmap (lost on crash/reset). Fixed: every 10 s the f2fs-pcache task also flushes mapped pages (write-protect first).
- Fail-first: pcachetest `bg_writeback_mapped` (store, keep mapping, no msync; needs written_back to rise within 30 s, PTE write-protected, re-store re-dirties). aarch64: RED on 586b88af ("never written back while mapped"), GREEN with fix (13.5 s).
- CACHE lock counters added: /proc/pcache `cache_lock_acq`, `cache_lock_contended`; pcachetest prints them. Full aarch64 regress (HVF): 616525 acquisitions, 148 contended (0.024%). Not a bottleneck; no change to the global lock.
- Open: periodic walk of all address spaces every 10 s is O(procs) with IRQs masked (same as msync); a real rmap would remove it. x86_64 box regress impossible (box NVMe 100% full, ENOSPC); x86 checked on Mac instead (see report).
