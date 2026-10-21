# lane/secmisc 2026-10-19/20

## L7.10: S_ISGID on chmod, tmpfs mount flags
- `xattr::chmod_filter_mode` (servers/xattr/src/lib.rs): a caller that is not root and not
  in the file's group loses S_ISGID on chmod/fchmod (Linux `setattr_prepare`). Used by the
  tmpfs chmod/fchmod in servers/vfs and by f2fs `chmod_inode` (now takes the full Cred).
  FAT: there is no chmod path (the fat server stores no modes), nothing to change.
- tmpfs roots (/tmp, /dev/shm, /run/user) keep nosuid/nodev/noexec flags
  (`TMPFS_ROOT_FLAGS`, `set_tmpfs_root_flags` in servers/vfs/src/lib.rs). `mount -o remount`
  on a root, or `mount -t tmpfs` onto one, sets them (contents kept; tmpfs elsewhere is
  EINVAL as before). exec ignores set-ID on nosuid; execve of a file or `#!` interpreter on
  noexec is EACCES (kernel/src/syscall.rs sys_execve). /proc/mounts and mountinfo list them.
  nodev: tmpfs refuses device nodes already, so it is only recorded and reported.
- Open: mmap(PROT_EXEC) of a file on a noexec tmpfs is not refused (Linux: EPERM). statfs
  of tmpfs does not report ST_NOSUID/ST_NOEXEC.
- privtest: `chmod_nonmember_drops_sgid_{disk,tmpfs}`, `fchmod_nonmember_drops_sgid_*`,
  `chmod_member_keeps_sgid_*`, `chmod_root_keeps_sgid_*`, `tmpfs_*` (remount nosuid/nodev,
  mount noexec, EACCES, EINVAL elsewhere, clear).

## L7.7: password hashes, dev passwords over the network
- `lib/shadowhash/shadowhash.rs`: core-only SHA-512, sha512-crypt (`$6$[rounds=N$]salt$`),
  legacy `$sha256$salt$hex` still verifies; constant-time compare. login and sshd include
  it with `#[path]`; pam-leandros (greetd/sudo/su) has a copy in its src/ because the port
  is built from a copied tree (ports/sudo-rs/.work). Keep the two identical.
- Image: scripts/mkfs-f2fs-populated.py writes `$6$rounds=65536$...` (hashlib only).
- sshd: `DevPasswordAuthentication no|local|yes` (default local). A password equal to the
  user name (root/root, leandro/leandro) is accepted only from loopback and 10.0.2.0/24
  (QEMU user net, where sshsmoke's hostfwd arrives). Root password login was already
  refused (prohibit-password). The laptop workflow uses keys (/leandros/authorized_keys),
  unaffected. Under socket_vmnet a password sshsmoke run from the host LAN address is
  refused: use a key or set `DevPasswordAuthentication yes` there.

## L5.8 (rest): tmpfs TMP_FILES 4096 - OPEN, not started
Facts for the next lane: `MAX_TMP_FILES = 4096` in servers/vfs/src/tmppool.rs:37; the
pool is a struct of fixed arrays indexed by u16 (`e`, `xattr`, `parent/first/next/prev`,
`hash`, `phash`, `nalias`, `used` bitmap, `IDX_SLOTS = 2x` open-addressed index). lib.rs
has parallel fixed arrays sized by it: TMP_VMOS (532), TMP_INFLIGHT (5010), TMP_FDREFS
(5017), TmpMask (5071), plus ~28 `MAX_TMP_FILES` uses. Follow lane/pools (2037673e):
turn the arrays into Vecs grown in chunks under the existing lock, rehash the index on
growth, keep u16 links (raise to u32 if the cap goes past 65535), grow TMP_FDREFS as a
chunked array of atomics so the lock-free readers stay valid.

## Tests (aarch64, HVF)
See the commit messages for the final privtest/regress results.
