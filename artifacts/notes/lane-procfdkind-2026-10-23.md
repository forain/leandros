# lane/procfdkind (L5.12 remainder) - FIXED (pipes); mounted files already worked

- open("/proc/<pid>|self/fd/N") of a pipe end returned ENOENT. `reopen_fd_kind_of` now
  returns a Pipe on the same ring whose end follows the access mode (O_RDONLY read,
  O_WRONLY write, O_RDWR keeps the original end); handle_open takes the endpoint ref
  (same path as dup alias).
- MountedFile arm added (reopen by VFS_FD_PATH path through the fs server, fresh offset).
  Evidence: fd_reopen_mounted passes on UNFIXED main for /root, /home and /bin, so
  mounted files were already reopenable (own pid); the arm is only reached for the
  /proc/<other pid>/fd/N form and is untested there.
- proctest: fd_reopen_pipe (red on main: ENOENT; green after), fd_reopen_mounted.
- Full aarch64 regress PASS (one earlier run lost the VM mid-suite at f2fstest, rerun green).
