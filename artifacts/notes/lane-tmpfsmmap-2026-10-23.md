# lane/tmpfsmmap 2026-10-23 -- FIXED

Bug: `mmap(PROT_READ, MAP_PRIVATE)` of a tmpfs file larger than 256 MiB returned EINVAL
(git: "packfile cannot be mapped: Invalid argument").

Cause: only `MAP_SHARED` tmpfs and demand-paged f2fs private maps bypass the eager-copy
path in `sys_mmap_inner`; a private tmpfs map falls into the eager copy, which has
`eager_too_big = len > MAP_MAX_BYTES (256 MiB)` -> -22.

Fix (kernel/src/syscall.rs): a read-only, page-aligned-offset MAP_PRIVATE of a tmpfs file
above the ceiling takes the K1 shared-VMO branch (aliases VMO frames; Linux semantics for
unmodified private pages; no contiguous copy block). Smaller maps and writable private
maps are unchanged (still eager copy, still capped).

Test: memtest `big_private_file_map` (64 MiB and 300 MiB, on /tmp tmpfs and /root f2fs;
offset 0, page-aligned nonzero offset, length past EOF; markers read back).
Before: FAIL (tmpfs 300 MiB: map off0/offN/beyond-EOF all EINVAL; f2fs and 64 MiB fine).
After: PASS, memtest 0 failures (aarch64).

Open: 1 GiB+ not exercised (tmpfs fills holes with real frames, needs RAM); writable
MAP_PRIVATE of tmpfs > 256 MiB still EINVAL (needs COW over VMO frames).
pstest (ps tty / stat tty_nr, 4 cases) fails deterministically in this worktree; untouched
by this change, not investigated.
