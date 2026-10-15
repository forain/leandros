# Programs

What is in `/bin` and where it comes from: the in-tree programs under
[userland/](../../userland/), the shells, the ported utilities, and the larger applications.
The regression-test crates that also live in `userland/` are documented with the test suites
in [../development/testing.md](../development/testing.md); they are summarised at the end of
this page.

## In-tree programs

| Program | Description |
|---|---|
| `init` | PID 1; mounts the root filesystem, supervises the graphical login, the serial getty and the VT 2..6 logins, and handles power-off ([below](#init)) |
| `login` | Authenticates against `/etc/shadow`, sets supplementary groups, drops privilege via `setresgid`/`setresuid`, sets `XDG_RUNTIME_DIR`, execs the account's shell |
| `greeter-launch` | `cosmic-comp`'s kiosk child: becomes the `cosmic-greeter` account, verifies the drop by reading `getuid()` back, execs the greeter ([../desktop/login.md](../desktop/login.md)) |
| `poweroff` / `reboot` / `halt` | One binary dispatching on `argv[0]` (hardlinked); asks init for an orderly shutdown over `/run/user/initctl`; `-f` does `sync` + `reboot(2)` directly (root only), and is the fallback when init is unreachable |
| `meminfo` | One-line `sysinfo` sample of buddy-allocator total/free, so a session script can read a series rather than a point |
| `shell` | The original built-in CLI with Unix-style commands and VFS integration, on leandros-libc |
| `aplay` | Command-line audio player (WAV, MIDI, test tone); claims the single-writer audio device, so it answers `EBUSY` while a PipeWire session holds it ([../audio.md](../audio.md)) |
| `mount` / `umount` / `fstab` | Filesystem mount management; `mount` with no arguments lists the live VFS mount table |
| `lsblk` / `lspci` / `lsusb` | Device enumeration with real block-device capacity from the kernel's block registry; `lsusb --summary` is what init prints above the login prompt |
| `ping` | ICMP echo over the smoltcp stack: a raw socket as root, the unprivileged ICMP "ping socket" (`SOCK_DGRAM`/`IPPROTO_ICMP`) otherwise |
| `loadkmap` | Loads a console keymap (kbd's `loadkeys -b` format) into the kernel with `KDSKBENT`; init runs it for `KEYMAP=` in `/etc/vconsole.conf` ([ports/kbd-keymaps](../../ports/kbd-keymaps/README.md)) |
| `xattr-util` | Get/set/list/remove extended attributes and POSIX ACLs |
| `tput` | Terminal capability query |
| `dbusprobe` | A raw-wire D-Bus client for `ListActivatableNames`, `StartServiceByName`, implicit activation and busd's `ServiceUnknown` fast path — there is no `busctl`/`dbus-send` in the image |
| `hello` | Minimal "Hello, world!" demonstration of leandros-libc |
| `sshd` | The SSH server, `/usr/sbin/sshd`, built on [russh](https://github.com/warp-tech/russh) (std + tokio, static musl). Started by init, listens on `0.0.0.0:22` ([below](#sshd)) |

Most link `leandros-libc` and are static `ET_EXEC` images ([libc.md](libc.md)).

### init

[userland/init](../../userland/init/src/main.rs) is a `no_std` program on leandros-libc. At
boot it mounts the f2fs root, `pivot_root`s into it, mounts `/etc/fstab`, applies
`/etc/vconsole.conf`, seeds `/run/user/<uid>` (0700, owned by the account) for every account
in `/etc/passwd` plus `/run/cosmic-greeter`, and listens on `/run/user/initctl`. Then it
supervises:

- **the graphical login**: `/bin/sh /bin/greeter-real` (greetd and the COSMIC chain), unless
  `/etc/leandros/text-login` exists or `/bin/greetd` is absent. Its stdout/stderr go through a
  pipe to a logger child that writes `/var/log/greetd.log` and rotates it at 8 MiB (a
  `cosmic-panel` once spun on a dead Wayland connection and wrote 8.4 M lines, over a gigabyte,
  before the guest died). Respawns back off 3, 6, 12, 24, then 30 s, drop back to 3 s once a
  chain has stayed up for 60 s, and stop after 20 consecutive short-lived runs. Exit 78 from
  the launcher means "no GPU renderer": init says so once and stops respawning;
- **the serial getty** on `ttyS0`;
- **text logins on `/dev/tty2`..`/dev/tty6`**, each in its own session with the VT as
  controlling terminal, respawned like agetty, with a quick-failure cap so an unopenable node
  cannot become a fork storm.

A **memory-pressure guard** samples `MemAvailable` (every 2 s, every 250 ms when close to the
floor of max(96 MiB, RAM/10) or falling fast) and, when it stays below the floor, `SIGKILL`s
the process with the largest resident set on the graphical side — never init, the serial
session or the logger, and the compositor or greetd's worker only as a last resort — as the
kernel OOM killer and systemd-oomd would. After three victims in one episode it kills greetd,
whose exit sweeps the tree and respawns the login under the usual backoff. This contains, rather
than fixes, an unbounded output queue in cosmic-session's launch-pad.

**Power-off** requests arrive as one line (`poweroff|reboot|halt`) on `/run/user/initctl` and are
authorised from `SO_PEERCRED` ([../desktop/session.md](../desktop/session.md#shut-down-and-restart)).

### sshd

[userland/sshd](../../userland/sshd/src/main.rs) is the SSH server: russh 0.64 (ring
crypto) on tokio, one root process for every connection.

- **Start.** init starts it at boot when `/usr/sbin/sshd` exists, unless
  `/etc/ssh/sshd_not_to_be_run` exists or the kernel command line has `sshd=0`. It runs in
  its own session; init restarts it with a 1, 2, 4, 8, 16 s backoff and gives up after five
  exits within 60 s. It logs to `/var/log/sshd.log`.
- **Listening.** `0.0.0.0:22`. It also tries `[::]:22`, which fails with `EAFNOSUPPORT`
  because there is no IPv6 yet; it logs that and goes on.
- **Host key.** An ed25519 key is generated on first boot into
  `/etc/ssh/ssh_host_ed25519_key` (0600, plus `.pub`) and reused afterwards.
- **Configuration.** `/etc/ssh/sshd_config` uses an OpenSSH keyword subset: `Port`,
  `ListenAddress`, `HostKey`, `PermitRootLogin`, `PasswordAuthentication`,
  `PubkeyAuthentication`, `PermitEmptyPasswords no`, `AuthorizedKeysFile`, `MaxAuthTries`,
  `LoginGraceTime`, `Subsystem sftp`. `sshd -t` checks the file and the key.
- **Authentication.** Passwords are checked against `/etc/shadow` in the system's
  `$sha256$salt$hex` format, as `/bin/login` checks them. Public keys come from
  `~/.ssh/authorized_keys`, with OpenSSH's StrictModes ownership and mode checks; lines with
  options are skipped. root may log in with a key but not with a password
  (`PermitRootLogin prohibit-password`). Empty passwords, locked (`!`) accounts and
  `false`/`nologin` shells are refused.
- **Sessions.** Each session is a child process. Between fork and exec the child calls
  `setsid`, then (with a pty) `TIOCSCTTY`, then `setgroups`, `setresgid` and `setresuid`.
  It reads the ids back and checks that `setuid(0)` fails before it execs.
  - A shell gets a `/dev/ptmx` pty. It runs as a non-login shell with `argv[0]` set to its
    path, like `/bin/login`.
  - An exec request runs `shell -c cmd` on pipes, with stderr sent as extended data 1.
  - `sftp` runs Alpine's `/usr/lib/ssh/sftp-server` (from [ports/git](ports.md#port-index)).
    `scp` works through it.
  - The environment is `HOME`, `USER`, `LOGNAME`, `SHELL`, `PATH` (with `/usr/bin` first),
    `XDG_RUNTIME_DIR`, `SSH_CLIENT`, `SSH_CONNECTION`, `SSH_TTY`, `TERM`, and the locale
    variables the client sends.
  - It also handles window-change, signals, exit-status and exit-signal.
  - When a client disconnects, the session's process group gets `SIGHUP`.

### login and accounts

`mkfs-f2fs-populated.py` provisions two accounts, both with `/bin/brush` as shell:

| User | Password | uid | Home | Groups |
|---|---|---|---|---|
| `root` | `root` | 0 | `/root` | — |
| `leandro` | `leandro` | 1000 | `/home/leandro` | `audio`, `video`, `input` |

plus the system account `cosmic-greeter` (uid 990, shell `/bin/false`, in `video` and
`input`). The `/etc/shadow` format is LeandrOS's own `$sha256$<salt>$<hex>`, the SHA-256 of
salt followed by password, verified by an embedded SHA-256 in
[userland/login](../../userland/login/src/main.rs) and by `pam-leandros`; there is no
`crypt(3)`. The image builder computes the same hash with Python's `hashlib`, and the two must
stay byte-for-byte in sync (a `pam-leandros` host test asserts it). There is no `passwd(1)` to
change a password.

`login` reads the username and password (without echoing itself on a pty, where the line
discipline already echoes), sets the account's supplementary groups before dropping
(`setgroups` needs root), then `setresgid` and `setresuid`, and exports `XDG_RUNTIME_DIR` as
`pam_systemd` would. More on credentials and permission enforcement:
[../servers/security.md](../servers/security.md).

## Shells

### brush

[brush](https://github.com/reubeno/brush) is the bash-compatible Rust shell and the interactive
default: `/bin/brush`, hardlinked as `/bin/sh`, the login shell of every account, and the
interpreter for every launcher script. It has working job control (`^Z`, `fg`, `bg`, `jobs`,
`kill %n`, `cmd &` with a real `$!`). It is upstream `e46b4ae` plus four LeandrOS patches
([ports.md](ports.md#brush)). Two brush behaviours shape every script in the tree: its exec
search does not fall through `PATH` entries (`/usr/bin` → `/bin`), so scripts name binaries by
absolute path; and a background job whose first command is a builtin or a loop has no pid, as
brush runs it in-process where bash would fork.

Getting brush to run unmodified was the first large Linux-compatibility push in the kernel:
edge-triggered epoll emulation, process-directed signals, real `wait4`, `CLONE_VFORK`, AF_UNIX
semantics, stdio proxy fds, TTY job control, and a crossterm cursor-position handshake over
serial that needed ANSI sequences returned whole by `read(2)`. Those are documented where they
live ([../kernel/signals.md](../kernel/signals.md), [../kernel/event-loop.md](../kernel/event-loop.md),
[../servers/tty-and-vt.md](../servers/tty-and-vt.md)).

### shell

The original `userland/shell` predates brush and is kept as a minimal fallback on
leandros-libc.

## Ported utilities

| Program | Source | Notes |
|---|---|---|
| coreutils (`ls`, `cat`, `cp`, `mv`, `rm`, `id`, `env`, …) | [uutils/coreutils](https://github.com/uutils/coreutils), sibling `../coreutils` | One static musl multicall binary built with upstream's `feat_os_unix_musl` set (which drops `stdbuf`, the one utility that needs a cdylib); every command name is a hardlink, the list derived from the checkout's `Cargo.toml` and `src/uu`. Also installed as `/usr/bin/env`. |
| `btm` / `bottom` | sibling `../bottom-leandros` | system monitor, static musl |
| `mkfs.fat`, `mkfs.xfs` | [ports/mkfs-fat](../../ports/mkfs-fat/), [ports/mkfs-xfs](../../ports/mkfs-xfs/README.md) | in-tree Rust formatters for disks-rs ([ports.md](ports.md#disks-rs-mkfsfat-and-mkfsxfs)) |
| `disktester` | sibling `../disks-rs` | AerynOS's provisioning tester, unmodified |
| `gpuprobe` | the Mesa GPU stage | reports virtio-gpu capsets and creates a real GBM + EGL context; `/bin/gpu-env` decides the renderer from it |
| `git`, `less`, `nano`, `sftp-server` | [ports/git](../../ports/git/build.sh) | Alpine 3.21's git 2.47 (https through libcurl/OpenSSL), its pager, an editor (`/etc/gitconfig` sets `core.editor = nano`) and OpenSSH's sftp server. `git`, `git-upload-pack`, `git-receive-pack`, `git-upload-archive`, `less` and `nano` are also in `/bin`, because the console login's `PATH` is `/bin` |
| `kmscube` | [ports/gl-stack](../../ports/gl-stack/NOTES.md) | the first dynamic `ET_DYN` + `PT_INTERP` binary; a KMS/GL smoke test |

The image has no `grep`, `su` or `busctl`; scripts and harnesses that run in the guest are written
around that.

## Applications

| Program | What | Page |
|---|---|---|
| COSMIC (`cosmic-comp`, `cosmic-session`, `cosmic-panel`, `cosmic-term`, `cosmic-settings`, `cosmic-launcher`, …) | the desktop | [../desktop/README.md](../desktop/README.md) |
| `greetd`, `fakegreet` | the login daemon and its protocol harness | [../desktop/login.md](../desktop/login.md) |
| `busd`, `leandros-sysbus`, `pop-launcher`, xdg-desktop-portal, PipeWire | session services | [../desktop/session.md](../desktop/session.md) |
| `firefox` | Firefox 136 from Alpine, hardware WebRender on virgl | [ports.md](ports.md#firefox) |
| `doom` | doomgeneric, as a Wayland client under COSMIC or on KMS/fbdev on a text VT | [ports.md](ports.md#doom) |
| `mame` | MAME with a LeandrOS OSD | [ports.md](ports.md#mame) |
| `leandros-applet`, `wl-globals` | the panel's stand-in clock; a `wl_registry` dumper for every Wayland socket (measurement only) | [../desktop/session.md](../desktop/session.md#the-panel-and-its-applets) |

## Regression tests

The rest of `userland/` is test crates, each named for what it guards and run in QEMU on both
architectures: `vfstest`, `f2fstest`, `permtest`, `privtest`, `memtest`, `uptrtest`, `forktest`,
`exectest`, `waittest`, `jobtest`, `killmt`, `spawnwedge`, `sigtest`, `sigtest2`, `sigchldtest`,
`timertest`, `pthreadtest`, `racetest`, `polltest`, `epolltest`, `wakepolltest`, `smpwaketest`,
`idletest`, `scmtest`, `nettest`, `ptytest`, `vttest`, `drmsmoke`, `venustest`, `evtest2`,
`evsplit`. The table of what each covers, how to run them, and the rules for trusting a result
are in [../development/testing.md](../development/testing.md).

`privtest` and `nettest` are single C files built both for the guest (`zig cc` static musl) and
for a Linux host, so the two runs' result lines can be diffed — Linux is the oracle.

## Limitations & open work

- No `passwd`, `su`, `grep`, `who`/`w`/`last` (and no utmpx for them to read).
- `shell` is legacy; new interactive work goes into brush.

## Code & tests

| Where | What |
|---|---|
| [userland/Cargo.toml](../../userland/Cargo.toml) | the workspace and its members |
| [scripts/build-userland.sh](../../scripts/build-userland.sh) | build, and which programs link relibc |
| [scripts/build-all.sh](../../scripts/build-all.sh) | sibling and port builds (`build_brush`, `build_coreutils`, `build_bottom`, …) |
| [scripts/mkfs-f2fs-populated.py](../../scripts/mkfs-f2fs-populated.py) | what is installed where, accounts, hardlinks |

## See also

- [README.md](README.md), [libc.md](libc.md), [ports.md](ports.md)
- [../getting-started/logging-in.md](../getting-started/logging-in.md)
- [../development/testing.md](../development/testing.md)
