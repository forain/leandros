# lane/nsproc (2026-11-08) - DELIVERED (L2)

- sched/src/ns.rs: identity-only uid/gid maps (5 extents/ns, atomics), setgroups deny flag, write_map/write_setgroups with
  Linux rules (write once, EPERM; writer in ns or parent; unprivileged = one line "X X 1" of own euid/egid, gid_map needs
  setgroups=deny first; privileged = writer in parent with CAP_SETUID/SETGID, extents inside parent's map; non-identity -> EPERM).
- Views: getuid/geteuid/getgid/getegid/getresuid/getresgid map kernel ids through the caller's userns (unmapped -> 65534).
  Credentials stay host ids in Task. stat st_uid, /proc status, SO_PEERCRED NOT translated (L2b, not needed by Firefox).
- sys_setgroups -> EPERM once the caller's userns has setgroups=deny.
- VFS: /proc/<pid>/{uid_map,gid_map,setgroups} read + write (fake writable path /tmp/.proc_nsmap_<kind><tgid>);
  /proc/<pid>/ns/{user,pid,net,ipc,mnt,uts,cgroup,pid_for_children}: readlink "user:[ino]" (kernel), lstat S_IFLNK,
  stat/open/access -> 0444 regular file with st_ino. Initial ids use Linux constants (user 4026531837, pid ...836, net ...840,
  ipc ...839, mnt ...841, uts ...838, cgroup ...835); user ns id N: 0xF0000000+4N. Only the user ns is per-task today; others report initial.
  "ns" dir entry added to /proc/<pid> listing.
- /proc/<pid>/root of a void-rooted task read from outside = "/.leandros-void" (not "/"); the task itself sees "/".
- nstest: map_firefox_seq, ns_links, void_root_link (+ old cases adjusted: unmapped euid is 65534). nstest 31/0, captest, chroottest pass (aarch64).
- Not shown failing-before on unfixed code (new cases need the new /proc files; main nstest has none of them).
- Open: L2b translation of stat/status/peercred; setuid inside ns mapped-target checks.
