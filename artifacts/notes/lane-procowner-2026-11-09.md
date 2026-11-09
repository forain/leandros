# lane procowner (2026-11-09)

Blocker #2 of podtrace2: newuidmap (setuid shadow) refused to write the map because
fstat on the /proc/<pid> dirfd reported st_uid 0 for a fresh CLONE_NEWUSER child.

Root cause: path stat of /proc/<pid>[/file] already used the task's euid, but the fd
opened by handle_open is a tmp-pool snapshot whose uid/gid stayed 0, so fstat (the
dirfd and files like uid_map opened through it) said root. Repro: idmaptest
proc_owner_newuser failed on main with `fstat fd=7 uid=0`, rc=4 (path stat was fine).

Fix:
- sched::cred::proc_dump_owner(pid): Linux task_dump_owner. Effective kuid/kgid; a
  non-dumpable task shows its user namespace's root (kuid 0 when unmapped). Kernel
  ids: translation into the reader's view stays on the copy to user memory (idstat).
- servers/vfs: path stat uses it; handle_open stamps the snapshot's owner with it
  for every /proc/<pid>/... open, so fstat agrees with stat.
- Dumpable: setuid/setgid changes and set-ID exec clear it (already so); idmaptest
  proc_pid_owner / setuid_maps_down now call prctl(PR_SET_DUMPABLE,1) after the id
  change, as on Linux where /proc/<pid> would otherwise be root.

Tests (userland/idmaptest): proc_owner_newuser (stat, dirfd fstat, openat uid_map),
proc_owner_nondumpable, newuidmap_e2e (needs idmap=test: uid 1000 runs
`newuidmap <pid> 0 1000 1 1 100000 65536`, uid_map shows the extents). All existing
cases kept.
