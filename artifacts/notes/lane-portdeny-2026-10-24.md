# lane/portdeny (L1.19) - handler ports deny user sends by default - 2026-10-24

**FIXED** (main was already closed after copyraw/pidtrust/openid; this lane flips the default
so a future or forgotten port fails closed, and adds a preventive guard).

## Change
- `ipc/src/port.rs`: `user_send_allowed` now returns false for any handler port that has no
  `register_user_filter` filter (and always for `register_kernel_handler` ports). Plain
  `register_handler` = no user sends (EPERM from `sys_send`/`sys_call`) until a filter names the
  tags user code may send. Queue ports (no handler) keep the owner-credential check only.
  In-kernel `port::send` (VFS forwards, replies) is never filtered. The window between
  `register_handler` and `register_user_filter` fails closed.
- The only user-reachable paths are `sys_send` and `sys_call` (kernel/src/syscall.rs); both
  already consult `user_send_allowed`. `net_server::force_bind_unix(_, port)` ignores the port.

## Classification of every handler registration (git grep register_handler, 6 sites)
| port | server (site) | class | user tags allowed | identity source |
|---|---|---|---|---|
| VFS (auxv 256) | servers/vfs/src/lib.rs:4359 | (a) kernel-only | none | n/a (kernel callers only) |
| f2fs | servers/f2fs/src/lib.rs:7710 | (a) kernel-only | none | n/a |
| evdev | servers/evdev/src/lib.rs:1859 | (a) kernel-only | none | n/a (pid slot 3 kernel-filled) |
| drm | servers/drm/src/lib.rs:488 | (a) kernel-only | none | n/a (pid slot 3 kernel-filled) |
| netctl | servers/net/src/netctl/mod.rs:648 | (a) kernel-only | none | n/a (NM uses /dev/netctl via VFS) |
| audio (auxv 258) | servers/pipewire/src/lib.rs:336/343 | (b) user filter | 0x100 SET_PARAMS, 0x200 PCM, 0x300 PUMP, 0x1000 PING | 0x100/0x200: `ipc_claim(caller_pid)` (kernel-supplied handler arg, tgid + dyn_dev_may_write); 0x300/0x1000 carry no identity |

auxv 257 (net port): `set_net_server_port` has no caller, so it is u32::MAX; not a handler port.

## Userland senders checked
- relibc: `leandros_ipc_send/call` (src/header/leandros/mod.rs) have no callers in relibc or the tree.
- userland/: raw IPC only in aplay (audio port, 0x100/0x200), privtest, racetest (its own
  queue ports from SYS_PORT_CREATE, no handler -> unaffected). userland/libc reads auxv 256-258.

## Guard - privtest `los_ipc_handler_ports_deny_default` (+ `los_ipc_audio_ping_allowlisted_ok`)
Root forks a child dropped to uid 54321 (no other process has it, so no queue port accepts);
it sends every VFS tag 0x10-0x30 and 0x101 to ports 1..1023 (fd/open_id -1); any success fails.
Positive control: audio PING 0x1000 to auxv 258 must still succeed.
- Main is already closed (all 6 sites classified above), so on plain main the guard is preventive.
- RED probe (main default + pipewire `register_user_filter` commented out = a forgotten filter):
  `FAIL (1 ports accepted a user VFS tag: 4/0x10)` (tests-portdeny-red).
- GREEN (lane): both PASS, privtest 0 failures (tests-portdeny-g2).

## Verification (aarch64 release, HVF)
- Full regress (tests-portdeny-green): all RC=0 except nettest tcp_keepalive_probes
  (192.168.105.1:53 connect failed, network); rerun nettest PASS (tests-portdeny-g2).
- sndtest PASS, `aplay test` RC=0, privtest PASS (tests-portdeny-g2).
- sshsmoke (in regress) PASS.
- net-e2e: all RC=0 except nmrealtest reboot ("no login prompt after reboot") in a run that
  overlapped an x86_64 build and a second QEMU; nmrealtest rerun alone PASS (reboot=ok).
- COSMIC greeter probe (--virgl, coldstart-bench CS_STEPS=term, VNC 5947): first attempt ran under
  the same overload (display never active, serial empty); rerun alone: greeter settled 3.1 s,
  session first frame 0.65 s, terminal window opened (1.08 s).
- x86_64 release build OK; x86_64 privtest PASS (both new cases).

## Open
- None from this lane. New handler ports must call `register_user_filter` if they have a user protocol.
