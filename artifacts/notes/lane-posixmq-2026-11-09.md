# lane/posixmq -- POSIX message queues (2026-11-09)

Status: FIXED (mq_open/mq_unlink/mq_timedsend/mq_timedreceive/mq_notify/mq_getsetattr).

- Kernel: sys_mq_* (x86_64 240-245, aarch64 180-185), blocking loop mq_blocking; queues live in
  servers/vfs/src/mqueue.rs, per IPC namespace, priority ordered (FIFO within priority).
- Filesystem: /dev/mqueue (readdir, open = QSIZE text, unlink); `mount -t mqueue` aliases it.
  The built-in mount is listed in /proc/mounts (unjailed readers).
- /proc/sys/fs/mqueue/{msg_max,msgsize_max,queues_max,msg_default,msgsize_default}: read-only constants (10/8192/256/10/8192).
- mq_notify SIGEV_SIGNAL: delivered with si_code SI_MESGQ (-3), si_pid/si_uid/si_value (write_siginfo shares the SI_QUEUE layout).
- relibc master 8a30eb78: src/header/mqueue wrappers.
- Not supported: SIGEV_THREAD (EINVAL; it needs a libc helper thread); sysctl values are not writable.

Evidence: userland/mqtest (musl-linked, raw syscalls) fails with MQ_OPEN dispatch disabled (RC=51) and passes
with it restored (0 failed, aarch64). Default aarch64 regress and x86_64 box regress results: see lane report.
