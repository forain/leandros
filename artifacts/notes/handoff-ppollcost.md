# lane/ppollcost handoff (2026-10-20)

## Done
- pollnarrow gained 4 cases (userland/pollnarrow/pollnarrow.c, `ppoll_cases`):
  a thread parked in raw SYS_ppoll vs per-call epoll_wait (old relibc shape) on
  (a) one AF_UNIX stream, (b) eventfd+pipe+AF_UNIX stream+dgram+UDP+timerfd,
  then 1 s of unrelated AF_UNIX/pipe traffic. Runs in `--suite regress` via
  pollnarrowtest.py.
- Result on unmodified main b9e16a71, aarch64/HVF (run id ppc-base1), all PASS:
    epoll_park_unix  mask 0x8008002000000  0 wakes / 91.9k trips
    ppoll_park_unix  mask 0x8000002000000  0 wakes / 93.7k trips
    epoll_park_mixed mask 0x800d222000020  0 wakes / 93.5k trips
    ppoll_park_mixed mask 0x8005222000020  0 wakes / 94.5k trips
  => HYPOTHESIS REFUTED for these fd types: ppoll parks keyed, NARROWER than
  epoll (no epoll self-tag bit), zero stray wakes. No kernel change made.

## Next suspects (unverified, file:line)
1. Global EPOLL_CTL_GEN in ppoll: kernel/src/syscall.rs:4544 / 4626 take
   gen0 = poll_gen() (EPOLL_CTL_GEN + retag gen, syscall.rs:12482), and after
   every park (4669) a moved gen cancels the park and forces a POLL_TAG_ALL
   filter + full re-probe (4680-4686). EPOLL_CTL_GEN is bumped by EVERY
   successful epoll_ctl system-wide (12403); Firefox's libevent/IPC threads do
   many EPOLL_CTL_MODs, so every Firefox ppoll waiter re-probes its whole set
   (more FD_TABLES/net locks -> recvmsg/sendmsg cost). Only sets holding a
   nested epoll fd (tag.nested != 0) need the EPOLL_CTL_GEN part. Test idea:
   ppoll ping-pong + a thread looping EPOLL_CTL_MOD; compare PO_PROBES/PO_CALLS
   in /proc/wakestat before/after.
2. Per-call listener churn: ppoll begins (ALL filter) / updates / parks /
   takes / ends the listener every call = 5 RUN_QUEUE holds (sched/src/lib.rs
   3683-3725); epoll keeps its listener across calls (syscall.rs ~13286
   poll_listen_store). RUN_QUEUE is taken by every wake, so recvmsg pays.
3. The 706/s ALL-mask wakes: fds whose VFS_POLL answers POLL_TAG_ALL —
   device servers without data[32]=1 (DRM, servers/vfs/src/lib.rs ~10454-10472),
   DevVt (~10486). Firefox polls dmabuf/DRM fds directly (fence waits); with
   per-call epoll those may have failed epoll_ctl instead of parking ALL.
   Check on the laptop: gd census K_PARKALL / churn_all_tag_note for ppoll.
