# Lane ffmem — 2026-10-02

Branch `lane/ffmem` on `08ac17c`. Not merged, not pushed.

## Result
Firefox on https://en.wikipedia.org/wiki/Firefox now runs at **2G guest RAM on virgl** on both arches. Before this lane, init's memory-pressure guard killed it a few seconds after startup unless the guest had 4G.

## Root cause, measured (aarch64/HVF virgl, 2G, `/proc/kmemstat`)
The guard measured correctly: free memory really fell to 161 MiB, below the floor of RAM/10 = 195 MiB. The memory went to **private per-process copies of file pages**. The kernel had no shared page cache, so every process that touched a page of libxul, libgallium or libcosmic got its own frame (`install_file_fault`, plus a 16-page fault-around read).

| state | free | file-page copies | anon | virgl BOs | page tables |
|---|---|---|---|---|---|
| desktop + cosmic-term, before Firefox | 1096 MiB | 471 MiB | 196 | 93 | 13 |
| last sample before the kill (t=167 s) | **161 MiB** | **987 MiB** | 522 | 126 | 26 |

- Per process at the kill: Firefox's 7 processes held anon 382 MiB and **file 465 MiB**. libxul.so is 143 MB, and the parent and each content, socket and utility process carried its own copy of the parts it touched. Each cosmic process carried 12-67 MiB of its own library copies.
- virgl BO backing grew by only ~30 MiB during Firefox startup. Venus survived at 2G because it sits a little further from the edge, not because virgl leaks. BO backing is rounded up to a power of two in the buddy allocator; that is a small, secondary waste and was left alone.
- Guard policy was **not** changed: the evidence shows the memory really was in use.

## Fix
- `d16c3d8` **mm: shared page cache** (`mm/src/pagecache.rs`).
  - **What is cached:** a page that holds file data from end to end, keyed by (mount, inode, page offset). It is mapped read-only into every private mapping that faults on it. A cache hit also maps the cached pages that follow it in the fault-around window, with no read at all.
  - **Writes:** a private mapping's first store, a fork sibling's store, and a kernel HHDM store (`write_user_buf`, prefault) all go through the existing copy-on-write promotion. It copies the page while the frame has another owner.
  - **Pages never shared:** a segment's partial last page (its tail is BSS), the partial page at EOF, and MAP_SHARED mappings, which keep their old eager-copy semantics.
  - **Lifetime:** the cache holds one `pageref` per frame and drops it in three cases:
    - **Last mapping goes:** when the inode's last mapping registry entry goes (`key_put`), so the cache never outlives its mappers.
    - **Data changes:** f2fs `write_file_data`, `truncate_to` and `free_inode_data_and_nodes` invalidate the key, under the same lock a fault's read takes. A per-key generation keeps a fault's racing read out of the cache.
    - **Memory runs low:** when free memory drops under RAM/8, frames that no mapping holds are freed, up to RAM/6. This runs from the fault path and from `/proc/meminfo` reads, which init's guard makes every 0.25-2 s.
  - **Exec images:** they are now registered by inode, like private mmaps. Each binary has one entry, so its pages are shared too; the per-open EXEC_FILES table is only a fallback.
  - **Reporting:** `/proc/meminfo` `Cached:` is the cache size, and `/proc/kmemstat` shows `pagecache_pages` and hits/fills/reclaimed.
  - **aarch64 caches:** every page entering the cache is cleaned to the point of coherency, and a cache hit in an executable mapping does `ic ialluis`.
- `d394ee0` vfs: `/proc/kmemstat` lists per-process anon/file/shmem pages and their sum (the census used above).
- `b1f2ff1` syscall: read(2)/pread64(2) into a read-only buffer now answer EFAULT. Before, the f2fs copy took a kernel permission fault with the filesystem lock held, the task was killed in place, and the machine hung (`[WDOG] cpu0 holds ADDRSPACE_BUSY`, last syscall read). The new memtest case found this. The check also keeps such a store away from shared cache frames.
- `0c498c0` memtest cases:
  - `pagecache_shared_across_processes`: a second process mapping a touched 512-page file allocated **1 page**.
  - `pagecache_cow_isolation`
  - `pagecache_write_truncate_unlink`
  - `pagecache_map_shared_unchanged`
  - `pagecache_memory_returns`: 1024 pages mapped and touched by two processes, **0 pages lost** after the last one exits.
  - `file_private_no_leak` now warms every page of `/bin/brush` first.

## After (2G, virgl)
| | aarch64/HVF | x86_64/TCG |
|---|---|---|
| desktop, before Firefox: free / page cache | 1251 / 271 MiB | 1180 / 288 MiB |
| Firefox up 3.5-4 min: free / page cache | 348 / 424 MiB | 307 / 445 MiB |
| guard kills, EXIT line | none | none |

- **Long run:** a 9-minute aarch64 session ended with **729 MiB free**, all 7 Firefox processes alive and 17/17 screenshots magenta-free.
- **Page tables:** they peaked around 286 MiB while the page loaded (Firefox's GC and JIT map across a wide address range) and were back to 32 MiB by the end of the 9-minute run. They are released, not leaked.

## Verification (final tree `0c498c0`)
- `./scripts/build-all.sh` (both arches): OK.
- 13 suites + vfstest via runtests.py (now `.claude/skills/run-leandros/runtests.py --suite regress`): **14/14 RC=0 on aarch64/HVF and on x86_64/TCG** (298 / 299 PASS lines, 0 FAIL).
- Firefox, Wikipedia, `LEANDROS_QEMU_MEM=2G`, virgl GPU WebRender:
  - **aarch64/HVF:** 210 s wait (over 3.5 min with Firefox up), no MEMORY PRESSURE, no EXIT, `magcount` 0 on all 6 screenshots. The 9-minute session above was also clean.
  - **x86_64/TCG:** 240 s wait, no MEMORY PRESSURE, no EXIT, 7 Firefox processes alive at the end, `magcount` 0 on all 7 screenshots.
  - Proof images: `lane-ffmem-tools/firefox-2g-wikipedia-{aarch64,x86_64}.png`.
- Desktop boot: greeter, login, panel and cosmic-term came up on both arches in every session above.
- Not run: x86_64/KVM and Venus on the linux desktop.

## Tools
- `.claude/skills/run-leandros/ffsession.py --snap` (the old `artifacts/notes/lane-firefox-tools/` path is a launcher since lane/harness) saves a filtered `/proc/kmemstat` (free and cache pages, allocation sites over 10 MiB, processes over 10 MiB) before Firefox starts and after the wait. `ffsession.py` now runs at the driver's 2G default.
- **Don't sample by streaming a shell loop to the serial console.** A `while read` sampler printing to the serial tty grew to over 200 MiB anon in one desktop run, but not in others and never headless. It is a measurement artifact and polluted the first after-fix run. Use one-shot snapshots instead.
