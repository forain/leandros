# lane/netfdshare (2026-11-10) - FIXED (net sockets + epoll follow the fd-table key)

Bug: after clonefiles (a3565f39) the VFS fd table was shared for CLONE_FILES but the net server's socket tables and the epoll holder ids were still per tgid, so one fd number named two objects (pasta: RTM_NEWLINK on the parent's netns-0 netlink socket -> EPERM).

Fix: socket table key and epoll holder id are the VFS table key (vfs::tbl_key). net: `tk()` in servers/net/src/lib.rs, rekey_table, dup_in, move_slot, sock_cloexec; NET_CLOSE_ALL takes the exact key. kernel/src/syscall.rs: ep_key/ep_cur, epoll_rekey, dup_handle_across; fd_detach_sharers rekeys vfs+net+epoll together; fd_redup_handles(tid, src, dst) copies a split table's sockets/epoll fds from the source table; exit of the last private-table user closes net+epoll tables by key.
Other fd-indexed tables (eventfd, timerfd, signalfd, inotify, pidfd, tun) are VFS vnodes inside the shared table: already correct.

Tests (forktest): clone_files_socket_shared, clone_files_close_visible, clone_files_epoll_shared, clone_files_netlink_netns.
Fail-first (box aarch64, tests-only ref on main): clone_files_socket_shared and clone_files_epoll_shared FAIL, run then hangs to the 900 s timeout. Fixed branch: forktest 1/1 PASS.
Open: pasta now stops at openat(/proc/<ns child>/ns/net) EACCES from ptrace_read_check dumpable requirement (see podtrace6-findings.md); needs an orchestrator decision.
