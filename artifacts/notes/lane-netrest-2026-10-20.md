# lane netrest (2026-10-20)

## L9.5 ICMP raw/ping (servers/net, nettest, ping)
nettest RC=0 fails=0 (icmp_raw included); net-e2e suite PASS; nettag2 icmp_unbound PASS
(66350 round trips, 0 wakes, first echo reply after 49 us).

## L10.8 Linux-compat facts
AT_HWCAP from the ID registers, uname release 6.0.0-leandros, sched_setaffinity/getaffinity
stored per thread and inherited. relibc ca5b2718 makes getauxval answer from the recorded auxv.
Root cause of the getsid(zombie) failure was sys_waitid ignoring WNOWAIT (it reaped the
child); now WNOWAIT uses sched::wait_peek. getpgid on an unreaped zombie now answers from
EXIT_LOG (exit_logged_pgid_of) instead of ESRCH.
Tests (aarch64): syscalltest 0 fails (getsid_zombie PASS); regress PASS; net-e2e PASS;
evherd, pollnarrow, listentag, nettag2 PASS under the qemu lock.

## Open
- Placement ignores Task.affinity.
- relibc has no sched_getcpu/sched_setaffinity wrappers; relibc waitid is unimplemented (test uses raw syscall).
- L9.8 (virtio-net RX IRQ) not done here; L9.7 untouched.
