# lane/panfix 2026-10-20 — PAN: raw user accesses moved to uaccess

## What
Under the default `pan=audit` (aarch64), every kernel load/store to a user address outside
the uaccess window logs `[PAN] audit: raw user access pc=.. lr=.. far=.. R|W`. This lane moves
those sites to `sched::uaccess` (copy_from_user/copy_to_user/read_user/write_user/copy_raw,
cmpxchg/load helpers), so `pan=strict` can become the default.

Converted:
- kernel/src/syscall.rs: poll/select/ppoll, iov, timespec, prlimit, cap*, getres*id, prctl,
  sigqueueinfo, sched_*, futex (wait/requeue value checks, robust list), pty bounce,
  gettimeofday, get/set_mempolicy nodemask, rt_sigsuspend/sigtimedwait, signalfd4,
  timerfd_settime, getrandom; statx/sysinfo/uname/sched_getaffinity build the result in a
  kernel buffer and copy out once; fstat_into/fstatat_into with a user destination run the
  kernel-buffer path and copy out once.
- servers/vfs: handle_read device arms (random, tty0, fb, eventfd, timerfd, signalfd),
  handle_write (/proc comm, eventfd), stat/statfs helpers (kernel buffer + one copy_raw,
  also used by f2fs stat).
- drivers/src/drm_device_interface.rs: `with_user_struct` / `uread` / `uwrite` helpers; every
  `&mut *(arg as *mut T)`, `read_unaligned(arg)`/`write_unaligned(arg)` handler and the
  id-array / modeinfo copies (version, getparam, get_resources, get_connector, encoder, crtc,
  create/map/destroy dumb, addfb/addfb2, set_crtc, page_flip, dirtyfb).

## Evidence
- Audit sites in one regress boot: 43 unique (round 1) -> 12 unique.
- aarch64 regress under pan=audit: `boots failed: 0/1; overall: PASS`.

## OPEN (12 sites left, regress boot, symbolized with scratchpad sym.py)
- syscall.rs: dispatch+0x9f0, epoll_wait_until+0x2868/+0x2990, sys_get_mempolicy+0xc8
  (nmask write path), sys_select+0xb8 (R).
- f2fs_server::write_file_data+0xa34/+0xa74 (memcpy from user write buffer).
- vfs_server: block::read_at+0x350, handle_read+0x1f8, handle_readlink+0x154,
  handle_statfs+0x198, handle_timerfd_gettime+0x26c.
- Then: `pan=strict` default (kernel/src/main.rs `_ => PAN_STRICT`), regress + drmsmoke
  under strict, privtest (guest `/bin/privtest` as root via driver.py run; not a runtests suite).
- x86 SMEP/SMAP: not started.
- Default stays `pan=audit`; this branch is safe to merge as is.
