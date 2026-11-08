# lane/secproc 2026-11-08 (S5, /proc part): DELIVERED

- /proc/<pid>/status and /proc/<pid>/task/<tid>/status now carry NoNewPrivs, Seccomp (0/1/2), Seccomp_filters (chain depth)
  after the Cap* lines. Source: sched::seccomp::status_of (RUN_QUEUE find_pid, same path as caps_of); VFS in servers/vfs/src/lib.rs.
- Note: /proc content is generated at open(), so an fd opened before strict mode shows the old mode; the strict test reads
  the child's status from the parent while the child blocks in read().
- seccomptest: proc_status_seccomp (0 -> NNP 1 -> 2 filters... Seccomp 2/filters 1 -> 2, task/<tid> too), proc_status_strict (Seccomp: 1). PASS aarch64.
- ffsandbox.py aarch64 --mode both: off: 9 firefox procs, all Seccomp 0 (parent, socket, forkserver, tab x4, rdd, utility), video ok.
  on: only parent(737) + forkserver(833) survive, both Seccomp 0 / filters 0; all content children log "no tsync support; using
  signal broadcast" and die on signal 31 (SIGSYS: TRAP placeholder kill + TSYNC EINVAL) -> expected until sectrap/sectsync merge. RESULT on=False off=False.
