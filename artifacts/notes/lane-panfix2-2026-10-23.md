# lane/panfix2 (panfix3 session, 2026-10-23) — L7.4 FIXED, L5.11 PARTIAL

Commit a9b65c92 (squash of the panfix/panfix2 WIP): strict FEAT_PAN default, raw user accesses converted.

## Root causes of the two strict-mode failures left by the previous session
- privtest SIGSEGV (EL1 fault in memcpy, FAR on user stack): `sys_setxname` copied the hostname with
  `copy_nonoverlapping` from the user pointer. Found with `pan=audit` + llvm-symbolizer (lr = sys_setxname).
- chroottest "Not a directory": not a regex slip; it was fallout of privtest dying mid-run under strict.
  It passes with privtest fixed.
- The regex conversion DID slip in sys_statx and sys_sysinfo: both build into a kernel buffer, and
  write_user() on a kernel address fails user_range_ok and drops the store. Symptoms: syscalltest
  statx_matches_stat (uid/gid 0 vs 1234/5678), timertest times_and_loadavg. Restored to kernel stores.
  All other converted call sites were checked: their base pointers are syscall arguments.

## Also converted
blkdev_info/blkdev_name/mounts_info copy-outs; syncfile_ioctl.rs (sync-file info, dma-buf sync) fully on uaccess.
audit mode no longer logs the deliberate privtest probe (sys_secprobe op 1).

## Evidence (aarch64, HVF)
- pan=audit, full regress: 0 `[PAN]` lines (acceptance). privtest+chroottest under audit: 0.
- strict (default), full regress: green except evherdtest (passed on rerun, flake).
- strict: /bin/privtest as root and `su leandro -c /bin/privtest` PASS; chroottest 23/0/0.

## Open (L5.11 remainder, handoff uaccess2.md)
- servers/drm and servers/evdev own arg copies (raw from_raw_parts/unaligned on ioctl args) not audited.
- getdents64 cursor advanced before a faulting copy-out (sys_getdents64 copy_to_user at the end).
- x86 SMEP/SMAP not enabled (nothing in tree).
