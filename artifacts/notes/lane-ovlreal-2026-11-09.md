# Lane ovlreal (O1) - 2026-11-09

Status: DELIVERED (aarch64; x86_64 left to the merge-batch run).

## What landed
- `servers/vfs/src/ovl/{mod,real}.rs`: real-layer helpers on kernel buffers
  (lstat, readlink, open/close, pread/pwrite, getdents, mkdir, mknod, symlink,
  link, unlink, rmdir, rename(noreplace), chmod, lchown, utimens, statfs,
  l{get,set,list,remove}xattr). Independent of the mount table (a mount is a u32 id).
- Per-mount pseudo-pid `OVL_PID_BASE(0x7000_0000) | id` with the mounter's `xattr::Cred`
  (`ovl::register/unregister`, `cred_of()` consults `ovl::cred_override` first;
  one relaxed atomic load when no overlay exists).
- `handle_as()` split out of `handle()`: runs a message for a canonical pid without `tgid_of`.
- In-guest selftest `/proc/sys/fs/ovl_selftest` (root only; prints FAIL lines + `RESULT pass= fail=`).
- `userland/ovltest/ovltest.c` (suite `ovlreal`, in the default regress): kernel selftest + the
  user-visible xattr ENODATA audit, on tmpfs (/tmp) and f2fs (/home).

## Findings
- f2fs is called through its IPC port with `sched::current_pid()` as the caller, NOT the pid the
  VFS handler runs as. A pseudo-pid alone therefore does not carry creds to f2fs (first run: an
  unprivileged "mounter" could create files in a root 0755 dir on f2fs). Fixed with `ovl::enter()`:
  a RAII window mapping the current thread to the mounter cred for the duration of a real op.
- VFS_PREAD/VFS_PWRITE serve block devices only (ERR_NOT_POSITIONAL otherwise): real pread/pwrite
  are lseek+read/write on the pseudo-pid's private fd.
- xattr ENODATA audit: NO defect found. get/list/remove of absent user.* (and security.*,
  trusted.overlay.*) names on files and dirs return ENODATA / empty list on tmpfs and f2fs for the
  path, l and f variants; round trips fine. No xattr/f2fs code changed.
- "Fails on main first": on main the selftest file does not exist (kernel_selftest case FAILs on open);
  the xattr audit cases pass on main by construction (no code change), they are regression guards.
  First run with pwrite/cred issues showed the f2fs cred gap above (cred-eacces, cred-owner FAIL).

## Evidence
aarch64 ovltest: 85 passed, 0 failed (kernel selftest 94 checks).
Tag collision note: runtests tag `ovl` is also used by lane ovlmknod; use unique tags.

## Open
- ofd per-slot `ovl` flag + release hook (design O1 table) not done here: no consumer exists until
  the overlay walker (O2) lands; left for that lane.
- mknod helper passes only the packed mode (VFS_MKNOD ABI); ovlmknod may extend it with a dev_t.
