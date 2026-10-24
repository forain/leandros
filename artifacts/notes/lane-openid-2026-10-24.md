# lane/openid — forged open_id via raw IPC (openbugs L1.18) — 2026-10-24

**FIXED.**

## Bug
The audio server (servers/pipewire, port published in auxv 258) and netctl
(servers/net/src/netctl) are VFS-forwarded device servers. VFS fills the
open's `open_id` into the message (slot 4 / slot 1 for VFS_CLOSE), but the
handler port accepted the same tags from a user `LOS_IPC_CALL`, so any process
could name another process's open_id: release the audio holder's claim
(VFS_CLOSE 0x13), pass holder-only SET_PARAMS (0x28/0x101), or act on another
open's netctl snapshot. `caller_pid` cannot tell the cases apart: both the
user syscall and the in-kernel VFS forward run in the caller's task.

## Fix (6b7dd5b3)
- `ipc::port::register_user_filter(port, fn(&Message) -> bool)` + `user_send_allowed`;
  `sys_send`/`sys_call` consult it after `ipc_send_permitted` and return EPERM.
  In-kernel `port::send` (VFS forwards, replies) is never filtered.
- Audio server: user code may send only 0x100/0x200 (legacy protocol, arbitrated
  by the kernel-supplied caller pid), 0x300 PUMP, 0x1000 PING. Legacy senders
  checked: aplay (0x100, 0x200), privtest.
- netctl: no user protocol (port not in auxv; NetworkManager uses /dev/netctl
  through VFS) -> filter refuses everything. Equivalent to copyraw's
  `register_kernel_handler`; can switch to it once that lands.

## Guard (63a404f6) — privtest `root_audio_forged_open_cases`
Root holds /dev/pipewire O_WRONLY; a child dropped to nobody sends, for
open_id 1..1024, raw VFS_IOCTL SET_PARAMS and VFS_CLOSE to auxv 258.
- RED (unfixed, tests-openid-red): `audio_forged_open_id_holder_kept: FAIL (got 0, want EBUSY)`
  — the forged VFS_CLOSE freed the device, a second writer got it.
- GREEN (tests-openid-green): both cases PASS.

## Verification (aarch64 release, HVF)
- privtest 0 fails; full regress (36 suites incl. nettest, netctltest) overall PASS.
- sndtest PASS; `root_audio_ipc_while_held_ebusy` (legacy 0x100 path) still EBUSY, not EPERM.
- x86_64 release build complete (not booted, per lane rule).

## Open
- Other handler ports that take VFS tags (any remaining `register_handler` server
  with an open_id/pid slot) should get a filter or `register_kernel_handler`.
