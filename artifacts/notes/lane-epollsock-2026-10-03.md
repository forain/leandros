# lane epollsock (2026-10-03)

Socket syscalls (bind/listen/accept/accept4/connect/sendto/recvfrom/sendmsg/
recvmsg/shutdown/getsockname/getpeername/setsockopt/getsockopt) on an open
epoll fd returned EBADF; Linux says ENOTSOCK.

Cause: `not_a_socket` (94a2994) only covered fds below `SOCK_FD_BASE`. Epoll
fds live in their own table at `EPOLL_FD_BASE` (0x400) + index, above the
socket range, so they reached the net server, which answers EBADF. eventfd,
timerfd, signalfd, memfd, pidfd, pipes and files are all VFS-range and were
already covered; `TTY_FD_BASE` (0x1000) has no kernel allocator.

Fix: `not_a_socket` maps an epoll-range fd to ENOTSOCK when the caller's thread
group holds it (`epoll_slot_for`), EBADF otherwise. Socket fds and VFS aliases
of sockets are unchanged.

Test: scmtest `send_on_epoll_enotsock` (send/recv/getsockopt on an epoll fd,
then EBADF after close). Before the fix: send/recv -9, getsockopt 0 (aarch64).
After: passes on aarch64 (HVF) and x86_64 (TCG); full regress suite green on both.
