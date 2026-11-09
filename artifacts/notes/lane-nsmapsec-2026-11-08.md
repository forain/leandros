# lane/nsmapsec 2026-11-08 (ffsbreview1 F1, F2, F5, F6)

- F1 FIXED: map files snapshot the opener's identity at open (sched::ns::Opener, vfs NSMAP_OPENERS by tmp slot). Open is EPERM unless the opener may write that ns's maps; write needs opener AND writer permitted, privileged branch only if both privileged, unprivileged branch needs the same id on both.
- F2 FIXED: write_map/write_setgroups run under one spinlock (MAP_LOCK): check and store are atomic w.r.t. other map writers.
- F5 FIXED: unshare and clone(3) with CLONE_NEWUSER return EPERM when chrooted.
- F6 REFUTED on main: fdinfo already uses proc_environ_allowed (861d66e9); guard test added, passes before and after.
- Evidence (nstest aarch64): unfixed main -> map_deputy FAIL (helper wrote "0 0 1", child setuid(0) succeeded), map_race FAIL (both writers succeeded, iter 10), chroot_newuser_eperm FAIL; fixed -> 56/56 pass.
