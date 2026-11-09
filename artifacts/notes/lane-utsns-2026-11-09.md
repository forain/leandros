# lane/utsns 2026-11-09 - UTS namespaces: FIXED

- CLONE_NEWUTS now accepted by clone/clone3/unshare (NEWNS/NEWCGROUP still EINVAL).
- sched/src/ns.rs: per-ns hostname/domainname table (copied from the creating ns), owner userns, `uts_admin()`
  (CAP_SYS_ADMIN in the owning userns or an ancestor), `unshare_uts`, `new_uts`; `Task.utsns` inherited on fork/clone.
- uname/sethostname/setdomainname use the caller's ns (global UTS_NAMES removed); EPERM via uts_admin (was euid==0).
- /proc/sys/kernel/{hostname,domainname} generated per requesting pid; /proc/<pid>/ns/uts reports the per-ns inode.
  /proc/<pid>/status has no UTS field on Linux, so none added.
- Tests (userland/nstest): uts_unpriv, uts_copy_inherit, uts_host_userns_eperm; NEWUTS removed from the EINVAL tables.
  Before fix (tests commit only): 72 passed, 3 failed (all uts_*). After: 75 passed, 0 failed (aarch64).
