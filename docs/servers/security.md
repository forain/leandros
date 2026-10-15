# Users, credentials and permissions

LeandrOS runs real multi-user sessions: accounts in `/etc/passwd`, `/etc/group` and a hashed `/etc/shadow`, logins that drop privilege with real `setresuid`/`setresgid`/`setgroups`, filesystem permissions and POSIX ACLs enforced on every path operation through one evaluator, and Linux's privilege rules on signals, process groups, devices, ports and system calls. This page describes that model and, just as importantly, where it is still thinner than Linux.

## Accounts

The image builder ([`scripts/mkfs-f2fs-populated.py`](../../scripts/mkfs-f2fs-populated.py)) seeds:

| Account | uid:gid | Home | Shell | Password | Supplementary groups |
|---|---|---|---|---|---|
| `root` | 0:0 | `/root` | `/bin/brush` | `root` | — |
| `leandro` | 1000:1000 | `/home/leandro` | `/bin/brush` | `leandro` | `audio` (29), `video` (44), `input` (104) |
| `cosmic-greeter` | 990:990 | `/home/cosmic-greeter` | `/bin/false` | none (cannot log in) | `video`, `input` |

These are development credentials, well known by design. `cosmic-greeter` is a system account below `UID_MIN` so the greeter does not offer it as a login choice, and its `/bin/false` shell excludes it a second, independent way. Group numbers follow Debian's.

### /etc/shadow

The hash format is `$sha256$<salt>$<hex>`, where `<hex>` is the lowercase SHA-256 of the salt bytes followed by the password bytes. It is verified by `/bin/login`'s embedded SHA-256 ([`userland/login`](../../userland/login), no libc `crypt`) and by `pam-leandros` for the graphical login, and produced by Python's `hashlib` in the image builder with a fixed per-user salt. The three must stay byte-for-byte in sync; `pam-leandros` has a host test asserting root's field is identical to what `shadow_hash` writes. `/etc/shadow` is mode `0600`. There is no `passwd(1)`.

## Logging in

Three login paths exist, all supervised and respawned by PID 1 ([`userland/init`](../../userland/init)); [../getting-started/logging-in.md](../getting-started/logging-in.md) describes them from the user's side.

