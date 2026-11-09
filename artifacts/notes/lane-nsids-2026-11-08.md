# lane/nsids (2026-11-08) - DELIVERED

Security guard (run on unfixed main first): inside an unprivileged clone(NEWUSER) child, with and without a
written map, setuid/setreuid/setresuid/setgid/setregid/setresgid/setgroups to other ids (0, 1001) all FAILED and
getuid() stayed unchanged -> case `setid_guard` PASSED on main: NO privilege escalation for an unprivileged creator.
Gap found on main (case `root_einval`, failing before): privilege was `euid == 0` (host), so a host-root task in a
userns with a map of only "0 0 1" could setuid()/setgid() to ids unmapped in its namespace (no EINVAL).
It is host root anyway, but violates the namespace contract and would have become wrong once maps broaden.

Changes
- sched/lib.rs `setid_priv(t, gid)`: initial ns = euid 0 (unchanged); user ns = CAP_SETUID/CAP_SETGID held there.
  Used by setuid/setgid/setresuid/setresgid/setreuid/setregid/setfsuid/setfsgid/setgroups.
- sched/ns.rs `ids_mapped(kind, ids)`; syscall dispatch returns EINVAL when any (non -1) target id of
  setuid/setgid/setres*/setre* and chown/fchown/fchownat/lchown is unmapped in the caller's userns.
  setfsuid/setfsgid ignore unmapped ids (Linux: no change); setgroups list must be mapped (else EPERM).
- Host->ns view (unmapped -> 65534) via `ns::view_id`: stat/fstat/lstat/statx st_uid/st_gid (vfs write_stat_full_rdev),
  /proc/<pid>/status Uid:/Gid: (reader's ns), SO_PEERCRED uid/gid (net), siginfo si_uid (kill/SI_QUEUE paths).
- Maps are identity-only, so ns->host translation = identity + mapped check.
- Not translated: SI_TIMER/seccomp si_uid fields, /proc/<pid>/stat(m) owner, SCM_CREDENTIALS, SysV ipc perms.

Tests (userland/nstest): setid_guard, root_einval, id_translate. Before: root_einval and id_translate FAIL
(stat returned 1000 instead of 65534; setuid(1000) succeeded). After: nstest 30/0, captest, chroottest pass (aarch64).
