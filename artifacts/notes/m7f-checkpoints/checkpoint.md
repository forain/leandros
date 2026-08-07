# M7f checkpoint

## State
- Main at 0107752 (verified). Tree: untracked ports/busd/.work/ (ephemeral). Clean otherwise.

## Mission A (busd wedge) — code analysis so far
- Coordinator's "5c43227 broke PendingAccept POLLOUT edge-seq -> EPOLLET storm on busd fd" hypothesis has a STRUCTURAL PROBLEM: busd's accepted peer fd is UnixConnected(is_a=false), NOT UnixPendingAccept. The PendingAccept arms only touch cosmic-comp's CLIENT side, which M7e proved is NOT wedged (completes handshake).
- Epoll edge-seq logic (kernel/src/syscall.rs 6267-6317) is SOUND: EPOLLET fire = cur!=0 && seq!=last_seq; last_seq committed after fire. Writable-forever socket only re-fires POLLOUT when conn.seq advances, and seq only advances on real I/O. NO free-running storm.
- => Decisive test still the ring-tracer (spinning vs silent). PENDING.

## Mission B (scmtest hang) — ACTIVE
- test_fd_pass PASSES (1 SCM_RIGHTS msg). test_cmsg_flags HANGS (2 back-to-back SCM_RIGHTS msgs on stream socketpair; child does 2 recvmsg, round A truncates control buf to 4 bytes -> MSG_CTRUNC).
- Strong suspect: kernel fd-to-stream-byte-offset association (PendingFdBatch pinning) interacting w/ truncated-cmsg path. Reading net server SCM_RIGHTS now.

## Next
1. Read net server sendmsg/recvmsg SCM_RIGHTS + fd-batch/offset code.
2. Root-cause scmtest hang.
3. Ring-tracer for busd wedge (heavy run).

## MISSION B RESULT (aarch64) — scmtest is NOT hanging; capture artifact
- Ran scmtest via scripts/scmrun.py (persistent serial reader) on the 0107752 aarch64 image.
- RESULT: FULL COMPLETION, ALL PASS. fd_pass, cmsg_flags, shared_memfd_pixels, seals, double_mmap_alias,
  read_mmap_coherence, big_memfd, fork_visibility, partial_munmap, close_while_mapped,
  ftruncate_grow_shrink, teardown_loop, socket_node_roundtrip, socket_node_devshm, unlink_rebind,
  many_socketpairs_and_listeners, tmpfs_mounts_exist, devshm_shared_mmap, queued_fd_cap. "--- scmtest done ---".
- => M7e's "hang after test_fd_pass" = DRIVER EARLY-BREAK ARTIFACT. driver.py's shell-prompt heuristic
  trips on scmtest's "-> " diagnostic lines (test_fd_pass prints "read via received fd -> 5 bytes").
  scmrun.py was created for exactly this. M7e/M7c ran via the wrong capturer and misread truncation as a hang.
- My earlier static trace of test_cmsg_flags was CORRECT (it passes). No kernel bug.
- NEXT: confirm x86_64 via scmrun.py too. If clean, Mission B is CLOSED (open issue = false alarm).

## MISSION B RESULT (x86_64) — also FULL PASS, 19/19. MISSION B CLOSED.
- x86_64 scmtest via scmrun.py: all 19 subtests PASS, "--- scmtest done ---".
- BOTH ARCHES CLEAN. scmtest "hang" was a driver.py early-break capture artifact on both. NO bug.
- Fix: track that scmtest MUST be captured via scmrun.py, not driver.py cmd. Close the "open issue".

## PIVOTING to Mission A: busd wedge-state ring-tracer.

## MISSION A — DECISIVE: Alpine control proves W1 is LeandrOS-SPECIFIC (busd/zbus exonerated)
- Executor model settled by zbus 5.13.1 source: under busd's `tokio` feature, Executor = zero-sized
  PhantomData; spawn->tokio::task::spawn (current runtime); is_empty()==true; tick()->pending().await;
  the std::thread start_internal_executor is #[cfg(not(tokio))] => cfg'd OUT. M7e CORRECT, M7b's
  "zbus internal-executor runner thread" model REFUTED by source.
- busd main = #[tokio::main(flavor="current_thread")]. peers.rs uses tokio::sync::RwLock (async, correct).
- Peer::new: connection::Builder::socket().server().p2p().build().await does the server AUTH handshake +
  spawns the connection's socket_reader; `busd::peer::stream created:` logs right after AUTH succeeds.
  add() HOLDS peers.write() across Peer::new().await.
- ALPINE CONTROL (docker linux/arm64, the EXACT static-musl aarch64 busd binary from ports/busd/.work):
  harness in /tmp/busd-alpine (busd + session.conf + client.py coalescing D-Bus client + run.sh).
  * NORMAL client (step-by-step handshake): HELLO_ANSWERED (264B). threads=2, State=S.
  * COALESCED client (NUL+AUTH+NEGOTIATE_UNIX_FD+BEGIN+Hello in ONE send): HELLO_ANSWERED (316B). healthy.
  => busd/zbus handle coalesced AUTH+Hello CORRECTLY on real Linux. NOT an upstream bug. W1 is LeandrOS-specific.
- CAVEAT: my coalescing client connects to busd's listener but on Linux the pre-accept bytes are buffered
  natively — it does NOT specifically exercise LeandrOS's 5c43227 connect-write-BEFORE-accept path. That
  path (UnixPendingAccept send/poll arms) is the PRIME untested LeandrOS-specific suspect on the W1 path.
- M7b's on-LeandrOS kernel ring dump (old kernel) already showed: busd runtime threads PARKED in futex_wait,
  "NO thread ever reads comp's peer socket (fd ~260)", NO wake issued => SILENT wedge (not a spin). Alpine
  shows busd DOES read+answer. So on LeandrOS busd's reader is never polled to read the peer socket.
- FIX PATH (escalation): NOT a vendored busd/zbus patch (exonerated). It's a LeandrOS syscall/scheduling
  divergence on the coalesced busd<->comp handshake that deadlocks busd's current_thread runtime. Next:
  (a) run a COALESCING client as a guest binary vs real busd on LeandrOS (desktop-free minimal repro), or
  (b) busd-armed ring-tracer during real cosmic-comp boot -> exact divergent syscall.
  Prime suspect: 5c43227 pre-accept buffering / accept edge-seq transfer for the peer fd.

## NO CODE CHANGES this wave. Close-out: scmtest observed 19/19 BOTH arches (done). Spot-check vfstest+wakepoll next.
