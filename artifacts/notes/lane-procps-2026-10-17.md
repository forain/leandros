# lane/procps (2026-10-17): /proc/<pid>/comm, thread names, `ps`

## What changed

- `sched/src/lib.rs`: a tid-keyed thread-name table (`set_comm`, `comm_of`,
  `clear_comm`). Unset = basename of the thread group's executable (the old
  behaviour). Cleared on exec (`set_exe_path`) and on task exit/reap; a forked
  child inherits its parent's explicit name (as Linux).
- `kernel/src/syscall.rs`: `prctl(PR_SET_NAME)` now stores the name (it was a
  no-op); `PR_GET_NAME` returns the real one (it always said "leandros").
  Same table as procfs.
- `servers/vfs/src/lib.rs`:
  - `comm` added to `PROC_PID_FILES`: `/proc/<pid>/comm`, `/proc/self/comm`
    read as `name\n` (<= 15 bytes) and show in the `/proc/<pid>` listing.
    `stat` `(comm)` and `status` `Name:` now use the same source
    (`sched::comm_of`), so a rename shows there too.
  - Opening `/proc/<tid>/comm` for writing (O_WRONLY/O_RDWR): allowed for root
    or the same euid, else EACCES; ESRCH-style ENOENT for a dead pid. The open
    returns an ephemeral tmp snapshot whose fake path is
    `/tmp/.proc_comm_<tid>`; `handle_write` recognises that path (ephemeral
    entries only, so users cannot forge it) and routes the bytes to
    `sched::set_comm` (first 15 bytes, one trailing newline stripped). The
    permission is re-checked at write time.
- `userland/ps` (`ps`, `pstest`), no allocator, no external tools. Registered
  in `userland/Cargo.toml`, `scripts/mkfs-f2fs-populated.py` (bins list) and
  `runtests.py` (regress list + new `procps` suite).

## ps

`ps` (session = caller's session, since tty_nr is 0 everywhere), `-e/-A/-a/-x`
(all), `aux`, `-ef`, `-l`, `-o a,b=HDR`, `-p`, `-T/-L`, `--no-headers`/`h`.
TIME = utime+stime; START/STIME are UTC; COMMAND = /proc/<pid>/cmdline, else
`[comm]`. Limits and caveats:

- TTY is always `?` (kernel reports tty_nr 0).
- This kernel keeps no argv, so cmdline is just the executable path.
- There is no `/proc/<pid>/task/<tid>/` directory; `-T/-L` therefore list one
  row per process (and use `/proc/<pid>/task` automatically if it ever
  appears). `/proc/<tid>/comm` itself works for any tid that `proc_stat_of`
  knows, so per-thread names are visible by tid.
- Kernel threads that have no exe path show `/bin/init` (existing fallback).

## Tests

`/bin/pstest` (26 checks): default name, write/read-back, truncation,
newline handling, prctl round trip, stat shows the name, EACCES for another
user's process, owner write works, root may rename a user's process, comm of
an exited process is ENOENT, and `ps -e`, `ps`, `ps -o pid,comm -p $$`,
`aux`, `-ef`, header suppression, `-T`, `-p <missing>`.
Suite: `runtests.py <arch> <tag> --suite procps` (also in `regress`).

## Merge risks

`servers/vfs/src/lib.rs` (PROC_PID_FILES, gen_proc_self_content comm,
handle_open proc branch, handle_write TmpFile arm), `sched/src/lib.rs`
(clear sites next to `clear_exe_path`), `kernel/src/syscall.rs` (sys_prctl),
`runtests.py` (regress list), `scripts/mkfs-f2fs-populated.py` (bins list),
`userland/Cargo.toml` (members list).
