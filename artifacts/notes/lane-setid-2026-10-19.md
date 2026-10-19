# Lane setid (2026-10-19): set-ID bits, nosuid, pidfd_getfd, fsuid

Bug list items L7.2, L7.5 and L7.8 (openbugs-2026-10-19.md, section L7, P0).

## L7.2: set-ID bits survive writes; no nosuid; alias mounts honour suid

- **Writes strip set-ID bits.** This follows Linux `file_remove_privs`. A write, ftruncate,
  fallocate or O_TRUNC open by anyone whose fsuid is not 0 clears S_ISUID. It also clears
  S_ISGID when group execute is set. The owner loses the bits too. The rule lives in
  `xattr::setid_kill_bits`.
  - f2fs applies it in `kill_setid` (`write_at`, `handle_ftruncate`, and the O_TRUNC
    open).
  - tmpfs applies it in `tmp_kill_setid` (the write arm, ftruncate, fallocate, O_TRUNC).
    The tmpfs write looks up the caller's fsuid only when the file has a set-ID bit, so
    ordinary writes cost nothing extra.
  - Before this, only chown cleared the bits.
- **MS_NOSUID.** `MountEntry.nosuid` is set by mount(2) and replaced by MS_REMOUNT.
  execve (`exec_setid`) ignores set-ID bits when the file's mount has the flag
  (`vfs::path_on_nosuid_mount`). /proc/mounts and mountinfo list `nosuid`.
- **Alias mounts are always nosuid.** These are proc, devtmpfs, sysfs, devpts and bind
  mounts. The path walk (`backing_walk`) reports whether it went through one, including
  through `/proc/<pid>/root|cwd`. For an alias mount, /proc/mounts shows `rw,nosuid`.
  claude-run binds /tmp, /home and the user's home into the Alpine root, so a set-ID file
  reached through those paths no longer grants privileges.

## L7.5: pidfd_getfd used same-ruid-or-root

The new check is `sched::cred::ptrace_attach_check`, Linux PTRACE_MODE_ATTACH_REALCREDS:
- the caller's own thread group always passes;
- root (CAP_SYS_PTRACE) always passes;
- otherwise the caller's real uid must equal the target's real, effective and saved uids,
  its real gid the target's three gids, and the target must be dumpable.

The secipc `ipc_send_check` uses the caller's effective ids and has no dumpable condition,
so it could not be reused as is.

Dumpable is now tracked (`Task.dumpable`, per process):
- An exec makes the process dumpable again, except an AT_SECURE exec, which makes it
  non-dumpable.
- Any change of euid, egid, fsuid or fsgid clears it (Linux `commit_creds`).
- `PR_SET_DUMPABLE` and `PR_GET_DUMPABLE` are real now. Values other than 0 and 1 are
  EINVAL.

## L7.8: no separate fsuid/fsgid

- `Task.fsuid` and `Task.fsgid` are new. Every set*id call and execve sets them to the
  effective ids.
- `setfsuid`/`setfsgid` return the previous value. The change is taken when the new id is
  the caller's real, effective, saved or current fs id, or when the caller is root.
- `vfs::cred_of` (the only `xattr::Cred` constructor) now builds from the fs ids. So
  permission checks, new-file ownership, chmod, chown, utimens and the set-ID strip all
  use the fsuid.
- /proc/<pid>/status shows the fs ids in the 4th column.

## Tests

Root-mode cases added to privtest:
- `setid_{write,ftruncate,otrunc}_by_user_strips_{disk,tmpfs}`,
  `setgid_without_gx_survives_write_*`, `setid_write_by_root_keeps_*`
- `nosuid_bind_*`: a bind mount, then `remount,bind,nosuid`
- `suid_mount_*` / `nosuid_mount_*`: the root f2fs device mounted a second time,
  read-only, without and then with MS_NOSUID
- `pidfd_getfd_*`, `dumpable_cleared_by_id_change`, `prctl_set_dumpable_2_einval`
- `setfsuid_*`, `fsuid_*`, `status_shows_fsuid`

Results (aarch64, HVF):
- With main's kernel (the 19:28 boot image) and the new privtest, 19 cases fail. The
  pidfd cases are skipped there, because the target setup needs dumpable tracking.
- With this lane's kernel, privtest passes (0 failures).
- `--suite regress` passes 38/38.

## Open

- Shared-mmap stores do not strip set-ID bits. Linux does not strip them there either for
  most filesystems.
- chmod by a non-member of the file's group does not drop S_ISGID (Linux
  `setattr_prepare`).
- The tmpfs roots (/tmp, /run, ...) take no mount flags.
