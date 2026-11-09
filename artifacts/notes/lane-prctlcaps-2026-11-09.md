# lane/prctlcaps 2026-11-09 (conmon/crun kernel needs)

Status: FIXED (aarch64; captest 27/27, nstest green).

- PR_SET/GET_CHILD_SUBREAPER: `Task::child_subreaper` (per process, not inherited). `pidns::subreaper_for`
  (Linux find_new_reaper): nearest live subreaper ancestor in the dead process's own pid namespace, else the
  existing ns-init/init choice; never for init or a namespace init. Zombie orphans go to the same reaper.
- PDEATHSIG already existed.
- Caps: `Task::caps` is now `[eff, prm, inh, bnd, amb, securebits]`. PR_CAP_AMBIENT (RAISE/LOWER/IS_SET/CLEAR_ALL),
  PR_GET/SET_SECUREBITS (locks, unknown bits, CAP_SETPCAP), PR_GET/SET_KEEPCAPS, NOROOT, NO_SETUID_FIXUP,
  NO_CAP_AMBIENT_RAISE. capset trims ambient to prm & inh. Exec (Linux rules without file caps): P' = root ? I|bnd : 0, | A';
  E' = P' only for euid 0, else A'; set-id exec clears A; KEEP_CAPS cleared on exec. setuid fixup clears ambient with perm.
  /proc status CapAmb. PR_CAPBSET_DROP checks EPERM before EINVAL (Linux order); READ existed.
- keyctl/add_key/request_key: ENOSYS (unchanged, deliberate). crun tolerates ENOSYS/EPERM for
  KEYCTL_JOIN_SESSION_KEYRING; no session-keyring semantics are needed. captest guards the ENOSYS.
- personality(2): new, per-task word inherited by fork, 0xffffffff queries.
- oom_score_adj: new /proc/<pid|self>/oom_score_adj, readable+writable, range check, lowering needs CAP_SYS_RESOURCE
  (EACCES), inherited by fork.
- setrlimit/prlimit64 NOFILE/NPROC already worked (guarded in captest). Hard-limit raise still tests euid 0, not CAP_SYS_RESOURCE (open).
- Guards: on main's kernel captest fails 11 cases (ambient_*, keepcaps, securebits*, noroot_exec, no_setuid_fixup,
  subreaper_flag, orphan_to_subreaper, personality, oom_score_adj); all pass with the fix.
