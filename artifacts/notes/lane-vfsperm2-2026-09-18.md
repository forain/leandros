# Lane `vfsperm2` — utimensat, exec x-bit, supplementary groups, default-ACL inheritance (2026-09-18)

Follow-up to lane/perms (`e67aeaf`), which enforced permissions on traversal, creation,
removal and AF_UNIX. This lane closes the four items that note left open.

## Premises, verified against the tree before changing anything

| Item | Claim in TODO | Finding |
|---|---|---|
| utimensat | "kernel no-op" | True, and deeper: `UTIMENSAT => 0` in the dispatch table, **and neither filesystem kept timestamps at all** — `write_stat_full_rdev` zeroed bytes 72..128 of every `struct stat`, tmpfs entries had no time fields, and the on-disk f2fs inode's time slots were never written. Implementing the syscall alone would have set values nothing could read back. |
| exec x-bit | "unchecked" | True for ELF binaries: `sys_execve` ran `script_exec_permitted` (VFS_ACCESS X_OK) only inside the `#!` loop; the final ELF was opened and mapped with no permission question asked. A 0644 ELF ran for anyone. |
| supplementary groups | "none exist" | True: `SETGROUPS => 0` / `GETGROUPS => 0`, no field on `Task`, and `xattr::access_check` matched groups by `egid == gid` only. |
| default-ACL inheritance | "missing" | True, and ACLs are real, not a phantom: both tmpfs and f2fs store `system.posix_acl_default` (validated, dir-only) and honour `system.posix_acl_access` on every gate. Nothing consulted the default ACL at create time, so it was write-only storage. |

## What changed

### One credential type for every gate (`servers/xattr`)
`xattr::Cred { euid, egid, ngroups, groups[32] }` with `in_group(gid)` replaces the
`euid, egid` pair in `may_access` / `access_check` / `may_read_xattr` / `may_write_xattr`.
`vfs_server::cred_of(pid)` is the single constructor from a pid (f2fs uses it too). Group
membership = egid or any supplementary group, in the mode-bit path and in the ACL walk
(`GROUP_OBJ` and named `GROUP` entries). chown's "gid only to a group you belong to" rule
consults the same list.

### Supplementary groups (`sched`, kernel, libc, login, image)
- `Task.groups[32]/ngroups`, copied at `fork_current` and `clone_thread`; exec leaves creds
  alone so they survive it.
- `setgroups(2)` (root only → EPERM; > 32 → EINVAL), `getgroups(2)` (count query, EINVAL when
  the buffer is short).
- `/bin/login` parses `/etc/group` member lists and `setgroups` before dropping (initgroups
  without a libc). greetd's session worker calls musl `initgroups`, which is now real.
- `/etc/group` seeds `video:x:44:leandro` and `input:x:104:leandro`.

### utimensat (kernel, both filesystems)
- `sys_utimensat` (+ x86-64 legacy `utimes`, `futimesat`, `utime`) resolves `UTIME_NOW` in
  the kernel to `sched::clock_ts()` — the same counter-derived clock `clock_gettime` reads,
  registered by the kernel at boot (`sched::register_clock_ns`) — and forwards
  `VFS_UTIMENS` / `VFS_LUTIMENS` (AT_SYMLINK_NOFOLLOW) / `VFS_FUTIMENS` (NULL path = fd form).
  `UTIME_OMIT` travels as the nsec sentinel; both omitted is a successful no-op without a
  permission check, as on Linux. nsec out of range is EINVAL.
- Rule (Linux `utimes_common`): any explicit time needs ownership or root (EPERM);
  "now" needs write permission or ownership (EACCES).
- tmpfs: `TmpFileEntry` carries atime/mtime/ctime; stamped at creation, mtime+ctime on
  write/truncate, ctime on chmod/chown; reported by stat/fstat/lstat.
- f2fs: on-disk inode bytes 36..72 (`i_atime`/`i_mtime`/`i_ctime` sec + nsec), previously
  unused; stamped by `create_inode`, `write_file_data`, truncate, chmod/chown. Old inodes
  read back 0, which is exactly what stat reported before.

