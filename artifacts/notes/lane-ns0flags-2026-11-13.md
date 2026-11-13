# lane ns0flags (2026-11-13) - FIXED

ns-0 alias/bind/self-bind mounts now carry MS_RDONLY/NODEV/NOEXEC (servers/vfs mnt.rs ns0::flags_at/set_flags).
KPath::from_ns publishes them as a hidden_flags CallCtx, so the existing mnt_flag_check (ro/nodev) and exec_permitted (noexec) enforce them; opens record them on the fd, and ns-0 dirfds are walked by backing path (fd_hidden_host mns==0).
Linux rules: bind copies source flags, MS_BIND|MS_RDONLY in one call is ignored, remount replaces flags, fresh mounts take their flags.
Test: mnttest e7_ns0_permount_flags_enforced (was e7_ns0_has_no_permount_flags, which pinned mkdir succeeding on the ro bind). aarch64 mnttest+vfstest green.
Open: /proc/mounts omits ro for ns-0 mounts; ns-0 dirfd flags are recorded at open (stale after remount-rw).
