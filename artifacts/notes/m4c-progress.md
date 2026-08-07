# M4c wave — progress + CRITICAL diagnosis correction

Owner: deep-reasoner M4-final wave. Exclusive git/QEMU/images. Resume "continue from checkpoint".
main @ 5120ff9 at start.

## STEP 0 — tree state verified
- git status: only .claude/skills/run-leandros/driver.py modified (+ driver.py.bak untracked).
  driver.py change = _serial_send prompt-sync ("#" wait) + 2-space pad (harness robustness, NOT QMP).
  Keep it, commit as tooling; delete driver.py.bak.
- Mesa checkout ~/.claude-forain/jobs/afde2e74/tmp/mesa-wave2/src/mesa STILL EXISTS.

## STEP 1 — Mesa patch preservation: MOOT (verified, nothing to preserve)
- gbm_dri.c == gbm_dri.c.orig (IDENTICAL), 0 [GBM] markers. Source is STOCK.
- The prior wave (m4b-progress.md) already proved the real fix was the KERNEL ioctl
  sign-extension mask, committed 8a2a271 ("cmd & 0xFFFF_FFFF"). Mesa was reverted to stock,
  ports/mesa/0001-*.patch deleted. NO ports/mesa patch exists or is needed. The coordinator's
  "unpreserved Mesa fix" premise is STALE — it predates the sign-ext discovery. Confirmed.

## STEP 2 — CRITICAL: briefing's MSG_DONTWAIT root cause is REFUTED (would be a NO-OP fix)
Evidence chain (all static, high confidence):
- Client hang (m4bc2-aarch64-serial.log): wlclient prints "connected to display" then
  "roundtrip: requesting globals..." then NOTHING. Hangs in the FIRST wl_display_roundtrip().
- libwayland 1.23.1 (src/wayland-1.23.1): display fd is SOCK_CLOEXEC only, NOT nonblocking
  (wayland-os.c wl_os_socket_cloexec + set_cloexec_or_close set only FD_CLOEXEC). recvmsg uses
  MSG_DONTWAIT (connection.c:544); sendmsg uses MSG_NOSIGNAL|MSG_DONTWAIT (connection.c:492).
- wl_connection_read (connection.c:516) IS a while(1) drain loop that terminates on
  recvmsg->EAGAIN — briefing correctly ID'd this loop. BUT wl_display_dispatch_queue's actual
  blocking primitive is poll(fd, POLLIN, -1) (wayland-client.c ~1956), THEN read_events.
