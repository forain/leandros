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

## lane ppollcost2 (2026-10-20) — suspect 1 CONFIRMED (mild), fixed
- New pollnarrow case `ppoll_ctl_churn`: ppoll ping-pong on the 6-fd mixed
  set, then the same with a thread looping EPOLL_CTL_MOD on an unrelated
  epoll (~175k MODs in 700 ms); asserts probes/call (/proc/wakestat
  "# pollx") <= 1.1x baseline + 0.5.
- Before (aarch64/HVF, ppc2-pre1): baseline 12.69 -> churn 15.42 probes/call
  (+21%, ~0.45 extra full passes/call). Fails the tightened threshold.
- Fix (kernel/src/syscall.rs poll_wait_listen): the park re-check compares
  poll_retag_gen always, EPOLL_CTL_GEN only when the last full pass saw a
  tag with `nested != 0` (an epoll fd in the set). Race safety unchanged for
  sets holding an epoll fd; plain fds cannot change answer on epoll_ctl.
- After (ppc2-post1/2/3): baseline 12.90 -> churn 12.51/12.52/12.82. PASS.
- Real-world weight: Firefox's MOD rate is ~1e3/s, not 2.5e5/s, so this is
  unlikely to explain the YouTube sys-time regression by itself.
- regress ppc2-reg1: all green except nettag2test (icmp_recv, icmp_unbound,
  connect_pending: 1-2 stray wakes per window). The fix can only remove
  park cancellations, not add wakes; re-run nettag2 on main to confirm it is
  pre-existing/flaky before merge.
- Not done: suspect 2 (5 RUN_QUEUE holds per ppoll call; baseline ~12.7
  probes/call = initial pass + listen full pass + ring probe, i.e. every
  parked call probes the set twice). Not measured; per-instance keying of
  EPOLL_CTL_GEN not done (nested sets still see the global gen).

## lane ppollcost4 (2026-10-20) — cost case done, second pass cut
- `ppoll_cost` (pollnarrow, informational): ping-pong on the 6-fd mixed set,
  ponger in raw ppoll / old relibc per-call epoll (create+6 ADD+wait+close) /
  persistent epoll. aarch64/HVF, best of 3 x 500 ms, us per round trip:
    before (fa711de2, ppc4-pre1):  ppoll 40.27 (12.86 probes/call), per-call epoll 55.70, persistent 34.68
    after  (ppc4-post1/post2 x2):  ppoll 46.24 / 43.56 / 38.49 (7.45-7.92 probes/call),
                                   per-call epoll 61.62 / 63.73 / 61.57, persistent 33.09 / 33.16 / 33.97
  => raw ppoll is NOT slower than a per-call epoll (about 30% faster); it is
  ~15-20% slower than a persistent epoll. Run-to-run noise (+-10%) is larger
  than the probe saving, so the time win is unproven on HVF.
- Fix: poll_wait's first pass probes tagged (sets <= 16 fds) after reading a
  new lock-free POLL_WAKE_EPOCH (sched/src/lib.rs; bumped under RUN_QUEUE by
  every tagged/keyed wake scan and timerfd expiry scan, not by mask
  widening). poll_listen_begin_since installs those tags directly; if no wake
  ran since the epoch read, and retag gen (and EPOLL_CTL_GEN for sets with an
  epoll fd) did not move, the second full pass is skipped (wakestat
  `# pollx ... preskip N`). Otherwise it falls back to the ALL-filter full
  pass as before. Saves 6 probes and 1 RUN_QUEUE hold per parked call.
- ppoll_ctl_churn: 7.8-7.95 -> 8.3 probes/call with 190k MODs: PASS.
  (A first version bumped the epoch in widen_port_masks; churn then defeated
  the skip. Removed: nested sets are covered by EPOLL_CTL_GEN.)
- NOT DONE (budget): nettag2 check of the fa711de2 stray wakes, regress,
  x86_64. Next: `--suite nettag2` on this lane, and if the 1-2 stray wakes
  appear, the same on fa711de2^ to see if they are pre-existing; then
  `--suite regress`. Real cost suspect left: listener churn (begin/park/
  commit/take/end = 5 RUN_QUEUE holds per call vs persistent epoll), which
  only a persistent per-thread ppoll listener would remove.

## lane ppollcost5 (2026-10-20) — QEMU green; laptop A/B NOT run (wrong deploy path)
- aarch64/HVF on b6a94910 (main-tree userland, relibc master): `--suite nettag2`
  (run ppc5-nt1) PASS, no stray wakes, so the fa711de2^ comparison was not
  needed; `--suite regress` (ppc5-reg1) overall PASS, 0 boots failed. No fix needed.
- x86_64 release built with LEANDROS_RELIBC_DIR=relibc worktree at 9928b292
  (rc 0, no dynlist WARNING), kernel deployed as trial 0cb65b5a1e10
  (`#1 b6a9491049-20261021T0135Z`, up after 33 s). Then discarded; the laptop
  is back on good cf0514ad32a7 (`#1 wifirate2-a`), confirmed over SSH.
  (The laptop had been sitting on an older trial 530e35577789 `03bc58582c-dirty`
  before this lane; that was overwritten.)
- YouTube was NOT measured, because deploy-kernel cannot test the relibc change:
  * Firefox links musl, not relibc: /usr/lib/libc.so comes from
    ~/code/leandros-artifacts/musl-dynamic/libc-dynlist (scripts/musl/relink-libc.sh,
    mkfs-f2fs-populated.py:622-637). LEANDROS_RELIBC_DIR does not change it.
  * relibc-linked binaries are on the stick's /bin (and in f2fs-data0), not in the
    kernel or initrd (initrd 127488 B, same size in A, B and here), so deploy-kernel
    leaves the stick's relibc userland as it is.
  * So for cpuab, A and B must have differed in /bin binaries deployed by hand, or
    in something else. Before retrying, find out which relibc-linked processes run
    during YouTube (snd-sink? the greeter or session helpers?) and how cpuab
    deployed B. Ports such as cosmic-* and pipewire are not in
    userland/target/x86_64-unknown-none/release.
  The ppoll in the puzzle must come from a relibc-linked peer or daemon, not from
  Firefox's own poll, since musl calls SYS_ppoll directly in both A and B. Check
  that first: it may move the lead off the POLL_TAG_ALL DRM fds.
- Next: list the relibc-linked processes running during playback, deploy-files
  their 9928b292 builds plus this kernel, then run m.sh hw
  (scratchpad/p9/m.sh + get.sh) and compare with A 212.8/130.9 and B 243.8/152.2.

## lane ppx86 (2026-10-20) — x86_64 failures were flaky stray wakes; x86_64 regress ppx86r1 all PASS
- pp-regress nettag2 tcp_recv (2/3/1 wakes per window) and pp-pollnarrow
  console_read_alias (1 wake in 12000 trips) did not reproduce: x86_64/TCG
  reruns ppx86n1 (nettag2) and ppx86p1 (pollnarrow) PASS.
- Logic: the lane cannot add wakes. The preskip path installs the narrow
  first-pass tags directly; the fallback widens to ALL and does the full
  pass exactly as before; gen_now only cancels fewer parks. Lost-wake check:
  every poll-channel wake (unblock_port, _tagged, _keyed, _exclusive[_keyed],
  try_wake_poll_key, signal wakes) funnels into unblock_port_tagged_inner,
  whose first statement bumps POLL_WAKE_EPOCH under RUN_QUEUE; timerfd
  expiry bumps too. No code change.
