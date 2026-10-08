# Lane tmpcap — 2026-10-01

**Outcome: FIXED.** Branch `lane/tmpcap` @ `af78883` (base origin/main cbe9947). Machine: Mac (aarch64 HVF; x86_64 **TCG only** — desktop had another lane's QEMU and 2.2 GB free disk).

## Root causes
1. `MAX_TMP_SIZE` = 32 KiB inline `data` array per pool entry (512 × ~34 KiB ≈ 17.6 MiB BSS). Writes past 32 KiB → ENOSPC / silently cut.
2. **The "wc -c = 4089 for 50 KB logs" bug is a separate one: tmpfs file offsets were per descriptor, not per open file description.** `VnodeKind::TmpFile.pos` was copied on dup/fork/SCM_RIGHTS, so every child wrote from the parent's offset. Repro before fix: `(seq 1 10000; seq 1 10) > /tmp/x` → 48894 bytes (Linux and our f2fs: 48915). A session whose many children inherit one redirected stderr ends up as long as its single largest writer.

## Change (servers/vfs only + vfstest)
- Regular-file bytes now always live in the inode's `TmpVmo` (lazy, page-granular; `pages[i]==0` or past the list = hole). Symlink bodies use a 256-byte inline `link`.
- Budget: `TMP_PAGES_USED` vs `tmpfs_budget_pages()` = total_pages/2 (ENOSPC); kernel /proc/sys/mtab snapshots are counted but never refused. 16 GiB per-file limit (EFBIG); page index grows with `try_reserve`.
- truncate/ftruncate/O_TRUNC share `tmp_set_size` (sparse grow, free + zero tail on shrink, F_SEAL_SHRINK, dmabuf EPERM). O_TRUNC used to leave a promoted VMO's length stale.
- Write past EOF zeroes the exposed gap in existing frames; MAP_SHARED fills holes in the mapped range.
- statfs: budget/used pages. st_blocks: real allocated pages. lseek SEEK_DATA/SEEK_HOLE.
- Read served up to 1 MiB per call (was 4 KiB).
- `TMP_OFDS` (4096 entries): shared offset per open(2), refcounted via pipe_ref_inc/pipe_ref_dec/release_vnode/handle_close; write reads/advances it under TMP_FILES so concurrent writers/appenders don't overlap. O_APPEND offset now read under the lock too.
- /proc/kmemstat gains `tmpfs_pages` / `tmpfs_budget_pages`; snapshot limits: kmemstat 128 KiB, smaps 256 KiB.
- Loop devices backed by a tmpfs file use the same page helpers.

## Evidence
- vfstest RC=0, 55 PASS on **aarch64/HVF** and **x86_64/TCG**, including new: tmpfs_large_rw (3 MiB, 1 MiB single read), sparse_hole (8 MiB hole, st_blocks 8, SEEK_DATA/HOLE, EFBIG at 16 GiB, 1 GiB sparse ftruncate), truncate, append_big (20×6001 B from two O_APPEND fds), cp_multi_mb (5 MiB + /bin/brush), unlink_frees (incl. open-unlink-close), mmap_sparse, shared_offset (fork + dup), enospc (fills to half-RAM budget, /proc still readable, unlink restores).
- Shell, both arches: `cp /bin/cosmic-comp /tmp` (33.6 MB) md5 identical; `(seq 1 10000; seq 1 10) > /tmp/x` = 48915; `seq 1 20000 2>/tmp/err 1>&2` = 108894; `truncate -s 1G` → du 0; dd 200 pages → statfs free −200, rm → back.
- exectest/forktest/ptytest/permtest/f2fstest/memtest RC=0 (aarch64); exectest/forktest/ptytest RC=0 (x86_64).
- COSMIC smoke (virgl, Mac GPU QEMU): greeter → login → panel/wallpaper → Super+T cosmic-term on aarch64/HVF and x86_64/TCG; no panics. /run/user/1000 (bus, wayland sockets, locks) and /tmp/panel.ckpt are fine; tmpfs held ~1160 pages in-session and returned to the same count after the terminal exited.

## Open / not mine
- `scmtest scm_import_emfile_single_release` FAILs on both arches, pre-existing: its dup loop is capped at 256 but MAX_FDS is now 512 (63fd24e), so the table never fills (hogs=256, errno 0). Test needs its bound raised to 512.
- `sh -c 'seq 1 5 >&2' 2>FILE` prints to the console and leaves FILE empty, identically on f2fs — a stdio/console redirection bug (fd 2 in a grandchild after `>&2`), not tmpfs. Probably part of "stderr capture truncates silently".
- `dd if=/dev/zero bs=1M` gets 4 KiB short reads (/dev/zero caps reads), not tmpfs.
- `df /tmp` shows the root f2fs (mtab lacks the tmpfs mounts); `stat -f /tmp` is correct.
- fallocate(2) is a kernel no-op (kernel/src/syscall.rs, shared) — posix_fallocate does not extend a tmpfs file.
- Directories keep a per-descriptor getdents cursor (no OFD); /proc snapshots allocate an OFD too (harmless).

## Shared files
servers/vfs/src/lib.rs, servers/vfs/src/block.rs, userland/vfstest/src/main.rs. Nothing in servers/net or kernel/.