- OUR KERNEL ALREADY HONORS MSG_DONTWAIT: syscall.rs:5778 net_blocking_op computes
  `nonblock = flags & MSG_DONTWAIT(0x40) != 0 || net_fd_nonblock(...)` and BOTH sys_recvmsg(5818)
  and sys_sendmsg(5810) route through it. So recvmsg(MSG_DONTWAIT) on empty returns EAGAIN, the
  drain loop terminates, NO block. => threading MSG_DONTWAIT into net/src/lib.rs handle_recvmsg/
  handle_sendmsg (briefing's fix) changes NOTHING. MSG_NOSIGNAL also moot (we deliver no SIGPIPE).
- The real block is poll(POLLIN,-1) waiting for anvil's registry+sync reply that never arrives.
  => problem is SERVER-SIDE: anvil is not servicing the client, NOT a client recvmsg block.

## Kernel paths audited, all CORRECT (not the bug):
- net server handle_poll: UnixListening reports POLLIN when a connect is pending vs its sock_id
  (lib.rs:2118); UnixConnected reports POLLIN when ring readable>0 (lib.rs:2100). 
- unix connect/accept: pre-accept writes buffer in the conn_idx ring; accept pairs by sock_id.
- sys_poll (syscall.rs:2105) is a correct check-then-block loop via probe_fd_events.
- These paths were proven by M1/M2 Rust wayland client<->compositor roundtrips.

## HYPOTHESES for why anvil doesn't service the client (need runtime evidence):
  H1 anvil never returns to epoll_wait after first frame — blocked in a DRM ioctl
     (page-flip/WAIT_VBLANK) our kernel never completes. (cursor tracked tablet in m4br D/E,
     but that was a client-less run; a first client buffer-commit/page-flip may differ.)
  H2 anvil's calloop registers the client fd but our epoll edge-seq/EPOLLET interaction misses
     the readiness edge for the accepted fd.
  H3 anvil accepts but never reads (ordering/level-vs-edge), or reads but never replies.
DECISIVE TEST: instrument net server unix connect/accept/sendmsg/recvmsg (pid+bytes) -> ONE
  aarch64 run shows exactly where the client<->anvil exchange stops.

## NEXT (resume here)
1. [pending] commit driver.py serial-sync fix (tooling), rm driver.py.bak.
2. [pending] instrument net server unix traffic; build aarch64 release kernel; regen image;
   run anvil+wlclient with m4_capture; localize stall (accept/read/reply).
3. Do NOT implement the briefing's net-server MSG_DONTWAIT fix — verified no-op.

## STEP 1 done — driver.py serial-sync fix committed 06defe1, driver.py.bak deleted. tree clean.

## STEP 2 done — full kernel socket-path audit complete. ALL PATHS CORRECT:
- net_blocking_op (syscall.rs:5778) honors MSG_DONTWAIT for sys_recvmsg + sys_sendmsg. [no bypass;
  unconditional dispatch 1100-1101; unix handled inline in handle_recvmsg]
- handle_send (net lib.rs:1209) calls sched::wake_poll() on n>0 -> anvil's plain-data reply
  (registry globals + wl_callback.done, no fds) WAKES the client's poll(POLLIN,-1). handle_sendmsg
  plain-data path (1552) delegates to handle_send; fd-carrying path wakes at 1648.
- handle_poll (lib.rs:2079): UnixListening POLLIN on pending connect vs sock_id (2118); UnixConnected
  POLLIN on ring readable>0 (2100).
- connect/accept: pre-accept writes buffer in conn_idx ring; accept pairs by sock_id.
CONCLUSION: kernel is NOT the blocker. Briefing's net-server MSG_DONTWAIT/MSG_NOSIGNAL fix is a
  VERIFIED NO-OP. Client hangs in poll() waiting for anvil's reply => stall is anvil-side (userspace).

## Prior client runs were HARNESS-CORRUPTED (driver.py drop bug) — evidence tainted:
  m4b-exit2 log is polluted by the echoed shell filter + "hellosleep" head-drop; its Vblank/ERROR
  counts are command TEXT, not anvil output. So the earlier "anvil services roundtrip" and even the
  "cursor tracks tablet" reads sit on shaky harness runs. Only m4bc2 (client hang at "requesting
  globals") is a clean-ish datapoint, and it only shows WHERE (first roundtrip), not WHY.

## DECISIVE NEXT EXPERIMENT (hand-off ready) — localize the anvil-side stall in ONE clean run:
  Instrument net server unix paths (tag "UNIXTRACE"): handle_connect(AF_UNIX), handle_accept(unix
  branch success, print pid), handle_sendmsg/handle_send + handle_recvmsg on unix (print pid + bytes).
  Build aarch64 RELEASE kernel, regen f2fs image, boot uefi-tcg, start anvil (ANVIL_DRM_DEVICE=
  /dev/dri/card0 SMITHAY_USE_LEGACY=1 XDG_RUNTIME_DIR=/run/user/0), run wlclient via the FIXED
  driver.py, capture with ~/code/leandros-artifacts/m4_capture.py. Read the UNIXTRACE sequence:
    (a) no anvil accept for the client  -> anvil calloop/epoll not waking for listener (kernel epoll
        edge vs calloop registration) OR anvil blocked before returning to epoll.
    (b) anvil accept + recvmsg(client bytes) but NO anvil sendmsg reply -> anvil wayland dispatch /
        globals advertisement path (smithay) stuck, or anvil blocked in DRM present/page-flip after
        first frame (never returns to service the queued client request).
    (c) anvil sendmsg reply present but client still stuck -> client-side poll wake (would contradict
        handle_send wake; re-check probe_fd_events edge-seq / poll_block).
  Print macro: use the same path that emits [EXIT]/[SYSCALL] (reaches driver.py serial socket); NOT
  pci::rdebug (not captured).

## STILL TODO (unchanged from ladder, AFTER real root cause found): x86_64 pass, regression both
  arches (vfstest FIRST, drmsmoke 20/20, scmtest, epolltest, evtest2, idletest, kmscube -D),
  screenshots to notes/m4-screenshots/, plan-doc M4 update.

## STEP 3 in progress — UXTRACE run
- Instrumented syscall.rs: gated uxtrace() (const UXTRACE=true) on sys_accept(ACC)/sys_connect(CON)/
  sys_sendmsg(SND, v>0)/sys_recvmsg(RCV, v>0). Prints "UXTR <tag> pid=<hex> fd=<hex> v=<hex>" to
  serial (crate::serial_print_str/hex). NOT committed (WIP; remove or keep-gated before final).
- build-all.sh --arch aarch64 rc=0 -> fresh leandros-limine-aarch64.img + f2fs-data0/1 @ 20:56.
- Diag driver: ~/code/leandros-artifacts/m4c_diag.py (persistent serial: login->anvil->wlclient->
  dump wl.log+anvil.log+ps). Reads UXTR to localize per the 3-outcome table.

## STEP 3 finding #1 — anvil PANICS on missing XDG_RUNTIME_DIR (harness bug, not the stall)
- UXTRACE run: anvil pid=6 exited code=101 (Rust panic) ~20s in, before any client:
    thread 'main' (6) panicked at anvil/src/state.rs:635:60:
    called `Result::unwrap()` on an `Err` value: RuntimeDirNotSet
  then wlclient pid=7 exited code=1 (no compositor to connect to). So the earlier "anvil hang"
  framing was never reproduced here — anvil died on missing env.
- CAUSE: my m4c_diag.py send_line lacked the PL011 head-drop guard (2-space pad) that driver.py
  got in commit 06defe1, so `export XDG_RUNTIME_DIR=...` lost its head -> var unset -> panic.
  This ALSO means every prior wave's anvil run depended on that same env delivery; the harness is
  the fragile link. Fixed diag: 2-space pad in send_line + inline env prefix on the anvil/wlclient
  launch (verified brush supports `VAR=val cmd`) + echo RTDIR verification. Re-running.
- anvil.log confirms anvil got as far as GPU selection ("...d0 as primary gpu", xcursor WARN) before
  the panic -> once env is set it proceeds into EGL/softpipe init. state.rs:635 is smithay's
  XdgActivation/socket runtime-dir unwrap.

## STEP 3 finding #2 — export->child propagation WORKS; serial corruption of long lines was the issue
- Verified via driver.py (robust send): `export XDG_RUNTIME_DIR=/propX; WAYLAND_DISPLAY=nodisplay9
  wlclient` -> wlclient printed "XDG_RUNTIME_DIR=/propX" = children DO inherit exports.
- The 2nd diag's wlclient got XDG_RUNTIME_DIR=(null) purely because the long inline-env launch line
  was serial-corrupted (WAYLAND_DISPLAY survived, XDG token dropped). Guest has NO ps/kill/grep/sh.
- FIX: diag now uses only short exports (proven) + short plain launch lines, no inline, no long
  lines. Fresh QEMU restart each time (no ps/kill to clear a stray anvil). Re-running (run bs73krcl2).

## STEP 3 RESULT — real hang REPRODUCED + localized (clean run, env correct)
- ENV=[/run/user/0][wayland-1], anvil survived startup. wlclient: "connected to display" ->
  "roundtrip: requesting globals..." then HANGS (== m4bc2 symptom, now reproduced cleanly).
- FULL UXTR trace = exactly ONE line: `CON pid=6 fd=256 v=0` (client connect OK). NO ACC, NO SND,
  NO RCV afterward. => anvil NEVER accepts the connection; client never even flushes its request.
- anvil.log: created socket wayland-1 (line 7), reached "Creating new Output" (guest 43.63s, line 40)
  and STOPS. Frozen at 40 lines across multiple checks spanning minutes. So anvil is stuck DURING
  DRM output init, BEFORE entering its calloop event loop -> that's why it never accept()s (outcome A
  of the 3-outcome table).
- CPU discriminator: QEMU at 296% CPU (3 vCPU threads pegged 55-86%). anvil is COMPUTE-BOUND, NOT
  syscall-blocked. => NOT the flip-event wait (that would be idle). Most likely softpipe first-frame
  render / GLES shader compile under TCG = pathologically slow (prior wave DID eventually render a
  desktop, so path completes given enough wall time). Currently polling anvil.log for forward progress
  to confirm slow-vs-stuck.
- IMPLICATION if "just slow": M4 exit under pure TCG softpipe may be impractical on wall-clock; the
  code path is likely correct. Need to either (a) give anvil much longer, (b) shrink the output mode
  (1280x800 is heavy), or (c) accept that HVF/real-GPU is required for a timely exit. TBD by the poll.
