# lane/mntiso-procfd (2026-11-11) - FIXED (E1)

/proc/self/fd/N reopen and /proc/PID/fd/N/<below> now resolve through the opener's mount namespace tree
(fd_ns_path in kernel/src/syscall.rs; follow_magic_final reopen with an fstat/stat identity check;
vfs::fd_mnt, read_stat_id), so ro/nodev/mask of the fd's mount are honoured and links never resolve in host space.

Evidence (aarch64): mnttest `mntiso_procfd_suite` (5 cases + host sentinel) RED on unfixed code (5 fails, sentinel pass),
GREEN after patch (fails=0). podbridge (root) + podpasta (rootless) pass; ffsandbox mode on: RESULT on=True
(no f2fs identity mismatch seen). Full aarch64 box regress after merging main: 83 suites, 0 bad.

Open: E7 (host_to_view hint), listed in TODO.md.
