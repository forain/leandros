# Gap 1: memfd unnamed-tmpfs-inode audit

Goal: enumerate every path that resolves a tmpfs inode BY NAME (path lookup) vs BY IDX/fd
(VnodeKind::TmpFile{idx} or equivalent), to scope what must be fixed before memfd unlink
can be safely enabled.

Status: IN PROGRESS (audit started)

## Key upfront finding (read this first)

The comment at kernel/src/syscall.rs:6904-6917 (added in commit b3659fa,
2026-07-30) claims that issuing VFS_UNLINK right after memfd_create breaks
because "ftruncate + the K1 shared-VMO mmap path... still resolve the inode by
name rather than through the fd's VnodeKind::TmpFile { idx }".

Static reading of the current tree CONTRADICTS that claim for both named sites:

- handle_ftruncate (servers/vfs/src/lib.rs:4698) takes fd, looks up
  tbl.fds[fd].kind -> VnodeKind::TmpFile { idx, .. }, and indexes
  TMP_VMOS[idx] / TMP_FILES[idx] directly. No tmp_find(path) call anywhere in
  this function. Unaffected by the name being gone.
- The K1 shared-VMO mmap path (kernel/src/syscall.rs:1659-1679 ->
  vfs::vmo_acquire_frames, servers/vfs/src/lib.rs:537) resolves via
  tmpfile_owner_of(pid, fd) (fd-table lookup -> idx). No path lookup.
- mark_memfd (servers/vfs/src/lib.rs:471) also goes through tmpfile_owner_of.
  idx-based.
