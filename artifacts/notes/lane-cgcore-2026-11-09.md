# lane/cgcore 2026-11-09 (design-cgroup CG0 + CG1)

Status: FIXED/DELIVERED for CG0 (cgns accept) and CG1 (core). No cgroupfs yet (CG2).

## What changed
- `sched/src/cgroup.rs` (new): 1024-node table, atomics for hot state (parent, depth, hierarchical NR_TASKS,
  DIRECT, subtree_control, pids.max/peak/events, max.depth/descendants, dead cpu ns), one `Mutex<CgTree>` for
  names/owners (lock order CgTree -> RUN_QUEUE). API: `create/remove/lookup_child/children/name/dir_owner`,
  `set_subtree_control` (ENOENT / EBUSY rules incl. no-internal-process), `migrate_tgid(tgid, dst, scope)`
  (scope = cgns root, ENOENT outside; EBUSY when dst has subtree_control, root exempt; EAGAIN on pids.max),
  `common_ancestor/in_subtree` (for the DAC check in CG2), `path_rel(cg, root)` (with `/..`), `resolve`,
  `charge/uncharge` (RunQueue::enqueue/remove, zombies count until reap, CPU folded into the node),
  `fork_check` (called by `fork_admission`: EAGAIN + pids.events max), `subtree_tgids/direct_tgids`,
  `dead_cpu_ns`, `ns_pin/ns_unpin`.
- `Task` gets `cgroup: CgId` and `cgns: NsRef`; both inherited at clone (two sites in clone.rs); `ForkOpts.cgns`.
- `sched/src/ns.rs`: cgroup namespace = flat id + root cgroup + owner userns (held as PARENT); pins its root
  against rmdir; `new_cgns`, `unshare_cgroup`, `cgns_of/current_cgns/cgns_root/cgns_owner/is_cgns`,
  `ns_id_of` kind 6 -> `/proc/<pid>/ns/cgroup` is per-task.
- `kernel/src/syscall.rs`: unshare and clone/clone3 accept CLONE_NEWCGROUP (CAP_SYS_ADMIN in the caller's userns,
  or implied by NEWUSER; EINVAL with CLONE_THREAD/CLONE_VM for clone). CLONE_NEWNS/NEWUTS untouched.
- Hook for lane `setns`: `sched::ns::setns_cgroup(id) -> isize` (swaps the caller's cgns, -22 if `id` is not a
  cgroup ns); `ns::is_cgns(id)`, `ns::cgns_owner(id)` for the CAP_SYS_ADMIN-in-owner-userns check. Membership is
  unchanged by setns.
- Tests: new `userland/cgtest/cgtest.c` (10 cases, registered in build-userland.sh, mkfs-f2fs-populated.py,
  runtests.py default list + `cgroup` suite); nstest: NEWCGROUP removed from the EINVAL lists, 5 new `cgns_*` cases.

## Evidence (aarch64, release, Mac HVF)
- `/bin/cgtest` 10 passed, 0 failed; `/bin/nstest` 77 passed, 0 failed (incl. cgns_unshare/clone/eperm/in_userns/leak).
- Before: main rejects NEWCGROUP with EINVAL in both unshare and clone (the old nstest asserted exactly that), so
  the new cases cannot pass there. Not re-run on a main-built kernel (budget).

## Open / for CG2
- pids.max is checked in `fork_admission` and charged in `enqueue`; two forks racing between them can overshoot by one.
- A child whose inherited cgroup was removed between clone and enqueue falls back to the root (`adopt`).
- Membership counts are not yet visible from userland, so charge/uncharge balance is untested beyond churn; CG2
  must assert `cgroup.procs`/`pids.current` return to baseline.
- Not done by design: cgroup.freeze/kill (CG5), CLONE_INTO_CGROUP.
