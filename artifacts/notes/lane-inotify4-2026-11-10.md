# lane/inotify4 (L4) 2026-11-10

Status: DELIVERED (L4 of design-inotify.md).

- Per-(userns, kuid) ucounts chain for inotify instances/watches/queued events, dynamic limits, `/proc/sys/fs/inotify/{max_user_instances,max_user_watches,max_queued_events}` (root-writable, EINVAL on 0).
- inotifytest: 13 new cases (limits, overflow, proc limits, userns chain, bind mount inside userns+mntns, IN_ONESHOT/MASK_ADD/MASK_CREATE/ONLYDIR/EXCL_UNLINK/DONT_FOLLOW).
- Fail-first: tests-only commit failed 7 cases with vfs.inotify=1. With the implementation: 27 passed, 0 failed (aarch64, vfs.inotify=1).
- Test fix found while running: idmap rules refuse kuid 0 maps (rule 6) and non-self writers, so the userns cases run as kuid/kgid 1000 and each child writes its own identity map (setgroups deny, uid_map, gid_map).
- Default knob 0: full aarch64 box regress, see merge message.
- Open: stat of /proc/sys/fs/inotify/* has no RamEntry rows (open works).
