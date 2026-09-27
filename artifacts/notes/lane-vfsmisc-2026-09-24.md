# Lane `vfsmisc` — 2026-09-24

Outcome: FIXED (all 5 items). Branch `lane/vfsmisc` @ `730a0b5`, pushed to
origin. Machine: Mac, aarch64 HVF (fast) + x86_64 TCG. Worktree
`~/code/leandros-vfsmisc`, base `origin/main` @ `fc5a6bc`. Scope per
`artifacts/notes/lane-protocol-2026-09-24.md`: access(2) ruid-not-euid,
atime-on-read (relatime), greeter-launch `initgroups`, vfstest header comment,
greetd `EBADF` self-pipe. Background: `lane-vfsperm2-2026-09-18.md` (left all
four non-EBADF items open, with the exact same wording TODO.md uses),
`lane-misc-2026-09-18.md`.

## 1. `access(2)`/`faccessat(2)`: real uid/gid, not effective (unless `AT_EACCESS`)

Confirmed the premise: `servers/vfs::cred_of` (the only place a permission
gate ever built a `Cred`) always used `sched::euid_of`/`egid_of`, and the
kernel's `sys_faccessat` silently discarded its `flags` argument.

- `sched/src/lib.rs`: `ruid_of`/`rgid_of`, mirroring `euid_of`/`egid_of`.
- `servers/vfs/src/lib.rs`: `real_cred_of(pid)` (real ids + the same
  supplementary-group list — this kernel doesn't swap groups on a setuid
  exec, so there's nothing to differ there). `handle_access` takes an
  `eaccess: bool` and picks `cred_of`/`real_cred_of` accordingly; threaded
  through the mount-proxy call (`xattr_proxy`'s 3rd arg) for f2fs.
- `servers/f2fs/src/lib.rs`: the `VFS_ACCESS` dispatch arm builds its own
  cred from `real_cred_of(caller_pid)` unless the eaccess bit is set (`ms.cred`,
  built once per call for every other op, stays effective-id as before).
- `kernel/src/syscall.rs`: `sys_faccessat` decodes Linux `AT_EACCESS` (0x200)
  from `flags` (previously `_flags`, unused) and forwards it as a third
  `VFS_ACCESS` message word. The legacy `ACCESS` syscall arm already called
  `sys_faccessat(AT_FDCWD, a0, a1, 0)` — flags 0 — so plain `access(2)` gets
  real-id semantics for free; only `faccessat`/`faccessat2` can opt into
  `AT_EACCESS`.

Wire encoding: the eaccess flag travels as a separate 3rd `VFS_ACCESS`
message word rather than packed into a spare bit of `amode` — either would
have worked (`handle_access`/`xattr_access` only ever mask `amode & (4|2|1)`
for the R/W/X bits), but a separate word is clearer.

## 2. atime on read (relatime)

Confirmed: `TmpFileEntry.atime`/f2fs's on-disk `i_atime` were written at
create/write/utimensat time only; nothing touched them on a read, despite
`/proc/mounts` already claiming `rw,relatime`.

Implemented Linux's actual `relatime` rule (`relatime_need_update`): skip the
update unless `atime <= mtime`, `atime <= ctime`, or `now - atime >= 1 day`.
Duplicated (small, ~6 lines) in both `servers/vfs/src/lib.rs` and
`servers/f2fs/src/lib.rs` rather than shared, matching this tree's existing
convention of independent per-backend timestamp helpers (`tmp_touch_mtime` vs
`inode_touch_mtime`).

- tmpfs: `handle_read`'s `VnodeKind::TmpFile` arm bumps `tmp[idx].atime` under
  the same `TMP_FILES` lock it already holds for the copy, gated by
  `relatime_needs_update`.
- f2fs: new `touch_atime_relatime(ms, ino)`, called from `handle_read` after
  `read_file_data`. Loads the inode block once (immutable) to evaluate the
  gate, and only re-touches it (`cache.get_mut` + `nat_update`) when the gate
  fires — a read that doesn't need the update costs one cache lookup, no
  write.

At file creation atime == mtime == ctime, so the *first* read after creation
always updates (satisfies `atime <= mtime`); a second read immediately after
must NOT move it again. `vfstest`'s new `atime_relatime_{tmpfs,f2fs}` checks
exactly that pair.

## 3. greeter-launch `initgroups`

`userland/greeter-launch/src/main.rs` dropped privilege with
`setresgid`/`setresuid` only — no `setgroups` call at all, so the greeter's
Wayland-client process ran with whatever supplementary groups `cosmic-comp`
(root) had, not the `cosmic-greeter` account's own list from `/etc/group`.
Added a byte-oriented `lookup_groups` (mirrors `/bin/login`'s, duplicated
since this is a separate `no_std` crate) and a `setgroups` call ahead of the
gid/uid drop. `cosmic-greeter` currently has zero `/etc/group` memberships in
the staged image, so the visible effect is that the drop now *clears*
whatever it inherited instead of silently keeping it — the interesting
mechanism (`setgroups` + fork/exec inheritance of supplementary groups) is
already covered by `permtest`'s `group_allowed_supplementary`/`--groups`
probe from lane `vfsperm2`, which exercises the identical kernel path.
Verified via boot: `GREETER-LAUNCH: dropped to uid ... gid ...` diagnostic
line still fires with no error, i.e. `setgroups` doesn't fail root's own
drop.

## 4. vfstest header comment

Fixed: it claimed `wait4()` returns a raw `exit()` argument; `lane/vfsperm2`
already found (and vfstest's existing bodies already assume) the
Linux-encoded `WEXITSTATUS` form. Every comparison in the file only checks
`wstatus == 0`, which is identical either way, so this was a comment-only
bug, not a functional one. Reworded to state the actual (correct) encoding
and warn against comparing to a bare `1`.

## 5. greetd/brush `EBADF` on the tokio self-pipe — FIXED (delegated to deep-reasoner)

Root cause: `sys_execve` ran the VFS/NET close-on-exec fd sweeps *before*
`sched::dethread_current_group()` (the call that kills every other thread in
the caller's group). Linux's `de_thread` runs before `do_close_on_exec` —
the opposite order — precisely because a sibling thread is still alive and
still using those fds until it's reaped. Brush (this OS's `/bin/sh`) is a
tokio program; greetd launches every session as `sh -c '...; exec <cmd>'`,
and by the time the `exec` runs, brush's SIGCHLD/SIGTSTP job-control
listener has already stood up tokio's signal self-pipe — a `SOCK_CLOEXEC`
socketpair (`servers/net`), one copy per driver via `F_DUPFD_CLOEXEC`. With
the fds closed first, a tokio worker thread parked in `epoll_wait` on its
copy woke to a `read()` returning EBADF and panicked, *before* the
now-reordered kill could reach it — an entirely real, if harmless (the exec
itself always completed), kernel bug, not a brush/tokio defect.

Fix (`kernel/src/syscall.rs`, `sys_execve`): moved `dethread_current_group()`
to just before the cloexec sweeps (still the last *fallible-adjacent* step —
everything after it is infallible, preserving the existing "point of no
return" invariant); `pid` is re-read immediately after, since a non-leader
caller takes over the leader's pid there (the tgid the fd table is keyed by
does not change).

New regression test `userland/exectest::test_exec_hides_cloexec_from_siblings`
(30 rounds: fork, nonblocking `SOCK_CLOEXEC` socketpair, a `pthread_create`d
thread polling it, then `execve`; the thread must see the fd's real end — the
whole process dying — never a report of its own).

Independent confirmation (mine, not the reasoner's) that this was a real,
not hypothetical, bug: on aarch64/HVF the pre-fix test passed 30/30 (HVF's
speed rarely opens the window); on x86_64/TCG the SAME pre-fix build caught
it directly on one run — `sibling saw the cloexec sweep: 009` (errno 9 =
EBADF) — then passed clean on immediate re-runs, i.e. a genuine
timing-dependent race, exactly matching two prior waves' "intermittent,
unexplained" reports. Post-fix: exectest clean on 3 separate boots on
aarch64/HVF and 5 on x86_64/TCG (the arch that reproduced it pre-fix) — see
Evidence.

**New bug found in the same area, NOT fixed (flagging for lane `execleak`,
which owns exec/process teardown):** on aarch64, `killmt exec_worker` starts
failing ("E6 exe /bin/init") after ~70 `SIGKILL`s of multi-threaded
processes. `sched::EXE_PATHS` (the 64-slot `/proc/self/exe` table) leaks one
slot per such kill — `kill_next_group_member_except` reaps a group leader
without calling `clear_exe_path`, and once the table fills, `/proc/self/exe`
answers `/bin/init` for every process system-wide. Reproduced with
`killmt 70 worker_parked` (never execs) then one more `exec_worker` pass.

## Tests added

`userland/vfstest`: `access_real_vs_effective_{tmpfs,f2fs}` (root process,
`setresuid(1000, 0, 0)` to split real/effective without needing a setuid
binary: real-id `faccessat` denied on a 0600 root-owned file, `AT_EACCESS`
allowed, plain `open()` unaffected as a control), `atime_relatime_{tmpfs,f2fs}`
(first read moves atime, second immediate read doesn't).

## Evidence (fresh images from `build-all.sh`, root login over serial)

Pre-fix (item 5 still open): see the FAIL captured on x86_64/TCG in section 5
above (`exec_hides_cloexec_from_siblings`, one run, errno 9). All numbers
below are POST-fix, on the final combined tree (all 5 items together):

| | aarch64 / HVF | x86_64 / TCG |
|---|---|---|
| `vfstest` | 45/45 PASS incl. both new pairs | 45/45 PASS incl. both new pairs |
| `permtest` | 43/43 PASS, 0 failures | 43/43 PASS, 0 failures |
| `exectest` | 10/10 PASS ×3 separate boots, incl. `exec_hides_cloexec_from_siblings` | 10/10 PASS ×5 separate boots (the arch that reproduced the bug pre-fix), incl. `exec_hides_cloexec_from_siblings` |

The deep-reasoner also independently validated (its own report, not
re-verified by me): `forktest`, `pthreadtest`, `sigchldtest` pass on
aarch64; `killmt 20 exec_worker` 20/20 on a fresh aarch64 boot; a real
`sh -c 'x=$(/bin/id -u); exec /bin/hello'` loop (brush, greetd-style) ran
10× with no panic post-fix.

Build note: both arches needed `SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk`
exported before `build-all.sh` — the Mac's default SDK resolution
(`MacOSX27.0.sdk`) is a stale/mismatched host toolchain SDK whose `.tbd`
files name an architecture triple (`arm64e.x1-macos`) the installed linker
doesn't understand ("tapi error: malformed file" / "unknown architecture"),
breaking every host-side build-script link (e.g. `libm`'s). Unrelated to any
lane's code; a pre-existing Mac environment issue, not fixed here (scoped the
env var to just these two build invocations rather than touching global
Xcode/`xcode-select` config).

## Left open

- **New, unfixed bug found while verifying item 5** (see section 5 for
  detail): `sched::EXE_PATHS` (the 64-slot `/proc/self/exe` table) leaks one
  slot per `SIGKILL` of a multithreaded process's group leader —
  `kill_next_group_member_except` doesn't call `clear_exe_path`. After ~70
  such kills the table fills and `/proc/self/exe` starts answering
  `/bin/init` for every process. Belongs to lane `execleak` (owns exec/
  process teardown); not fixed here, out of this lane's declared scope.
  Repro: `killmt 70 worker_parked` then one more `killmt exec_worker` pass.
- `test_exec_hides_cloexec_from_siblings` never caught the pre-fix bug on
  aarch64/HVF in this session (only on x86_64/TCG, where the race window is
  wider) — worth deciding whether it needs more than 30 rounds to be a
  reliable gate on the fast accelerator, since HVF is this Mac's default.
- Item 3 (greeter-launch initgroups) has no dedicated new automated test —
  the underlying `setgroups`/fork+exec-inheritance mechanism it now uses is
  already covered by `permtest`'s pre-existing `group_allowed_supplementary`
  case; only boot-log verification (`GREETER-LAUNCH:` line, no error) covers
  the wiring into `greeter-launch` itself. `cosmic-greeter` has zero
  `/etc/group` memberships in the staged image, so there's no positive
  (nonzero-groups) boot-time check possible without also changing
  `scripts/mkfs-f2fs-populated.py`'s account seeding, which is out of this
  lane's scope.
- Shared files touched outside this lane's listed scope: `kernel/src/syscall.rs`'s
  `sys_execve` (item 5's fix) overlaps lane `execleak`'s ownership of
  "kernel exec/process teardown" — flagging for the orchestrator to
  reconcile at merge time, though the diff there is small and additive
  (moved one existing call earlier + re-read `pid`, no logic removed).
  `userland/exectest` is not listed as owned by any other lane this wave.
