# lane/nspidproc (P3) - /proc in pid namespaces - 2026-11-08

FIXED. servers/vfs/src/lib.rs only (+ nstest).
- Numeric /proc components (pid, task/<tid>, fd, fdinfo, root) go through `sched::pidns::from_cur`
  (`proc_parse_num`, `pv_in`): outside pids -> ENOENT, `1` -> the namespace init.
- /proc listing and /proc/<pid>/task listing: invisible tasks skipped, names printed in the reader's view
  (`pv`); the getdents cursor stays kernel-pid based. Snapshot paths use view numbers so re-parse is consistent.
- readlink(/proc/self) (new: it was ENOENT before) = tgid in view; /proc/thread-self = "<tgid>/task/<tid>".
- status: Tgid/Pid/PPid/PGid in view, TracerPid:0, NStgid/NSpid/NSpgid/NSsid at each level from the
  reader's ns to the task's ns. stat: pid/ppid/pgrp/session in view.
- nstest: proc_self_tgid, proc_self_init, proc_listing (also hidden outside pid), status_views (2 and 3 levels).
  All 4 failed before the change (aarch64), pass after.
