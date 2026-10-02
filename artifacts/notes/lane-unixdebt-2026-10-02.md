# Lane unixdebt — AF_UNIX / fd kernel debts (2026-10-02)

Branch `lane/unixdebt`, worktree `.claude/worktrees/unixdebt`, base `08ac17c`. Not pushed, not merged.

## Status (2026-10-02, evening): DONE
Everything is verified, and the two former gaps are closed too. Nothing remains open in this lane.

**Why QEMU died at `driver start` in the paused run:** it was not disk space or a stale lock. `/tmp/leandros-unixdebt-qemu-stderr.log` says `-vnc 127.0.0.1:61: Failed to find an available port: Address already in use`. Another session's QEMU was holding VNC port 5961 at that moment. The runs were redone on `LEANDROS_VNC_PORT=5983`, after checking that the port was free with `lsof -iTCP:5983`.

Env used: `LEANDROS_RUN_ID=unixdebt LEANDROS_VNC_PORT=5983 LEANDROS_QEMU_MEM=4G FFSESSION_OUT=<scratch>/ff`.
Disk note: the first build hit ENOSPC on the full host disk. It failed on its very last step, `cp f2fs-data0 f2fs-data1`, after everything else had been built. data1 was then made with `cp -c` (an APFS clone that takes no extra space). The second build ran with 42 GB free and went through cleanly.

## Commits
- `c88f6af` vfs: prune every socket alias at exec, not the first 16
- `0961bd1` net: read()/recv() on a unix stream closes the fds it reads past
- `145bf9f` net: garbage-collect AF_UNIX ends that only their own queues keep alive
- `7d42d25` net: MSG_PEEK on AF_INET leaves the data queued
- `7c07617` net: unix GC follows listeners and their embryonic connections

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
4. **MSG_PEEK on AF_INET** (gap 1, closed): TCP uses `peek_slice` and UDP uses `peek`, so a peek no longer dequeues. This covers recv, recvfrom and recvmsg. Raw ICMP still consumes on peek, because smoltcp has no peek there.
5. **GC through a listener's backlog** (gap 2, closed) [Linux: embryos sit in the listener's receive queue, and unix_gc walks them].
   - Listeners are now graph nodes, with BoundPath::refs as their refcount.
   - The edges of a candidate listener are its embryos: end B of each pending connect whose sock_id matches. Those ends become candidates too.
   - An unreached listener is collected by purging its embryos' queues. That releases the address.
   - The pending connects are found by scanning SOCK_TABLES. That scan runs only while `INFLIGHT_LISTENERS` > 0, and SOCK_TABLES stays held for the whole pass (lock order SOCK_TABLES > UNIX_CONNS > BOUND_PATHS).

## Tests (scmtest)
`unix_gc_self_cycle` (600 rounds, more than MAX_CONNS 512, plus a pipe-EOF check), `unix_gc_two_conn_cycle` (300 rounds × 2 conns), `unix_gc_keeps_reachable`, `read_discards_fds` (28 steps: whole batch, partial reads across a boundary, no control buffer, MSG_PEEK), `exec_prunes_many_aliases` (24 aliases), `unix_gc_listener_backlog` (20 rounds of listener-in-own-backlog with a check that the address is released, plus a reachable variant), `inet_msg_peek` (TCP and UDP; recvfrom and recvmsg).

## Results (final tree `7c07617`)
- build-all.sh: OK.
- 13 suites + vfstest, **aarch64/HVF: 14/14 RC=0** and **x86_64/TCG: 14/14 RC=0**. scmtest SCMRC=0 on both arches, and all 7 new cases PASS on both.
- Desktop boot: aarch64 virgl (greeter login, panel, cosmic-term, then Firefox on top of it) and x86_64 virgl (`run-deskx/term.png`: Orion wallpaper, panel, cosmic-term with a brush prompt).
- Firefox aarch64/HVF virgl, `https://en.wikipedia.org/wiki/Unix_domain_socket`, `--wait 240`. Launched 17:39:35 and still running when the session stopped at 17:44:07, which is more than 4.5 min. The page rendered, logos included. ff.log has no IPDL, channel, crash or EXIT lines; the only WARN lines are WebRender shader notices. ps.txt lists the Firefox process tree alive at the end of the run. Proof: `lane-unixdebt-firefox-wikipedia-aarch64.png`.

## Known gaps
- Raw ICMP sockets still consume data on MSG_PEEK (smoltcp's icmp::Socket has no peek).