- Corroborated by pre-existing design notes k1-vmo-design-minimal.md:234-245
  and k1-vmo-progress.md:31-33 ("open resolves fd->OWNER idx... read/write/
  ftruncate/fcntl/fstat can key TMP_VMOS[idx] directly"), both written BEFORE
  b3659fa.
- A working precedent already ships: commit 36f62d0 (2026-07-23, drm/dmabuf
  PRIME export) unlinks its /tmp/dmabuf:<n> node immediately after
  install_dmabuf_vmo — same create-then-unlink-while-fd-open idiom, same VFS
  machinery, not known to be broken.

CONCLUSION: the specific failure mode described in the b3659fa comment does
not reproduce from static analysis — ftruncate/mmap/mark_memfd are already
idx-keyed. Either (a) the comment is stale/inaccurate and the actual "TRIED
AND REVERTED" breakage had a different root cause not yet identified, or (b)
there is a runtime-only hazard invisible to this read-only audit. Flagged as
UNSURE below. Recommend an instrumented runtime re-test before trusting either
conclusion. The exhaustive table below stands regardless.

## Full site table

| Site (file:line) | Function | Resolution | Breaks when unnamed? |
|---|---|---|---|
| servers/vfs/src/lib.rs:4698 handle_ftruncate | ftruncate(fd) | fd -> tbl.fds[fd].kind -> idx; indexes TMP_VMOS[idx]/TMP_FILES[idx] | NO |
| kernel/src/syscall.rs:1659-1679 + servers/vfs/src/lib.rs:537 vmo_acquire_frames | K1 shared MAP_SHARED mmap | fd -> tmpfile_owner_of(pid,fd) -> idx | NO |
| servers/vfs/src/lib.rs:459 tmpfile_owner_of | shared helper (mark_memfd/install_dmabuf_vmo/dmabuf_handle_of/vmo_acquire_frames) | fd -> FD_TABLES -> idx | NO (the idx-safe primitive) |
| servers/vfs/src/lib.rs:471 mark_memfd | called right after VFS_OPEN in sys_memfd_create | fd -> tmpfile_owner_of -> idx | NO |
| servers/vfs/src/lib.rs:2852/2984 read/write TmpFile arm | read(fd)/write(fd) | VnodeKind::TmpFile{idx,pos,..} from fd table | NO |
| servers/vfs/src/lib.rs:3226/3618-3620 lseek TmpFile arm | lseek(fd) | idx from fd's VnodeKind | NO |
| servers/vfs/src/lib.rs:3124-3184 handle_close | close(fd) final path | VnodeKind::TmpFile{idx,..} -> tmp_release_ephemeral(idx) | NO (intended reclaim path) |
| servers/vfs/src/lib.rs:3496 release_vnode (exit/exec cloexec sweep) | process teardown fd release | same idx -> tmp_release_ephemeral(idx) | NO |
| servers/vfs/src/lib.rs:1923 tmp_drop_name | called by handle_unlink | idx from tmp_find on the path BEING UNLINKED (correct — this is the unlink call itself) + open_fds bitmask (fd-table based) | NO — correctly marks ephemeral when fd still open |
| servers/vfs/src/lib.rs:2139 tmp_release_ephemeral | close-time reclaim | idx param, cross-checked against FD_TABLES by idx | NO |
| servers/vfs/src/lib.rs:3581 resolve_lock_range / LockKey::Tmp | fcntl(F_SETLK) range | LockKey::Tmp(*idx) from fd's VnodeKind | NO |
| servers/vfs/src/lib.rs:3835 F_ADD_SEALS/F_GET_SEALS | fcntl seals | tbl.fds[fd].kind -> idx -> TMP_VMOS[idx] | NO |
| servers/vfs/src/lib.rs:5384/5680/5700/5720/5740 handle_fchown/fsetxattr/fgetxattr/flistxattr/fremovexattr | f*-family (fd-based) | fd -> kind -> idx, then tmp_owner(idx) for target | NO |
| servers/vfs/src/lib.rs:4870 rename target overwrite | rename(2) replacing existing target | idx from tmp_find on the RENAME TARGET path (real path op) + open_fds mask | NO — same pattern as unlink |
| servers/vfs/src/lib.rs:3398 export_fd / :3418 import_fd | SCM_RIGHTS fd passing | TransferFd{kind,..} Copy of sender's VnodeKind::TmpFile{idx} | NO |
| servers/vfs/src/lib.rs:3438 handle_fork_dup | fork() fd inheritance | copies whole FdEntry array (idx preserved) | NO |
| servers/vfs/src/lib.rs:3910-3914 handle_alloc_fd / dup | dup(2)/dup2(2) | copies VnodeKind (Copy), idx preserved | NO |
| servers/vfs/src/lib.rs:6084 handle_fstat | fstat(fd) | fd -> kind -> idx -> TMP_FILES[idx]/tmp_nlink(idx) | NO |
| servers/vfs/src/lib.rs:6003 handle_fstatfs | fstatfs(fd) | pool-wide counters only (MAX_TMP_FILES, used_slots) | NO — unrelated to any single inode's name |
| servers/vfs/src/lib.rs:6335-6390 VFS_FD_PATH (readlink /proc/self/fd/N) | FdInfo::TmpIdx(i) branch | fd -> kind -> idx, then reads tmp[i].path (left in place by tmp_drop_name when it flips ephemeral) | NO functional break — but Linux-fidelity gap: no " (deleted)" suffix appended. See UNSURE #2. |
| servers/vfs/src/lib.rs install_dmabuf_vmo/dmabuf_handle_of (~487-518) | PRIME_HANDLE_TO_FD / PRIME_FD_TO_HANDLE | fd -> tmpfile_owner_of -> idx | NO — already-shipped precedent (commit 36f62d0) unlinking its own ephemeral node immediately |
| servers/vfs/src/lib.rs:4054,4882,4891 getdents64 / rename dup-check | directory listing, dup-name check | `if e.ephemeral { continue; }` filters unnamed entries | NO — CORRECT (unlinked file must not appear in readdir) |
| servers/vfs/src/lib.rs path-only syscalls: handle_rmdir:5262, handle_chmod:5309, handle_chown:5372, handle_setxattr:5588, handle_getxattr:5608, handle_listxattr:5628, handle_removexattr:5649, handle_access:5663 | *(path)* variants e.g. chmod("/tmp/memfd:foo",...) | tmp_find(path) — genuinely BY NAME | These correctly ENOENT after unlink, matching Linux. NOT a break; never issued against an open memfd by path in the observed wl_shm/COSMIC flow. Listed for completeness. |
| servers/vfs/src/lib.rs:2076 handle_open parent-dir resolution | open() parent-dir existence check | tmp_find(path[..comp_end]) on the PARENT dir (always /tmp) | NO |
| mm/src/vmm.rs:385 (doc comment only) | describes the K1 shared-VMO primitive | N/A — mm/vmm.rs has no by-name resolution of its own; consumes the frame list vmo_acquire_frames already resolved by idx | NO |
| "VFS fstat-proxy / mounted-fd close refcount" servers/vfs/src/lib.rs:3170-3184, 6060-6070 | MountedFile refcount-on-close / fstat proxy | keyed on (port, file_id) — an f2fs/mount concept, NOT tmpfs | N/A — does not apply to tmpfs/memfd inodes |

## Must-fix-before-unlink subset

None identified with certainty from static analysis. Every fd-reachable
operation on a TmpFile (ftruncate, K1 mmap, read/write/lseek, close/release,
fcntl seals/locks, fstat, fchown/fxattr, dup/dup2, fork inheritance, SCM_RIGHTS
export/import, dmabuf install) already resolves through
VnodeKind::TmpFile{idx} or the shared tmpfile_owner_of helper, never through
tmp_find(path). The only genuine by-name (tmp_find) call sites are the
path-argument syscalls (chmod/chown/*xattr/rmdir/access) and rename/unlink's
own name-drop bookkeeping — none exercised against an already-open,
since-unlinked memfd fd in the observed COSMIC/wl_shm flow, and all correctly
return ENOENT post-unlink (matching real Linux for a deleted path).

## UNSURE items

1. THE CENTRAL CONTRADICTION. The b3659fa comment's specific technical claim
   ("ftruncate/mmap... resolve the inode by name") does not match what the
   code in this tree actually does (both idx-keyed, corroborated by two
   pre-existing design notes plus the shipped analogous dmabuf fix, commit
   36f62d0). Cannot tell from static reading whether: (a) the comment is
   stale/wrong and a re-attempt would just work; (b) the actual experiment
   unlinked at a different point than "right after mark_memfd" (e.g. before
   mark_memfd ran, or unlinked the un-suffixed path when the :seq EEXIST-retry
   fired, missing the real name) and hit an unrelated bug; or (c) a
   runtime-only hazard (lock ordering, a cached `kind` somewhere not found by
   this audit, or a client-side assumption) is invisible to static analysis.
   NEEDS an instrumented runtime re-test, not a guess.
2. /proc/self/fd/N readlink missing " (deleted)" suffix
   (servers/vfs/src/lib.rs:6383-6389, FdInfo::TmpIdx arm). Not a crash/ENOENT,
   but a real Linux-fidelity gap — some libraries probe for that suffix to
   detect an unlinked-but-mapped file. Unknown whether anything in the
   COSMIC/wl_shm/GBM stack depends on it; flagging rather than guessing.
