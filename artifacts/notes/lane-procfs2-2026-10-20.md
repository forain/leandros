# lane procfs2 (L10.6) -- 2026-10-20

Applied: (b) vfork child exec starts with its own fault counters (sched
`replace_address_space`, borrowed-space check), (d) proctest `majflt_cold`
with a 2 MiB `/bin/proctest-cold.bin` staged by `mkfs-f2fs-populated.py`,
(c) `stat`/`access` of the /proc diagnostic files (vfs `PROC_DIAG_FILES`),
(f) legacy faccessat ignores a3; faccessat2 rejects unknown flags and modes
outside rwx with EINVAL.

PENDING: (a) PR_SET_PDEATHSIG from every thread of the child (+ last
non-leader thread reparents). It edits exit()/reparent_children, which
lane/leaderexec is changing; do it after that merges (see design procfs2.md).
(e) `lstat("/proc/self/exe")` was already fixed by d2fecaca.

Known gap: faults a vfork child takes before exec are counted to the parent.
