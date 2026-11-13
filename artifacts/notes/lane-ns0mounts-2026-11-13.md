# lane ns0mounts (2026-11-13) - FIXED

1. /proc/mounts and mountinfo print ro/nodev/noexec for ns-0 alias/bind mounts (lib.rs write_alias_opts): "ro|rw,nosuid[,nodev][,noexec]" (+ ",relatime" in mountinfo, super options stay "rw").
2. ns-0 dirfds record the mount identity (alias::id_of(mountpoint), ns u16::MAX); the kernel (hidden_below/fd_hidden_host) and fd_mnt_ro look up live flags by alias::flags_by_id, falling back to the recorded flags only when the mount is gone. KPath::from_ns now carries the identity even for unflagged mounts (hidden_flags Some(0) falls through to normal resolution in the VFS).
Tests: mnttest e7_ns0_mounts_show_flags, e7_ns0_dirfd_flags_live (rw dirfd -> remount ro -> EROFS; ro->rw allows). aarch64 mnttest green.
Fail-first: the old code printed no ro (seen as "nonero" only after a half-fix); red build of the unfixed tree was not separately run.
