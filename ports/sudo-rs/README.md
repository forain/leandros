# sudo-rs (LeandrOS port): `su` and `sudo`

Upstream: <https://github.com/trifectatechfoundation/sudo-rs>, Apache-2.0 OR MIT,
pinned to v0.2.15 (`e4cfa9cf`). The memory-safe Rust reimplementation of sudo and
su that Debian/Ubuntu ship; it uses PAM for authentication and reads a standard
`/etc/sudoers`. `build.sh <arch>` fetches the pinned commit into `.work/`, applies
`patches/`, and builds the `sudo` and `su` binaries static-musl (ET_EXEC) into
`out/<arch>/`. `scripts/mkfs-f2fs-populated.py` installs both as `/bin/sudo` and
`/bin/su`, set-user-ID root (04755).

## The LeandrOS deltas

**PAM is `pam-leandros`** (`patches/0001-pam-leandros.patch`). There is no Linux-PAM
here; `ports/greetd/pam-leandros` implements the libpam application ABI against
`/etc/shadow` (the same `$sha256$salt$hex` scheme `/bin/login` verifies) and is
linked in as an rlib, exactly as greetd links it. sudo-rs's `#[link(name = "pam")]`
is satisfied by an empty `libpam.a` the build puts on the link path. The patch also
adds an empty `[workspace]` so cargo does not treat the tree as a member of the
LeandrOS repo-root workspace.

**A pty follower has no chownable node** (`patches/0002-pty-chown-optional.patch`).
With `use_pty` (the default) sudo and su run the command on a fresh pty and
`chown("/dev/pts/<n>")` it to the target user. The VFS hands pty pairs out by
number from `/dev/ptmx` and keeps no owner for them, so that chown is ENOENT and
every command failed with "cannot execute". The failure is now a dev log line:
the pty is private to the sudo that opened it either way.

**`/dev/tty` must be a terminal** (`patches/0003-dev-tty-must-be-a-tty.patch`).
Linux answers `open("/dev/tty")` with ENXIO in a session without a controlling
terminal; the LeandrOS VFS hands out a proxy of the caller's stdin instead, so
with stdin a pipe sudo-rs "had a terminal" whose every termios call was ENOTTY.
`UserTerm::open` now reports ENXIO unless the descriptor is a tty, and sudo-rs
falls back to running the command without a pty, as it does on Linux.

**`sudo -E`** (`patches/0004-preserve-env-flag.patch`). Upstream ignores a bare
`-E` with a warning. Here it requests every variable of the caller's environment
exactly as `VAR=value` arguments would, so the policy decides as ogsudo does:
allowed with SETENV (which `ALL` implies), refused otherwise. `PATH` stays
`secure_path`'s and `SUDO_*` stay sudo's.

What a distribution's `/etc/pam.d/{su,su-l,sudo}` stack would do is in
pam-leandros, keyed on the service name:

- `pam_rootok` for `su`/`su-l`: a caller whose real uid is 0 switches without a
  password. (sudo exempts root itself, in its policy code.)
- `pam_systemd`'s `XDG_RUNTIME_DIR` for `su`/`su-l`/`sudo`/`sudo-i`:
  `pam_open_session` exports `XDG_RUNTIME_DIR=/run/user/<uid of the target>` when
  init has created that directory. su and sudo merge the PAM environment into the
  command's, so `su leandro -c firefox` and `sudo -u leandro firefox` reach the
  user's Wayland and PipeWire sockets, not root's unreadable `/run/user/0`.
  greetd's services are untouched.

## Configuration

- `/etc/sudoers` (0440 root, from `data/sudoers`): `Defaults env_reset`, a `secure_path`, `env_keep`
  adding `WAYLAND_DISPLAY` and the session/locale variables (sudo-rs keeps
  `DISPLAY`, `XAUTHORITY`, `XDG_CURRENT_DESKTOP` by default),
  `timestamp_timeout=5`, `root ALL=(ALL:ALL) ALL`, `%wheel ALL=(ALL:ALL) ALL`.
- `wheel` is gid 10; `leandro` is a member (alongside audio, video, input, netdev).
- `/etc/shells` (`data/shells`) lists `/bin/sh` and `/bin/brush` (`su -s` checks it for non-root
  callers).

## Credential cache

sudo-rs keeps per-user records in `/run/user/.sudo-rs/ts/<uid>`
(`patches/0005-timestamps-on-tmpfs.patch`: upstream's `/var/run` is plain f2fs
here and would carry records across a reboot; `/run/user` is the tmpfs root,
0755 root). A record is scoped to the controlling tty's session leader when
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
