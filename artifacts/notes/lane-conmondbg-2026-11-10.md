# lane/conmondbg 2026-11-10

Status: PARTIAL (blockers #1 and #2 FIXED; `podman run --rm --network=none alpine echo hi` not iterated to the end).

- errtrace knob `sched.errtrace=<comm>`; root cause #1: chmod/stat of /proc/self/fd/N for socket and tmpfs-directory fds (conmon chmods its console socket).
- Security: dir-fd stat/chmod go through fstat/fchmod on the VFS handle; the raw-path follow refuses `/.leandros-sb`. mnttest `pool_procfd_tmpfs_dir` added; mnttest pool_* unchanged and green.
- Blocker #2: AF_UNIX bind/connect on mounted filesystems (f2fs): real S_IFSOCK inode (mount server mknod) + in-memory (path, ino, sock_id) table; unlink / stale node / EADDRINUSE / parent permission semantics. conmontest unix tmpfs + f2fs groups (fail on main: bind returns EOPNOTSUPP on f2fs).
- /var/run -> /run symlink in image when missing.
- `[PF] SEGV ... (kernel pc)` is the diagnostic printed before uaccess fixup turns it into EFAULT; not a kernel bug.
- Verified: conmontest 100% and mnttest on aarch64 (Mac); box x86_64 full regress PASS; box aarch64 see merge commit.
- Open: podman run next failure (likely crun lo-up SIOCSIFFLAGS, lane netnsfix). Details: orch/designs/conmon-findings.md.
