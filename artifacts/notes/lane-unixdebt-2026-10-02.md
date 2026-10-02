# Lane unixdebt — AF_UNIX / fd kernel debts (2026-10-02)

Branch `lane/unixdebt`, worktree `.claude/worktrees/unixdebt`, base `08ac17c`. Not pushed, not merged.

## RESUME HERE (paused 2026-10-02, usage limit)
State: all three fixes are committed. Each has scmtest regression cases, and the 13 suites plus vfstest pass on both arches.
Still to do:
1. Desktop boot on both arches: `ffsession.py <arch> desk --nofirefox`.
2. Firefox Wikipedia session on aarch64, at least 3 min with no IPC errors: `ffsession.py aarch64 wiki --url https://en.wikipedia.org/wiki/Unix_domain_socket --wait 240`. The one attempt so far did not test anything. QEMU (`--virgl`) died right at `driver start` ("shell prompt not seen", then "cannot connect to serial socket"), so the boot never reached the guest. The cause has not been looked at. Possible causes: host contention (several other lanes' QEMUs were running) or low disk space. Retry first, then read `qemu-stderr`.
3. Optional: show that the new tests fail on the base kernel. Revert `servers/` to `08ac17c`, keep the tests, and rebuild aarch64.
Env used: `LEANDROS_RUN_ID=unixdebt LEANDROS_VNC_PORT=5961 LEANDROS_QEMU_MEM=4G FFSESSION_OUT=<scratch>/ff`.
Disk note: the host disk was full (ENOSPC). build-all.sh then failed on its very last step, `cp f2fs-data0 f2fs-data1` for x86_64. Everything before that step had already been built, so the build itself was complete. I made data1 with `cp -c`, an APFS clone that uses no extra space. I did the same on aarch64 to free 3.2 GB.

## Commits
- `c88f6af` vfs: prune every socket alias at exec, not the first 16
- `0961bd1` net: read()/recv() on a unix stream closes the fds it reads past
- `145bf9f` net: garbage-collect AF_UNIX ends that only their own queues keep alive

## The fixes (Linux reference in brackets)
1. **Unix GC** [net/unix/garbage.c unix_gc; unix_release_sock purges the receive queue].
   - `unix_gc()` runs after any end loses a reference, while `INFLIGHT_SOCKS` > 0.
   - An end is a candidate when refs == its queued in-flight copies. Live non-candidates are roots.
   - The collector marks everything reachable from the roots' receive queues. The receive queues of candidates it does not reach are lifted out and passed to `xfer_drop`. The normal close path then frees the connections.
   - Everything runs in one UNIX_CONNS critical section. A reference that is in transit (counted in refs but not in any queue) never makes its end a candidate.
   - Re-entry is guarded with GC_ACTIVE/GC_AGAIN.
   - Also new: closing an end releases the fds still queued for it (`UnixConn::end_put`), and `handle_close_all` uses that helper too.
2. **read()/recv() with queued fds** [unix_stream_read_generic + scm_recv; unix_peek_fds].
   - One reader, `unix_stream_read_locked`, now serves both the recv and recvmsg paths.
   - A batch is taken at the first of its bytes that is read. The read stops at the batch's `end_byte`.
   - When the call has no control buffer, the fds are closed (recvmsg also sets MSG_CTRUNC).
   - MSG_PEEK is new. The kernel used to ignore it, and recvfrom never passed its flags on. Peeking copies the bytes without consuming them. On a stream it stops at a batch boundary. With a control buffer, recvmsg(MSG_PEEK) installs new references to the batch's fds and leaves the batch queued.
3. **prune_sock_aliases**: replaced the fixed 16-entry array with a Vec.

## Tests (scmtest)
`unix_gc_self_cycle` (600 rounds, more than MAX_CONNS 512, plus a pipe-EOF check), `unix_gc_two_conn_cycle` (300 rounds × 2 conns), `unix_gc_keeps_reachable`, `read_discards_fds` (28 steps: whole batch, partial reads across a boundary, no control buffer, MSG_PEEK), `exec_prunes_many_aliases` (24 aliases).

## Results
- build-all.sh: OK (see the disk note above).
- aarch64/HVF: scmtest, sigtest, sigtest2, memtest, polltest, forktest, exectest, pthreadtest, epolltest, timertest, jobtest, waittest, sigchldtest and vfstest all RC=0. All five new cases PASS.
- x86_64/TCG: the same 14 all RC=0. runtests printed RC=? for scmtest because it lost the serial capture, but serial.log has RC=0. I reran scmtest on its own: SCMRC=0, and all five new cases PASS.
- Desktop boot on both arches and the Firefox session: NOT DONE (see RESUME HERE).

## Known gaps, not addressed
- MSG_PEEK on AF_INET sockets still consumes the data. The new peek support covers unix sockets only.
- The GC treats listeners and pending-accept connections as roots or leaves. A cycle that runs through a listener's backlog would still leak. It cannot form today, because no fds can be queued on a connection until it is accepted.
