# lane/panfix2 (2026-10-20)

Converted PAN audit sites to sched::uaccess helpers: write_nodemask, sys_select (timeval/sigdata),
epoll_wait_ready event writes, clock_getres (the former dispatch+0x9f0: `str xzr,[x20]` in
sys_clock_getres), f2fs write_file_data, vfs block read_at/write_at, readlink (tmpfs link, /proc
copies), statfs flags, timerfd_gettime.

Audit run (default pan=audit, aarch64 regress): 0 unique sites with ALL conversions applied.

## OPEN: two vfs conversions break tests, so they are left raw (audit sites return)
With the full set, regress failed: memtest proc_pid_dir, proctest fd_rename_follow(/tmp)/fd_deleted(/tmp),
chroottest procfd_walk_tmpfs/procfd_walk_chroot ("readlink /proc/self/fd/3 = , want /tmp/..").
Bisected by rebuild: original vfs lib.rs passes; reverting only these two hunks passes:
 - servers/vfs/src/lib.rs handle_readlink FdInfo::TmpIdx arm byte store (write_user::<u8>)
 - servers/vfs/src/lib.rs handle_read RamFile arm copy (copy_raw + -14)
Not explained (a lock-free staged copy_to_user variant of the TmpIdx arm failed identically).
Strict default NOT enabled (audit would still report these two sites).
