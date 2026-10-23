# lane/tmpfsbig 2026-10-23 -- FIXED (L10.12)

1. MAP_SHARED of a tmpfs file > 256 MiB was EINVAL (syscall.rs shared-VMO branch rejected
   eager_too_big). The branch aliases the VMO frames with no copy, so the eager ceiling is
   dropped; only ANON_MAP_MAX_BYTES applies.
2. ftruncate (shrink) zapped the mapper's CoW'd copies in aliased private maps (zap_backing).
   Regions from map_private_vmo_frames now track CoW'd pages in `written` (tracks_cow_vmo,
   set by the existing promotion paths); zap_backing skips them. Untouched pages past EOF
   still fault to SIGBUS. After fork the child's written bits follow clone_as (a lost bit
   only falls back to the old behaviour for that page).

Tests: memtest big_shared_map (300 MiB: map store visible via read(), second process
sees/writes, write() visible via map) and big_private_truncate (CoW'd pages survive,
untouched page past EOF SIGBUS). Before the fix: memtest RC=135 with big_shared_map FAIL
("shared map failed"). After: memtest/forktest/shmtest/vfstest 0 failures (aarch64).
Open: none known for L10.12.
