# lane/opentree 2026-11-10 - FIXED

open_tree(OPEN_TREE_CLONE), move_mount and mount_setattr over the per-namespace mount tree. A detached tree is an anonymous mntns held by an NsFd (kind 4).

Security: clone and move_mount need may_mount(); clones keep MNT_LOCKED children; non-recursive clone that would drop locked children is EINVAL; setattr cannot clear a locked flag (EPERM); /.leandros-sb is not clonable; ns 0 unchanged.

Evidence: mnttest opentree_suite (7 cases) FAILED 7/7 on ff-opentree (377318e1, tests only); PASS on the lane (aarch64 box). Note: per-mount RDONLY is bookkeeping only in this VFS (not enforced on file ops), so the setattr case checks mountinfo ro/rw. Full aarch64 box regress: 80 suites, 0 bad.
