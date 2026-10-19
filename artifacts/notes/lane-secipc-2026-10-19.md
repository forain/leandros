# lane/secipc — 2026-10-19 (bug list L7.1, L7.3)

## L7.1 — credential checks on the Leandros-private syscalls

Audit of who uses them:
- SYS_IPC_SEND / SYS_IPC_CALL (511/513): doom's SDL shim and privtest send to
  the audio server port (auxv 258); racetest ping-pongs between a root parent
  and its root child over SYS_PORT_CREATE ports. relibc and userland/libc only
  have wrappers. Every published server port (VFS, net, audio, drm, evdev,
  f2fs) is a kernel handler port that already checks the caller per operation.
- SYS_SPAWN (510): `sched::spawn_user` is a stub that always returns None; the
  relibc wrapper has a different (path, argv, envp) signature and no callers.
- SYS_DBG_SERIAL_WRITE (590): compiled to a no-op (gate const off); only
  out-of-tree instrumentation patches ever called it.

Policy (kernel/src/syscall.rs `ipc_send_permitted`, sched/src/cred.rs):
- Handler (server) ports: open to everyone, as before.
- Queue ports: root, the owner's own process, or a caller whose euid/egid
  equals every one of the owner's real/effective/saved uid/gid
  (`ptrace_may_access`-style, not `kill`-style: the kill rule would let a user
  forge the VFS replies a setuid program it started is waiting for).
  Missing owner task: root only. EPERM otherwise.
- sys_send also checks a caller-chosen `msg.reply_port` (if it exists): a
  server handler writes its answer there, so an unchecked one let any user
  plant a server reply in another user's reply queue. sys_call stamps its own
  reply port, so only the target is checked.
- SYS_SPAWN and SYS_DBG_SERIAL_WRITE: root only (EPERM).

## L7.3 — AT_RANDOM

Was `ticks ^ const` / an LCG of the tick count. Now 16 bytes from
`sched::random::fill` (the existing ChaCha20 CSPRNG seeded from virtio-rng,
DTB rng-seed, RNDR/RDRAND and cycle-counter jitter).

## Tests (aarch64, HVF)

privtest gained: `los_spawn_non_root_eperm`, `los_dbg_serial_write_non_root_eperm`,
`los_ipc_send_own_port_ok`, `los_ipc_send_root_port_eperm`,
`los_ipc_call_root_port_eperm`, `los_ipc_send_reply_to_root_port_eperm`,
root positives `los_dbg_serial_write_root_ok` / `los_ipc_send_root_own_port_ok`,
and `atrandom_differs_across_execs` (8 back-to-back re-execs, all distinct,
none zero). LeandrOS-only cases SKIP on Linux. privtest PASS; regress 36/36 PASS.

## Open

- `execv("/proc/self/exe")` fails on LeandrOS (test uses readlink of it instead).
- Port-id reuse between the check and the enqueue is a theoretical TOCTOU
  (ids are monotonic mod MAX_PORTS, so reuse needs a full wrap).
- x86_64 not run in this lane (per protocol).
