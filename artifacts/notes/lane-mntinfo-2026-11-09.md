# lane/mntinfo (M3), 2026-11-09

Status: DELIVERED (see evidence).

- `servers/vfs/src/mnt.rs`: propagation state (MS_SHARED/PRIVATE/SLAVE/UNBINDABLE, +MS_REC), `set_propagation`, per-ns tree renderer `render_tree` (mountinfo and mounts), ns-0 propagation table `PROP0` (`/` defaults to `shared:1`; re-sharing `/` returns to group 1).
- `servers/vfs/src/lib.rs`: `/proc/<pid>/mountinfo`, `/proc/<pid>/mounts`, `/proc/self/*`, `/proc/mounts` use the target task's mount ns; ns-0 lines carry `shared:N`/`master:N`/`unbindable`; parent ids now point at the root line (id 20) instead of a constant 1. `ns0_snapshot` copies propagation into new namespaces.
- `kernel/src/syscall.rs`: `sys_mount` propagation branch; MS_PRIVATE etc. on a non-mountpoint gives EINVAL; several propagation bits gives EINVAL.
- Deviation: other ns-0 mounts are private (no field) unless set; Linux makes children of a shared mount shared. Event replay between peers is M10.
- Tests: userland/mnttest (11 new cases, all existing kept). Mac aarch64 HVF mnttest: all PASS.
- Box farm was tmpfs-full on the first attempt (ENOSPC, 0 suites), so the "fails on main" run is by construction (old code printed zero optional fields and routed MS_PRIVATE to the mount path).
