# M1 exit-gate progress (wltest)

## Build: DONE both arches (static ET_EXEC, rs backend)
- Project: /Users/forain/.claude-forain/jobs/afde2e74/tmp/m1-wltest
- wayland-server 0.31.13 + wayland-client 0.31.14 pure-rs backend; rustix+linux-raw-sys => RAW syscalls.

## KERNEL FINDING #1: FIONBIO on socket fds -> ENOTTY  [FIX APPLIED, building]
- Rust std UnixListener/UnixStream::set_nonblocking issues ioctl(FIONBIO), NOT fcntl.
- sys_ioctl (kernel/src/syscall.rs ~5434) FIONBIO arm was scoped `fd < SOCK_FD_BASE`
  (VFS only); socket fds fell through terminal-ioctl tail -> tty ENOTTY(25).
  => wayland-server rs backend died at listener bind ("Not a tty (os error 25)").
- FIX: added socket-range FIONBIO arm [SOCK_FD_BASE,EPOLL_FD_BASE) routing to
  net_server NET_GETFL/NET_SETFL toggling the SAME O_NONBLOCK(0x800) bit fcntl uses
  (handle_setfl/getfl share socks[slot].nonblock). User int read via validate_user_buf
  + raw read BEFORE net_server::handle lock (spinlock invariant honored).
- Verified net contract: handle_getfl returns 0x800/0; handle_setfl sets nonblock=flags&0x800.

## COMMITTED: FIONBIO socket fix = e980eb0

## KERNEL FINDING #2: AF_UNIX accept() returns malformed peer sockaddr  [FIX APPLIED, building]
- servers/net/src/lib.rs handle_accept UnixListening branch (~998): zeroed only 2
  bytes of addr (sun_family=0=AF_UNSPEC) and NEVER wrote *addrlen_ptr. std
  UnixListener::accept parses peer sockaddr; addrlen stayed = sizeof(sockaddr_un)
  (nonzero) with family 0 => rejects "file descriptor did not correspond to a Unix
  socket". Server accept() failed; client saw Broken pipe at registry roundtrip.
- Also: that user-memory write happened UNDER SOCK_TABLES.lock() (dropped at 1001)
  => the fault-under-spinlock freeze hazard. FIX moves writes AFTER drop(tbls),
  sets sun_family=AF_UNIX + *addrlen_ptr=2 (unnamed-peer; std accepts).

## Run #1 (x86_64, pre-FIONBIO): server ENOTTY at bind. Fixed by e980eb0.
## Run #2 (x86_64, post-FIONBIO): server listens+globals, client connects, then
   accept() "not a Unix socket" + client Broken pipe. -> Finding #2.

## COMMITTED: finding #2 = e76324b (net accept sockaddr). build3 EXIT=0, both images fresh.

## RESULT x86_64: VERDICT PASS. Full flow: registry roundtrip, SCM_RIGHTS memfd (fd4),
   MAP_SHARED alias, gen0 verify OK, gen1 live re-read OK (corners changed
   deadbeef->dfacbfee etc = live alias, not copy). commits=2, all steps OK.

## DONE — M1 EXIT GATE PASSED
- wltest x86_64: VERDICT PASS (all steps, gen0+gen1 live-alias)
- wltest aarch64: VERDICT PASS (interleaved output, both PIDs exit 0)
- scmtest aarch64: 19/19 PASS, 0 FAIL
- idletest aarch64: IDLE_CPU_US 0, idle_cpu PASS, timer_wake PASS
- Kernel commits: e980eb0 (FIONBIO sockets), e76324b (AF_UNIX accept sockaddr)
- mkfs tweak reverted; git working tree clean; wayland_cosmic_plan.md untouched
  (tracked by pre-existing 3a3120a, not my commits); QEMU killed.
