# lane/sendmsgfault (L10.3) - FIXED

Bug: bad iov_base through sendmsg/writev/readv/recvmsg/pwritev/preadv.
Findings:
- relibc sendmsg/recvmsg already pass msghdr straight to the kernel (Linux platform path). The userspace gather was in
  sys_uio: readv/writev/preadv/pwritev copied the iovecs in userspace and SIGSEGV'd on a bad base.
  Fix (relibc eb01acd9 + follow-up): those four call the READV/WRITEV/PREADV/PWRITEV syscalls directly (no copy, faster).
- Worse, on the unfixed tree sendmsg on a stream socket with a kernel-address iov_base took an EL1 Sync Fault in
  the net server (ring.write copy_raw has no range check; the uaccess fixup only covers user addresses).
  Fix: servers/net/src/lib.rs range-checks each iov (user_range_ok) in the sendmsg/recvmsg iov loops and the
  SCM_RIGHTS sendmsg pre-read. handle_send/handle_recv cannot check: splice passes kernel buffers to NET_SEND.
Guard: syscalltest iov_efault_{sendmsg,writev,readv,recvmsg,pwritev,preadv}_{one,kernel} (12 cases, child process,
  EFAULT or short count accepted, signal fails). Red on main: kernel EL1 fault at sendmsg_kernel; green: 12/12.
Regress aarch64: all green except nettest tcp_keepalive_probes (needs 192.168.105.1:53 / eth0 host route - environmental, TCP path untouched).
Open: AF_UNIX datagram/inet paths not individually probed with kernel iov bases (they go through ucopy_in / user_range_ok sites).
