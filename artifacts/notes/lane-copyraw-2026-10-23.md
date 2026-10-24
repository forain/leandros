# lane/copyraw (L1.16) - copy_raw reachable from user-controlled pointers

Status: FIXED (evdev/drm pid-trust finding left open, see below).

## Sites
copy_raw sites inside vfs/f2fs/net serve kernel callers. The holes were at entry points:
- Raw IPC to the VFS/f2fs ports: now kernel-only, EPERM for user senders.
- sendmsg/recvmsg iov_base and msg_control (servers/net/src/lib.rs, 10 inserts): now user_range_ok, EFAULT.
- DRM nested pointers (syncobj handles, GETPARAM): uread_user/uwrite_user (EFAULT). nvsmap's ucp_in/ucp_out cover
  GETRESOURCES/modes/props/version strings. uread/uwrite/user_struct_in/out (copy_raw) are used only on the top-level
  `arg`, which may be the kernel bounce copy, so they stay unchecked.
- tty/pty audited OK. pty TIOCSIG reads *arg as a pointer (Linux takes a value): semantic bug, not fixed here.

## Guards (red on main, green on lane)
- privtest los_ipc_call_vfs_port_eperm / los_ipc_send_vfs_port_eperm: main "got 0, want EPERM", lane PASS.
  los_ipc_send_reply_to_root_port_eperm retargeted to the audio port (auxv 258).
- syscalltest kaddr_drm_syncobj_handles, kaddr_sendmsg_iov, kaddr_sendmsg_control, kaddr_recvmsg_iov:
  main = kernel crash "EL1 Sync Fault FAR=FFFF800000000000"; lane PASS. The sendmsg/recvmsg probes use raw
  syscall() because relibc gathers iovs in user space (SIGSEGV instead of EFAULT; Linux-divergent, not a security bug).

## Final verification (copyraw3, merged with main)
- aarch64 + x86_64 release build OK.
- COSMIC greeter login under --virgl aarch64 (coldstart-bench, CS_STEPS=term): session reached, terminal window opened.
- x86_64 syscalltest and killmt 40 proclife: RC=0 (earlier killmt red did not repeat).
- aarch64 full regress green except sshsmoke ssh_hostkey (known flake; ssh_password_exec flaked once, passed on rerun).

## Audit of non-kernel-only handler ports
- drm: SAFE after this lane + nvsmap; nouveau gpu_uapi backend (backend_takes_user_arg) not audited.
- evdev (servers/evdev/src/lib.rs:1855): copy_to_task/copy_from_task safe for kernel R/W, BUT the target pid is taken
  from msg slot 3, so a raw-IPC sender can read/write another process's user memory. Same in drm VFS_READ
  (lib.rs:413, pid from arg3). OPEN, new finding.
- netctl: SAFE (read_user_buf ignores PROT_READ, minor). pipewire: SAFE.
- Port ids are small integers from port::create (guessable); drm/evdev/netctl are not in auxv.