- **Serial getty** — `/bin/login` on the console in a fresh session. It reads `/etc/passwd` and `/etc/shadow`, verifies the password, parses `/etc/group` member lists and calls `setgroups` (initgroups without a libc), then `setresgid` and `setresuid`, and execs the account's shell. Every interactive harness logs in this way.
- **Text VTs** — the same `/bin/login` on `/dev/tty2..6` ([tty-and-vt.md](tty-and-vt.md#virtual-terminals)), with `setsid` and `TIOCSCTTY`; `login` does not echo the username itself on a pty, or it would appear twice.
- **Graphical login** (the default) — `greetd` runs `cosmic-comp` in kiosk mode **as root**, whose child `/bin/greeter-launch` looks up `cosmic-greeter`, `setgroups`/`setresgid`/`setresuid`s to it, **reads `getuid()` back** to prove the drop, and execs `cosmic-greeter`. Authentication crosses greetd's IPC to its session worker, which uses `pam-leandros` (the libpam application ABI backed by `/etc/shadow`) and starts the session entirely as the user. A launcher that reports a drop it did not perform would produce a login screen indistinguishable from a correct one, which is why the uid is checked. Details: [../desktop/login.md](../desktop/login.md).

`touch /etc/leandros/text-login` disables the graphical login.

**SSH** is a fourth way in. `/usr/sbin/sshd` ([../userland/programs.md](../userland/programs.md#sshd)) listens on `0.0.0.0:22` from boot.
- It checks passwords against the same `/etc/shadow` hash as `/bin/login`, and keys against `~/.ssh/authorized_keys`.
- It allows root to log in with a key but not with a password.
- Its children drop privilege the way `login` does (setgroups, then setresgid, then setresuid) and read the ids back.
- The development passwords above are therefore reachable from the network wherever the VM's port 22 is. With SLIRP that is only the host's loopback, but with `socket_vmnet` or on real hardware it is the LAN. Change them, or disable sshd with `touch /etc/ssh/sshd_not_to_be_run`, before exposing a machine.

## Credentials in the kernel

- Each task carries real, effective and saved uid and gid plus up to 32 supplementary groups. All are copied by `fork` and `clone` and survive `execve`. Any new per-task security property must be added both to the task constructors and to the inheritance paths in `sched/src/clone.rs` (`fork_current`, `clone_thread`); missing the latter lets every `fork()` escape.
- `setresuid`/`setresgid` are real (relibc's `setuid()` routes through them — they were once accept-and-ignore, so libc `setuid` silently kept root), as are `setreuid`/`setregid` with Linux's rules, `setuid`/`setgid`, `getres*id`, and `setgroups` (root only → `EPERM`; more than 32 → `EINVAL`) / `getgroups`.
- `umask` is applied kernel-side in `openat`/`mkdirat` and inherited across `fork`. When a parent directory has a default ACL the umask is *not* applied (Linux `posix_acl_create`), so it travels with the create request and each filesystem applies it through the shared `xattr::acl_create`.
- There are no setuid binaries and no capability sets: privilege is "euid 0" (`capget` reports bits 0..40 for euid 0 and none otherwise; a root `capset` drop is accepted but has no effect).

## Filesystem permission enforcement

One evaluator decides every filesystem permission question: `xattr::may_access` / `xattr::access_check` in [`servers/xattr`](../../servers/xattr) — mode bits, the stored POSIX ACL (named users and groups, mask), supplementary groups, and the root bypass (which still requires at least one execute bit to execute). The VFS (tmpfs) and f2fs call it at every site, with credentials built by one constructor (`vfs_server::cred_of`). Because there is one evaluator, ACLs apply automatically wherever a new check is added.

| Operation | Check |
|---|---|
| Path traversal | search (`MAY_EXEC`) on every directory component, **before** the lookup, so an unsearchable directory answers `EACCES` whether or not the leaf exists (no existence leak through `ENOENT`); `ENOTDIR` for a non-directory mid-path |
| `open` | read/write per the access mode; `O_TRUNC` needs write even on an `O_RDONLY` open |
| Create (`open(O_CREAT)`, `mkdir`, `mknod`, `symlink`, `link`, AF_UNIX `bind`) | write + search on the parent, *after* the `EEXIST` probe (so `mkdir` of an existing name is `EEXIST`, not `EACCES`, as on Linux) |
| Remove (`unlink`, `rmdir`, `rename`) | write + search on each parent; the sticky rule (`EPERM` unless the caller owns the entry or the directory); write on a directory that changes parents |
| AF_UNIX `connect` | write on the socket inode |
| `chmod` | owner or root |
| `chown` | uid change root-only; gid only to a group the caller belongs to |
| `utimensat` / `utimes` / `futimesat` / `utime` | explicit times need ownership or root (`EPERM`); "now" needs write or ownership (`EACCES`) |
| `execve` | the execute bit on the final ELF or interpreter, not only on `#!` scripts (a `0644` ELF used to run for anyone) |
| `access` / `faccessat` | the **real** ids, unless `AT_EACCESS` asks for the effective ones |
| `link` to another user's file | refused unless the caller could read and write it (`protected_hardlinks`), on tmpfs and f2fs |
| `chmod`/`chown` of a tmpfs mount root | `EPERM` (root: a no-op only if unchanged) |

Device nodes follow udev's defaults: block devices and `/dev/loop-control` `root:disk 0660`; evdev `root:input 0660`; the DRM primary node and `/dev/fb0` `root:video 0660`; `/dev/pipewire` `root:audio 0660`; render nodes `0666`. Before this, uid 1000 could read `/dev/vdb` raw.

### Why it is built this way

- **One evaluator, not ad-hoc mode checks.** Until 2026-09-16 the only permission check in the system was `open(2)` of an inode that already existed: the path walk read no mode on any component, creating or removing an entry checked only that the parent existed, and AF_UNIX `connect` discarded the caller. Any uid could create files inside a `0700` root directory and connect to root's sockets. A measurement enumerated every call site of the evaluator: there were five, and each took a fully resolved existing inode — none took a parent directory. The fix put the gates into the two path walks and the create/remove paths, all through `may_access`.
- **Checks in the backend walk.** Only the backend sees inodes, and it already receives the caller's pid with every request (synchronous dispatch), so f2fs keeps the caller's credentials on its per-mount state for the duration of a request and the walk needed no new signatures.
- **The greeter survived by staging, not by holes.** The greeter's drop had depended on the missing checks: it reached the compositor's `wayland-1` under a `0700` root `/run/user/0`, and greetd's root-owned control socket. When enforcement landed, the greeter phase moved into the greeter account's own runtime directory, `/run/user/990` (init seeds `/run/user/<uid>`, `0700`, owned by the account, for every passwd account, because `/run/user` is `0755 root`), and `greeter-launch`, still root, chowns both socket nodes to the account before dropping — upstream's ownership arrangement reached without unprivileging the compositor. A suspected "greeter login fails, compositor deaf" regression during that work was refuted: it was a serial-capture artefact of the test harness.

## Process and system privilege

Measured against a Linux host with the same test program (`userland/privtest`, which runs as a user and, as root, forks root-owned targets and then drops to uid 1000): the unprivileged results are identical to Linux's.

- **Signals**: `kill`, `tkill`, `tgkill`, `rt_sigqueueinfo`, `rt_tgsigqueueinfo` to another uid's process are `EPERM`, with Linux's `SIGCONT`-within-session exemption; `kill(-pgrp)` signals only the permitted members of a mixed group; `kill(-1)` follows `kill_something_info`; a forged `si_code` to another process is `EPERM`.
- **Process groups and sessions**: `setpgid` with Linux's checks (`ESRCH` for a non-child, `EPERM` across sessions or for a session leader); `getsid(pid)` reports the target's session.
- **Scheduling and limits**: `setpriority` on another uid is `EPERM`, lowering nice is `EACCES` (`RLIMIT_NICE` defaults to 0); `sched_setscheduler` to FIFO/RR needs `RLIMIT_RTPRIO` or root; `sched_setaffinity` on another uid is `EPERM`; `setrlimit`/`prlimit` store per-process limits (fork-inherited, kept over exec), refuse raising a hard limit, and refuse touching another uid's limits.
- **System calls reserved to root**: `mount`, `umount2`, `pivot_root`, `chroot`, `sethostname`, `setdomainname`, `settimeofday`/`clock_settime`, `reboot`, `acct`, `vhangup`, `swapon`/`swapoff`, module and `kexec` loading, `iopl`/`ioperm` (x86-64) are `EPERM` for others.
- **Network**: binding a port below 1024 is `EACCES`; `SOCK_RAW` is `EPERM` (ping uses an ICMP datagram socket).
- **`/proc/<pid>/status`** reports real `Uid`/`Gid` quadruples (it used to say `0 0 0 0` for everything), which is how a COSMIC session was proven to run all its components at uid 1000.

### chroot

`chroot(2)` is real confinement, not a path prefix. The kernel resolver anchors absolute paths at the jail root and clamps `..` at it; f2fs and tmpfs re-anchor absolute symlink targets at the jail root; dirfd paths are translated back into the jail's view. A symlink inside a jail pointing above it (`chroot /home/jail`, link → `/above`) stays inside. See [vfs.md](vfs.md#path-resolution).

### Power-off authorisation

`init` accepts `poweroff`/`reboot`/`halt` requests on a stream socket at `/run/user/initctl` (sockets only live on tmpfs, and `/run/user` is the tmpfs only root may create entries in, so nobody can squat the name). The request is authorised from the peer's `SO_PEERCRED`: root always, anyone else only if the requesting process belongs to a local session init supervises (the graphical login, the serial login, or a text VT login) — logind's default `allow_active=yes` policy. The session's `login1` service and `/bin/poweroff`/`/bin/reboot`/`/bin/halt` ([`userland/poweroff`](../../userland/poweroff)) forward to it; the actual `reboot(2)` remains root-only.

## The privilege boundary of the desktop

As it stands:

- **The greeter's compositor runs as root.** `libseat` is a shim with no seatd behind it, and greetd's `default_session.user` applies to the whole greeter command, which is why the boundary is a separate binary (`greeter-launch`) rather than a configuration line. The greeter client itself is uid 990.
- **The user's session runs entirely as the user**, including its own `cosmic-comp`, inside `/run/user/<uid>`; it reaches the DRM and input nodes through `video`/`input` group membership.
- Permission enforcement on traversal, creation, removal and AF_UNIX `connect` is in place, so the greeter can no longer reach root's runtime directory or root-owned sockets.

The project's broader list of desktop limitations lives in [../desktop/limitations.md](../desktop/limitations.md).

## Open gaps

| Gap | Notes |
|---|---|
| No real PAM stack | `pam-leandros` implements the application ABI greetd needs (including the environment table, which *is* greetd's session `envp`); `open_session`/`setcred` are stubs, which is correct for greetd. Whether to keep extending it or port Linux-PAM (which `dlopen`s its modules) is undecided. |
| No utmpx | Nothing writes `utmp`/`wtmp`; no shipped program is known to read them. |
| Private IPC syscalls unchecked | `SYS_IPC_SEND` to any port id, `SYS_SPAWN` and the debug serial-write syscall carry no credential checks. |
| No capabilities | Root is all-or-nothing; `capset` drops are accepted and ignored. |
| rlimits stored, mostly not enforced | Only `RLIMIT_NICE` and `RLIMIT_RTPRIO` act; all others default to unlimited. |
| `protected_regular` / `protected_fifos` | `O_CREAT` on another user's file in sticky `/tmp` is not restricted. |
| `setsid()` by a group leader | Succeeds (Linux: `EPERM`); left because of session-start risk, with no privilege gain. |
| BLKPG and loop ioctls | Gated only by the device open; Linux also wants `CAP_SYS_ADMIN` for BLKPG. |
| `mknod(S_IFSOCK)` | `EPERM` for everyone; Linux allows it unprivileged. |
| Password hashing | A single salted SHA-256 pass, not a slow password-hashing scheme; no `passwd(1)` to change passwords. |
| Keyboard ioctls | VT keymap writes are root-only; Linux also lets the VT's controlling user write them. |
| No `ptrace` | `ENOSYS`, so there is no hole there. |

## Code and tests

- [`servers/xattr/src/lib.rs`](../../servers/xattr/src/lib.rs) — `Cred`, `may_access`, `access_check`, `sticky_denies`, `hardlink_denied`, `acl_create`.
- [`servers/vfs/src/lib.rs`](../../servers/vfs/src/lib.rs) — `cred_of`/`real_cred_of`, tmpfs gates, device node ownership (`dyn_dev_meta`, `blockdev_meta`), `unix_bind_node`/connect.
- [`servers/f2fs/src/lib.rs`](../../servers/f2fs/src/lib.rs) — `resolve_path_ex` traversal checks and the create/remove gates.
- [`kernel/src/syscall.rs`](../../kernel/src/syscall.rs) — credential syscalls, `exec_permitted`, `sys_faccessat`, `sys_utimensat`, the privileged-syscall checks; [`sched/src`](../../sched/src) — per-task credentials, groups, rlimits, signal permission.
- [`userland/login`](../../userland/login), [`userland/greeter-launch`](../../userland/greeter-launch), [`userland/init`](../../userland/init), [`ports/greetd`](../../ports/greetd).
- Tests: `userland/permtest` (traversal, creation, removal, sticky, AF_UNIX, ownership, supplementary groups, exec bit, `utimensat`, default ACLs — on f2fs and tmpfs), `userland/privtest` (the process and system privilege matrix, as root and as a user; also builds and runs on a Linux host for comparison), `userland/vfstest` (`access_real_vs_effective`, chroot confinement), `userland/nettest` (raw-socket `EPERM` as a user).

## See also

- [filesystems.md](filesystems.md#extended-attributes-and-posix-acls) — the xattr/ACL contract.
- [vfs.md](vfs.md) — where the checks sit in path resolution.
- [../desktop/login.md](../desktop/login.md) — greetd, the greeter, `pam-leandros`.
- [../desktop/limitations.md](../desktop/limitations.md) — what the desktop still cannot do.
- [../kernel/processes.md](../kernel/processes.md) — credentials, chroot and priorities in the process model.
