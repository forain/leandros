# lane/procrss — per-process memory accounting, OOM-style guard, slab reclaim (2026-09-26)

Branch `lane/procrss` from origin/main a5b5b62, head 228853d (pushed). Mac only: aarch64 HVF (+virgl for the guard), x86_64 TCG.

## Changes
- mm/vmm.rs: `MemCounters` summed from per-VMA frame counts (eager span / `lazy_count`) — O(VMAs), no page walk. RssAnon/File/Shmem by VMA kind (device excluded, ELF BSS tail anon, eager file copies tagged `MAP_FILE_COPY`). VmHWM/VmPeak Linux-style (snapshot in unmap_range + brk shrink). `stack_top` for VmStk. `vma_census` (smaps only; per-frame pageref for Pss).
- sched: `proc_mem_of`/`proc_vma_census` clone the target Arc under RUN_QUEUE, then spin on busy (safe vs reaping); `process_pids_from`; exe-path table 64 -> MAX_TASKS.
- vfs: status (Name, Vm*, Rss*, Threads), statm, stat (real comm; field layout was shifted by 2 from num_threads on), cmdline=exe path, smaps, smaps_rollup; `/proc` lists pids, `/proc/<pid>` opens as a dir (ephemeral pool dir), openat(dirfd,...) works; kmemstat `slab_reclaimed_pages`.
- slab: doubly-linked free list + u16 live count per pfn; empty page freed beyond 4-page reserve per class; 8-byte class folded into 16.
- prctl PR_GET_AUXV -> EINVAL (was 0 = empty auxv => rustix page_size 0 => bottom Mem 0 B).
- init mem_guard: largest-RSS graphical-side process (excl. init, serial session, logger, greetd; cosmic-comp/greetd last); fallback to greetd after 3 kills/episode.

## Evidence
- memtest (both arches, all PASS): proc_rss_anon +32768 KiB exactly (8244->41012->8244), fork_cow child rss 24.6 MiB shared 24.6 pss 12.3 -> private 16.4 pss 20.5, file_lazy 36->36 (map) ->1060 (touch 1 MiB), proc_pid_dir, slab512 2->peak~250->5, reclaimed 262.
- forktest/exectest/jobtest/killmt/vfstest/pthreadtest rc=0 (aarch64); forktest/exectest/jobtest/killmt 20/vfstest rc=0 (x86 TCG). killmt mem deltas -240..0 KiB.
- Guard, aarch64 HVF virgl, greeter up: `memtest hog` (setsid, 32 MiB/s) -> `MEMORY PRESSURE: MemAvailable 127 MiB < floor 196 MiB; killed pid 66 (memtest), RSS 1608 MiB`; greetd/cosmic-comp/greeter survive, greeter pixel-verified. At 160 MiB/s the hog OOMs before 2 samples (guard cadence limit).
- greeterstorm aarch64 virgl, 5 deaths/40 s settle: slab 128-134 pages, not monotonic (+1/death noise); free -83 pages/death all in user lazy-fault sites (vmm.rs:741/816), noise at this settle.
- bottom: Mem column now real (cosmic-comp 91 MiB, greeter 55 MiB, greetd 9 MiB); names real. bottom still draws only its first frame over serial (pre-existing).

## Open / notes
- Every exec maps an eager 8 MiB stack: ~8 MiB of each process's RSS (VmStk 8192) is that. Making it lazy would save ~250 MiB in a COSMIC session.
- Guard not exercised on x86_64 (needs GPU session; userspace-only code). Written private file pages still count as RssFile (no dirty tracking).
- Shared files: mm/vmm.rs, mm/cow.rs (1 line), mm/slab.rs, mm/buddy.rs (phys_end), sched/lib.rs, kernel/syscall.rs, kernel/init.rs, servers/vfs, userland/init.
