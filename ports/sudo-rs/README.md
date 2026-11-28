# sudo-rs (LeandrOS port): `su` and `sudo`

Upstream: <https://github.com/trifectatechfoundation/sudo-rs>, Apache-2.0 OR MIT,
pinned to v0.2.15 (`e4cfa9cf`). The memory-safe Rust reimplementation of sudo and
su that Debian/Ubuntu ship; it uses PAM for authentication and reads a standard
`/etc/sudoers`. `build.sh <arch>` fetches the pinned commit into `.work/`, applies
`patches/`, and builds the `sudo` and `su` binaries static-musl (ET_EXEC) into
`out/<arch>/`. `scripts/mkfs-f2fs-populated.py` installs both as `/bin/sudo` and
`/bin/su`, set-user-ID root (04755).

## The LeandrOS deltas

**PAM is Alpine's real Linux-PAM.** sudo-rs's `#[link(name = "pam")]` resolves to
the locked `linux-pam` `libpam.so.0` (`scripts/lib/realpam.sh` puts it on the link
path), so `sudo` and `su` are dynamic musl PIEs (`PT_INTERP`
`/lib/ld-musl-<arch>.so.1`) and libpam can `dlopen` `pam_unix`, which verifies
passwords against `/etc/shadow` through the setuid `unix_chkpwd`. The only patch
to the PAM side is an empty `[workspace]` (`patches/0001-standalone-workspace.patch`)
so cargo does not treat the tree as a member of the LeandrOS repo-root workspace.
The binaries are installed set-user-ID root (04755); they are no longer static.

**A pty follower is chownable** with no patch: the VFS's devpts gives `/dev/pts/<n>` the
opener's uid, group tty (5), mode 0620, and chown/chmod/stat work on it.

**`/dev/tty` without a controlling terminal** needs no patch any more. The VFS
used to answer `open("/dev/tty")` in a session with no controlling terminal with
a proxy of the caller's stdin, so with stdin a pipe sudo-rs "had a terminal" whose
every termios call was ENOTTY, and a patch made `UserTerm::open` report ENXIO.
Since lane procfix (2026-10-18) the VFS answers ENXIO itself, as Linux does, and
sudo-rs falls back to running the command without a pty on its own.

**`sudo -E`** (`patches/0004-preserve-env-flag.patch`). Upstream ignores a bare
`-E` with a warning. Here it requests every variable of the caller's environment
exactly as `VAR=value` arguments would, so the policy decides as ogsudo does:
allowed with SETENV (which `ALL` implies), refused otherwise. `PATH` stays
`secure_path`'s and `SUDO_*` stay sudo's.

The service stacks are ordinary PAM files, shipped from `ports/pam-rundir/data/`
as `/etc/pam.d/{sudo,sudo-i,su,su-l}`:

- `su`/`su-l` start with `auth sufficient pam_rootok.so`: a caller whose real uid
  is 0 switches without a password. (sudo exempts root in its policy code.)
- All four include Alpine's `base-*` stacks, then `-session optional
  pam_rundir.so` (`ports/pam-rundir`), which exports
  `XDG_RUNTIME_DIR=/run/user/<uid of the target>` when init has created that
  directory. su and sudo merge the PAM environment into the command's, so
  `su leandro -c firefox` reaches the user's Wayland and PipeWire sockets, not
  root's.

## Configuration

- `/etc/sudoers` (0440 root, from `data/sudoers`): `Defaults env_reset`, a `secure_path`, `env_keep`
  adding `WAYLAND_DISPLAY` and the session/locale variables (sudo-rs keeps
  `DISPLAY`, `XAUTHORITY`, `XDG_CURRENT_DESKTOP` by default),
  `timestamp_timeout=5`, `root ALL=(ALL:ALL) ALL`, `%wheel ALL=(ALL:ALL) ALL`.
- `wheel` is gid 10; `leandro` is a member (alongside audio, video, input, netdev).
- `/etc/shells` (`data/shells`) lists `/bin/sh` and `/bin/brush` (`su -s` checks it for non-root
  callers).

## Credential cache

sudo-rs keeps per-user records in upstream's `/var/run/sudo-rs/ts/<uid>`, i.e.
`/run/sudo-rs/ts` (`/var/run` -> `/run`, a tmpfs, so records never survive a reboot). A record is scoped to the controlling tty's session leader when
`/proc/self/stat` names a tty, and to the parent process otherwise. The VFS
reports `tty_nr` 0, so the scope is always the parent -- the shell sudo was
typed in -- identified by pid **and** `starttime`. `starttime` used to be the
current uptime rather than the process's creation time; it is now the
thread-group leader's creation time (`Task::start_ns`), which is what keeps a
later process that reuses the pid from inheriting the record.

## Tests

`/bin/sudotest` (userland/sudotest, in `runtests.py --suite regress`) runs as root
and drives the real binaries as root, as `leandro` (wheel) and as `cosmic-greeter`
(not wheel): identity and supplementary groups, `su -`/`-l` login environment, the
target user's password for su, sudo's env_reset/secure_path/`-E`/`-i`/`-s`,
`-S` with right and wrong passwords, `-n` with and without a cached credential,
`-K`, `-l`, denial of a non-wheel user, and the installed modes.
