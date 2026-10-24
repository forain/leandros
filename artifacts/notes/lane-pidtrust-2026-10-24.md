# lane/pidtrust (L1.17) - 2026-10-24 - FIXED

## Bug
evdev (servers/evdev/src/lib.rs) and drm (servers/drm/src/lib.rs) trusted a victim PID carried in IPC
slot 3 and wrote results into that task's address space (copy_to_task). Both ports were registered with
`register_handler`, so any unprivileged process could LOS_IPC_SEND raw messages to them and make the
server write into an arbitrary process (evdev EVIOCGNAME, drm VFS_READ).

## Fix (cdeeceed)
Both servers now use `register_kernel_handler`. Their ports are reached only through
vfs_server::register_device forwards and in-kernel calls; ports are not in auxv, there is no lookup
syscall and no userland sender. `ipc_send_permitted` returns EPERM for user sys_send/sys_call, while the
in-kernel port::send path bypasses it (same mechanism as copyraw's VFS/f2fs ports).

## Audit
Grep over servers/ for pid-from-slot / copy_to_task: only drm and evdev (+client_key). The nouveau/nvidia
and i915 gpu_uapi backends take no pid and use current-AS uaccess on the VFS-forwarded arg; unforgeable now
that drm is kernel-only. Safe.

## Evidence
- Guard 6bcfc58c: privtest `los_ipc_evdev_foreign_pid_untouched` forks a victim (buf 0xAA) and an attacker
  sending evdev EVIOCGNAME and drm VFS_READ with the victim pid to ports 1..1023.
  RED on unfixed code: "FAIL (victim buffer written via raw IPC)" (victim got "QEMU Virtio Keyboard").
  GREEN after fix: /bin/privtest 1/1 PASS on aarch64 and x86_64 (after merging main with copyraw).
- evgkeytest, regress and greeter probe (session /run/user/1000, WAYLAND_DISPLAY=wayland-1) on the merged tree:
  see merge message / lane report. Known flake: sshsmoke ssh_hostkey.

## Open (follow-up)
pipewire (servers/pipewire/src/lib.rs:354, port in auxv 258) and netctl (servers/net/src/netctl/mod.rs:694)
are VFS-forwarded device servers that trust open_id (slot 4) from raw IPC: identity forgery (e.g. pipewire
HOLD_OPEN / holder-only SET_PARAMS), no memory access. pipewire's port is in auxv so it may have a real
userland sender (get_audio_port); needs sender-identity stamping rather than kernel-only.
