# lane/setns 2026-11-09 — DELIVERED

setns(2) (x86_64 308, aarch64 268) and openable /proc/<pid>/ns/* files.

- `VnodeKind::NsFd {kind,id}`: each fd copy holds one NsRef (pipe_ref_inc/dec). fstat ino = readlink inode (pid and pid_for_children now share an inode); st_dev constant.
- setns(nsfd, nstype) for user/net/ipc/pid(_for_children): Linux rules (userns: same-ns/threaded EINVAL, chrooted EPERM, CAP_SYS_ADMIN in target or creator-owner; net/ipc/pid: CAP in owning userns + caller's; pid must be descendant). Joining a userns grants full caps. nstype 0 = any; mismatch EINVAL.
- setns(pidfd, CLONE_NEW* mask): user first, later entries checked with its caps; mnt/cgroup/uts EINVAL.
- net/ipc/pid namespaces now record their owning userns (OWNER_UNS, counted ref).
- ioctls NS_GET_USERNS / PARENT / NSTYPE / OWNER_UID on ns fds.
- Evidence: nstest 12 new cases failed on main (75/12), 87/0 after; default aarch64 regress PASS.
- UTS and cgroup joins supported. Open: mnt (EINVAL: needs root/cwd reset); bind-mounting an ns fd path to keep a namespace alive; no ptrace-style permission check on open of other users' ns files; NS_GET_* skip Linux's reachability checks; joined net ns has down loopback (existing design).
- nstest: cgns_in_userns fails identically without this lane (see lane-idroot-2026-11-09.md); everything else 98/1 -> setns cases all pass.
