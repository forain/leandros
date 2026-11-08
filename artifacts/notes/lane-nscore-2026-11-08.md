# lane/nscore 2026-11-08 (nsbox L1)

Status: user namespaces, CLONE_NEWUSER only.

- `sched/src/ns.rs`: lock-free table (256 ids, 0 = initial, uncounted). `NsRef` (Clone takes a ref, Drop puts; parent chain released on free), `new_userns(parent, euid, egid)`, `level/parent/owner`, `current_userns()`, `userns_of(pid)`, `unshare_user()`, `live_count()`, `SUPPORTED`.
- `Task.userns: NsRef`; `ForkOpts.userns` (pre-created id); fork and thread clone take their own ref, task reap drops it (no manual put on failure paths).
- clone/clone3 (`do_clone`): other CLONE_NEW* stay EINVAL; NEWUSER with THREAD/FS/VM is EINVAL; depth > 32 is EUSERS; table full ENOSPC. Child gets caps [ALL, ALL, 0, bnd].
- unshare(NEWUSER): single-threaded callers only (EINVAL otherwise), implies unshare_fs, full caps.
- caps: `ns_capable(cap)` (own userns), `host_capable(cap)` (initial userns only), `capable` = ns_capable. chroot is the only converted site; all euid==0 sites untouched (credentials are host ids).
- exec_setid: set-id bits ignored when userns != 0.
- Not done: uid_map/gid_map (nsproc); Linux "chrooted caller cannot NEWUSER" rule.
- Tests: userland/nstest, 9 userns cases (unpriv clone, ns chroot, host-root guards, unshare, threaded EINVAL, other NEW* EINVAL, depth 32, 1200x leak vs 255-slot table, suid ignored).
