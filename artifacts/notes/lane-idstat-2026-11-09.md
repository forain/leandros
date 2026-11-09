# lane idstat (2026-11-09) - PARTIAL (knob cases unverified)

Rule: kernel buffers carry kuids; translation only on the copy to user memory.
- servers/vfs write_stat_full_rdev: view_id only when the destination is a user address; weak debug assert on caller thread (no caller pid is available in the fill).
- kernel/syscall.rs: user-facing fstat_into/fstatat_into fill a kernel buffer first, so they call new vfs::translate_stat_owner_for_user before copy_to_user. statx translates itself. exec_setid reads raw kuids.
- Audit of kernel VFS_STAT callers: exec_setid (raw, correct), the rest read mode/dev only.
- /proc/<pid>/status Groups line goes through view_id. SIGCHLD si_uid in signal.rs was already view_id on main; waitid does not fill si_uid.
- userland/idmaptest created here (idcore had no commits): statx_matches_stat, proc_pid_owner, status_groups_view PASS; stat_unmapped_65534 and sigchld_si_uid_view SKIP without idmap=test (unverified). Expect an add/add conflict with lane/idcore on idmaptest.c and wiring; keep both case sets.
Evidence (aarch64 HVF): idmaptest 3 pass/2 skip; default regress all RC=0; ffsandbox on: RESULT True.
Open: run the two knob cases once idcore lands.
