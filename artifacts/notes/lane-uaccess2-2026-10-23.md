# lane/uaccess2 (2026-10-23) — L5.11 remainder FIXED

## getdents64 (kernel/src/syscall.rs sys_getdents64)
The VFS advances the directory cursor while filling the kernel bounce buffer, so a faulting
copy-out lost the entries. Fix: before calling the VFS, probe every destination page
(read a byte, store it back through copy_from_user/copy_to_user); a bad page returns EFAULT
before any cursor moves. Test: vfstest `getdents_efault` (20 files, destination starts 64 bytes
before a PROT_NONE page so the dirents spill into it; expects EFAULT or a short count, then a
retry with a good buffer must deliver all 22 entries). FAILED on the unfixed kernel, PASSES fixed.
(First attempt with an unmapped page passed on the old code: validate_user_buf already rejects
that; only a mapped-but-unwritable page reproduces the loss.)

## DRM
- servers/drm: render_node_version (drm_version struct + name/date/desc buffers) and VFS_WRITE
  now bounce through kernel memory.
- drivers/src/drm_device_interface.rs: the ~100 raw accesses were the real exposure (24 [PAN]
  audit hits in `--suite drm --gpu virgl` before the fix). `handle_ioctl` is now a wrapper that
  bounces the top-level argument struct (size/direction from the _IOC encoding of cmd) through
  kernel memory and copies it back out whatever the handler returned (as Linux); the old body
  is `handle_ioctl_inner`. Pointers nested in the struct (property/value arrays, blobs, plane
  and format lists, syncobj handle/point arrays, execbuffer bo_handles/command, get_caps,
  getparam value, context_init params, timeline points) go through uread/ucp_in/ucp_out.
  Ioctls with a zero _IOC size/dir are passed through unchanged (none hit in the suite).
## evdev
Audited: every user access already goes through sched::copy_to_task/copy_from_task/zero_task/
write_user_buf; the from_raw_parts hits are kernel arrays. No change.
## f2fs (found by the drm audit)
read_file_data_planned's cached-block and sparse paths wrote the user buffer with
copy_nonoverlapping/write_bytes (1 [PAN] hit, drmsmoke loading a file); now copy_raw.

## Evidence (aarch64, HVF)
pan=audit: drm suite (virgl) 24 -> 1 -> 0 [PAN] lines; regress + privtest 0 [PAN] lines, PASS.
Strict default: see delivery report.