### exec x-bit (kernel)
`exec_permitted` (the former `script_exec_permitted`) now also runs on the final ELF /
interpreter. Only EACCES is an answer; ENOENT/ENOSYS fall through to the loader so a missing
binary still reports ENOENT and initrd binaries keep the stat-mode fallback. Root needs at
least one x bit (`access_check` already encoded CAP_DAC_OVERRIDE's exception).

### Default-ACL inheritance (`xattr::acl_create`, both filesystems)
Linux `posix_acl_create`: a child of a directory with a default ACL gets that ACL, with
USER_OBJ / MASK-or-GROUP_OBJ / OTHER intersected with the requested mode, as its access
ACL (stored only when non-trivial), and a child directory also gets the default ACL
verbatim. The umask is NOT applied in that case, so it now travels with the mode
(`xattr::pack_create_mode`: `mode | umask << 16` on VFS_OPEN / VFS_MKDIR) and each
filesystem applies it through `acl_create` — one function, so tmpfs and f2fs cannot
disagree. Sites: tmpfs open(O_CREAT)/mkdir/mknod/symlink, f2fs open(O_CREAT)/mkdir.

## Tests
- `permtest` (root, both `/data` f2fs and `/tmp` tmpfs): `group_denied_without_membership`,
  `group_allowed_supplementary` (incl. `permtest --groups 44` re-exec probe for fork+exec
  inheritance and chgrp to a supplementary group), `exec_xbit_user` / `exec_xbit_root`
  (f2fs only: the x86-64 `hello` is larger than a tmpfs file), `utimens_rules`,
  `utimens_root`, `default_acl_inherit`, `default_acl_named_user`, `default_acl_other_denied`.
- `vfstest`: `timestamps_tmpfs` / `timestamps_f2fs` (create ≈ CLOCK_REALTIME, write
  advances mtime, utimensat exact).

## Evidence (fresh images from `build-all.sh`, root login over serial)

| | aarch64 / HVF (Mac) | x86_64 / TCG (Mac) |
|---|---|---|
| `permtest` | 43/43 PASS, 0 failures (f2fs matrix incl. `exec_xbit_*`, tmpfs matrix incl. sockets) | 43/43 PASS, 0 failures |
| `vfstest` (first run on the image) | 41/41 PASS incl. `timestamps_tmpfs`/`timestamps_f2fs` | 41/41 PASS incl. both timestamp cases |
| `exectest` | — | 10/10 PASS (the `#!` gate, ENOEXEC, ELOOP paths unchanged) |
| `login leandro` → `id` | `uid=1000(leandro) gid=1000(leandro) groups=1000(leandro),44(video),104(input)` | same |
| coreutils `touch`/`stat` | `stat` shows real times; `touch -d @5000` → `%Y` = 5000 (legacy `utimes` on x86-64) | same |
| Greeter login → desktop | boot-time greeter, typed `leandro`+Enter via HMP sendkey: uid-1000 session, cosmic-panel + dock painted at ~4 min 52 s guest time; `/var/log/greetd.log` has **zero** `Permission denied`/EACCES | not run (x86_64/TCG desktop is a ~10 min affair and the code paths are arch-independent) |
| `chmod 644` a copied ELF, run it from brush | `Permission denied (os error 13)`, rc 126 | — |

vfstest's `chroot_confines_symlink_resolution` and `xattr_list_*` FAIL on a **re-run** in the
same image (the known `/tmp/jail` / `xa_list` residue, sibling lane `misc`) — the numbers
above are first runs.

Two findings during the run, both in the new test, not the kernel: `wait4` now returns the
Linux-encoded status (exit code in bits 8..16 — permtest's header comment said raw, and
`exectest` already decodes `>> 8`), and the kernel read the clock once per timestamp so a
`NULL` utimensat gave atime != mtime by a few µs; it now reads once for both.

## Left open
- `access(2)` still evaluates with the effective ids (Linux uses the real ids unless
  AT_EACCESS); no setuid binaries exist so nothing observes it.
- atime is never updated by reads (noatime behaviour); `/proc/mounts` says relatime.
- `greeter-launch` does not initgroups the greeter account (it has no memberships).
