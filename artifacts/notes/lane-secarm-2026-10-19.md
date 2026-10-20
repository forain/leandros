# lane/secarm — AArch64 PXN/PAN/EL0 cache ops, setsid EPERM, rlimits (L7.4, L7.6)

## L7.4 AArch64 hardening
- **PXN on every EL0 page** (`arch/aarch64/src/paging.rs` translate_flags).
  Root cause found on the way: `PageDescFlags::PXN`/`UXN` were swapped
  (PXN = bit 53, UXN = bit 54 in a stage-1 descriptor). Harmless while only
  `NO_EXEC` (both bits) was used, but setting "PXN" alone made every user
  text page EL0-non-executable: init looped on an instruction abort at its
  entry point. Fixed the constants.
- **PAN** (FEAT_PAN): `SCTLR_EL1.SPAN = 0` so every exception entry sets
  PSTATE.PAN; BSP sets it before `smp_init` snapshots SCTLR, APs set their
  own PSTATE.PAN (`smp.rs`). `sched::uaccess::__uaccess_copy` clears PAN for
  the copy and restores it; `UserAccess`/`with_user_access` is an explicit
  window; `cpu_switch_to` saves/restores PAN per task (`CpuContext.pan_clear`,
  offset 640). Kernel knob `pan=audit|strict|off`, default **audit**: a raw
  kernel access to a user address (permission fault, PAN set, far < 2^48)
  is logged once per PC (`[PAN] audit: raw user access pc=...`) and let
  through (SPSR.PAN cleared for the rest of that context); `strict` kills
  the task (SIGSEGV). Without FEAT_PAN the mode is forced off.
- **EL1 instruction abort on a user address** (PXN) kills the task with
  SIGSEGV instead of halting the machine.
- **SCTLR_EL1.UCI/UCT** (and DZE) were ALREADY set on main
  (`arch/aarch64/src/mmu.rs` ~line 94, copied to the APs); now covered by a test.
- **x86_64 SMEP/SMAP: not enabled** (no CR4.SMEP/SMAP, no stac/clac anywhere
  in `arch/x86_64`). Left open.
- Test hook: root-only `SYS_SECPROBE` (591): op0 mode, op1 raw load from a
  user page, op2 call user code at EL1, op3 PAN trap count.

## L7.6
- `setsid()` answers EPERM when a process group with the caller's id exists
  (caller is a group leader, or an unreaped member waits); sets sid/pgid on
  every thread of the process.
- NOFILE / NPROC: ALREADY enforced on main (fd tables, clone.rs).
- FSIZE: f2fs write_at, tmpfs handle_write, ftruncate, fallocate: EFBIG +
  SIGXFSZ at/after the limit, straddling write cut short.
- STACK: exec maps a main stack of the soft limit (clamped 128 KiB..4 GiB;
  8 MiB when unlimited). AS/DATA: mmap, mremap, brk (brk answers the old
  break). CPU: SIGXCPU each second past the soft limit, SIGKILL at the hard
  one, checked on return to user (one relaxed load until someone sets one).

## Tests (aarch64 HVF)
- privtest: new cases all PASS (kernel_pan_traps_raw_access,
  kernel_uaccess_under_pan_ok, el0_cache_maintenance_ok,
  kernel_pxn_refuses_user_code, setsid_*, rlimit_fsize_* tmp + f2fs cwd,
  rlimit_nofile/nproc/as/data/stack/cpu). syscalltest PASS. aarch64 regress suite: overall PASS.
- x86_64: `cargo check` of the kernel clean (not booted, per lane rule).

## Open
- PAN audit found 28 distinct raw user-access PCs in one boot + privtest run
  (1263 traps): sys_writev/readv/check_iov, sys_prlimit64, sys_ppoll,
  epoll_wait_until, sys_nanosleep, sys_clock_gettime/settime, sys_capget/
  capset, sys_getresxid, sys_prctl, sys_rt_sigqueueinfo,
  sys_sched_setscheduler, futex::wait_key, pty slave_read/slave_write/
  master_write, vfs handle_write, virtgpu_handle_getparam, std_handle_version,
  plus memcpy/memset from unknown callers. Each must move to the uaccess
  helpers (or a `UserAccess` window for in-place atomics like futex) before
  `pan=strict` can be the default. Cost meanwhile: at most one extra EL1
  fault per syscall that touches user memory raw.
- x86_64 SMEP/SMAP.
