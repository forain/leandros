# lane/syscpu 2026-11-08 - FIXED (A, B); ffsandbox still on=False for a different reason

A. servers/vfs/src/sysclass.rs: additive overlay for /sys/devices/system/cpu/{present,possible,online,kernel_max},
   cpu<N>/ (N < active_cpu_count, `online` for N>0) and /sys/devices/cpu/ (empty dir). Only those two subtrees are
   claimed; /sys/devices and /sys/devices/system and every other /sys path resolve as before. Hooked through
   sysclass::is_path/dir/attr/dirent, so intercept, open, stat, statfs, getdents pick it up with no lib.rs edit.
B. ffsandbox.py: info lines (tsync, Parent path, Adding paths from, errno -2) are dropped; errno -13/EACCES/denied/
   "seccomp sandbox violation" always count; up to 10 counted lines printed.
Test: proctest sys_devices_cpu (PASS aarch64). Before-state is the ffbroker evidence (EACCES/ENOENT on .../cpu/present).
Desktop check: ffsandbox --mode on, aarch64: greeter/desktop/terminal/firefox all up. (One earlier run never got the
desktop within 120 s, rerun fine: flake, not reproduced.)

ffsandbox on main 207c4be7 + this lane: on=False. Roots of sandboxed children are /.leandros-void (chroot engaged now),
Seccomp 2 on socket/tab/rdd/utility. NSpid column still "?" (/proc status lacks NSpid). Counted lines (6, all the same cause):
  EACCES open /usr/local/lib/libsqlite3.so.0 (and path IO_USE_VFS=local/libsqlite3.so.0) for tabs 980, 1067.
No more /sys/devices denials, no violations. Open: ffbroker fix C (/usr/local/lib in policy or ld.so.conf) and the odd
"IO_USE_VFS=local/..." path string (broker path mangling after chroot?).
