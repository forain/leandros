# lane/nvsmap (2026-10-23) - DELIVERED (NVIDIA PC verification PENDING)

## Changes (1f808f68)
- NVIDIA UAPI: every user access via sched::uaccess (read_user/write_user/copy_from_user, EFAULT on fault); BACKEND user_args on.
- DRM: LeandrOS-private ioctls 0x1001..0x100A are bounced via legacy_arg_size() (previously dir/size=0 skipped the bounce and
  handlers dereferenced user memory: SMAP / "[PAN] raw kernel access to user memory").
- Nested user pointers (version strings, resource id arrays, connector encoders/modes, property arrays) use range-checked copy_from_user/copy_to_user.

## Evidence
- main (c5d9e805+): aarch64 `/bin/drmsmoke` RC=139 (PAN) -> RED. Branch: RC=0 fails=0 -> GREEN.
- Branch merged with main: aarch64 full regress PASS; x86_64 `--suite i915fake` PASS; x86_64 drmsmoke PASS.
- Earlier "overall: FAIL" runs were nettest/netctltest (aarch64) and nettag2test (x86, known flake); both net suites pass on re-run; not DRM related.

## Open
- NVIDIA UAPI paths only execute on the RTX 3070 (no fake GSP mode): PC verification pending.
- v3d arms: nested bo_handles via syncobj_read_handles not range-audited.
- tty/pty ioctl bounce table in kernel/src/syscall.rs not verified complete.
- vfs/net/f2fs copy_raw uses (27+5+6) not range-audited.
