# lane/mntpivot 2026-11-10 (podtrace5 blocker: `crun: pivot_root: Invalid argument`)

FIXED (pivot_root inside a mount namespace); podman result: see "after mntpivot" in podtrace5-findings.md.

- pivot_root in a mntns != 0 used to return EINVAL by design. Now `sys_pivot_root_ns()` (kernel/src/syscall.rs; ns != 0 only, `may_mount()` over the owner userns,
  refused for a chrooted task) calls `mnt::pivot_root()` (servers/vfs/src/mnt.rs): re-parents the old root mount under put_old, makes new_root the tree root,
  transfers MNT_LOCKED from the old root to the new one, and returns a `PivotMap`. `sched::rebase_fs_paths` then rewrites the string root/cwd of every task
  sharing that fs state. `umount("/")` is accepted for the root overmount stack, so crun's `umount2(".", MNT_DETACH)` after `pivot_root(".", ".")` works.
- Linux checks mirrored: new_root must be a mount point (EINVAL), put_old inside new_root (EINVAL), root as new_root (EBUSY), shared put_old / new_root parent (EINVAL), locked new root (EINVAL).
- Tests (mnttest pivot_suite): pivot_dotdot (crun sequence: chdir(root); pivot_root(".","."); fchdir(oldroot); rslave; umount2(".",MNT_DETACH); chdir("/"); getcwd()=="/"; old root invisible),
  pivot_errs, pivot_locked_userns (uid 1000, userns+mntns), pivot_ns0_intact.
- Fail-first (box aarch64, tests-only commit 7817c633): pivot_dotdot, pivot_errs, pivot_locked_userns FAIL; with the fix all pass (commit a00db272 + merge).
- Test fixes needed after first compile: pivot_errs made the new root mount itself shared (parent "/" was not its parent: /tmp is a separate mount); pivot_locked_userns root moved to /tmp/pvu3 (uid 1000 could not mkdir under root-owned /tmp/pv).

## Caveat
Propagation rslave on "." (`mount("", ".", MS_SLAVE|MS_REC)`) matches by mount ns path: after the pivot the old root hangs at "/" in string terms
(PivotMap rewrites paths), so rslave on "." hits EVERY mount whose ns path is "/" prefix-wise, i.e. the whole recursive tree incl. the new root, not just the detached old root.
Harmless for crun (it detaches right after), but a program applying rslave to "." while both trees exist would change the new tree's propagation too.

## Left open
- Not verified against real Linux for exotic cases (put_old == new_root with other mounts stacked; threads with CLONE_FS split cwd).
