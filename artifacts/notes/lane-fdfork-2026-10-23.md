# lane/fdfork 2026-10-23 - FIXED (L10.4 remainder)

- fork from a thread with a private fd table: fork_dup_tables now releases, in the child, every net socket and epoll fd that the child's copied fd table does not name (net/epoll tables are per process, so they had been dup'd for the whole process). New helpers vfs::table_handles, net_server::open_sock_fds.
- setrlimit/prlimit(RLIMIT_NOFILE) now updates every fd table owned by the process (vfs::set_fd_limit).
- Test: proctest fork_unshared_socket. Before the fix: FAIL status 0x2000 (exit 32, peer saw no EOF); after: PASS. proctest + scmtest green, full aarch64 regress 42/42 RC=0.
- Open: epoll_ctl registrations remain per process; clone(CLONE_THREAD) without CLONE_FILES still shares.
