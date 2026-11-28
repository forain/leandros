# Graphical login

The default boot ends at a COSMIC login screen: upstream
[greetd](https://github.com/kennylevinsen/greetd) drives `cosmic-greeter`, which runs
unprivileged as its own system account (uid 990), authenticates against `/etc/shadow`,
and hands off to a COSMIC session running as the account that logged in. This page
explains the chain, the three kernel-shaped constraints that dictate its form, and the
pieces LeandrOS supplies in place of PAM, logind and seatd.

For the user-facing side — accounts, passwords, the text-login opt-out — see
[../getting-started/logging-in.md](../getting-started/logging-in.md).

## Overview

PID 1 ([userland/init](../../userland/init/src/main.rs)) supervises the logins side by side:

- the **graphical login** on VT 1 — `/bin/sh /bin/greeter-real`, which sources the render
  environment and execs `greetd`;
- a **serial `login:` getty** on `ttyS0`;
- **text logins on VT 2..6** (`Ctrl+Alt+F2`..`F6`), each a real terminal of its own.

All of them are respawned when they exit. A correct password at the greeter starts the
COSMIC session; a wrong one leaves the greeter up with the field cleared. Two accounts are
provisioned in `/etc/shadow` and work from every login: `root`/`root` and
`leandro`/`leandro`.

Touching `/etc/leandros/text-login` opts out: init checks for that marker before forking
the greetd chain, so the boot lands on the serial and VT text logins only. Headless kernel
tests use this, since they log in on the serial getty and never need a compositor.

`sh /bin/greeter-fake` runs the same greeter against upstream's `fakegreet` protocol
harness, with no daemon, no PAM and no session worker in the way — the smaller thing to
reach for when the greeter itself is what is under test. (`fakegreet`'s hardcoded flow is
user `user`, password `password`, then `7 + 2:` answered `9`; it is left unpatched on
purpose, because an accepted login makes cosmic-greeter exit, which takes cosmic-comp and
the login screen with it.)

## How it works

```
init
 └─ sh /bin/greeter-real          sources /bin/greeter-env and /bin/gpu-env;
     │                            exit 78 if there is no GPU renderer
     └─ greetd                    vt = "none", source_profile = true,
         │                        control socket /run/greetd-<pid>.sock
         ├─ greeter session (user = "root"):
         │    sh -c '. /etc/profile; exec cosmic-comp /bin/greeter-launch --no-xwayland'
         │      └─ cosmic-comp (kiosk)  binds wayland-N in /run/user/990
         │           └─ greeter-launch  chown sockets → setgroups/setresgid/setresuid(990)
         │                              → getuid() == 990? → execve
         │                └─ /bin/cosmic-greeter-login   greeter::main(), uid 990
         └─ on success, user session (uid of the account):
              sh -c '. /etc/profile; exec <cosmic.desktop Exec>'   XDG_RUNTIME_DIR=/run/user/<uid>
```

1. **`/bin/greeter-real`** sources `/bin/greeter-env` (shared with `greeter-fake`, so the
   two launches cannot drift) and `/bin/gpu-env`. With no hardware renderer it exits 78
   (`EX_CONFIG`); init reports that on the serial console once and stops respawning, so a
   headless boot gets its serial login without a fork storm. (On a PC whose NVIDIA card is still booting GSP, `gpu-env` first polls `/proc/nvrm_state` — `absent`, `probing`, `ready` or `failed` — every 250 ms for up to 90 s, and init restarts the graphical login once if greetd exited 78 while the state was `probing`; without that the greeter, which now starts ~0.8 s after init on a fast root, found no renderer and was refused for good.) Otherwise it execs greetd.
2. **greetd** starts the greeter session as root:
   `cosmic-comp /bin/greeter-launch --no-xwayland`. cosmic-comp's kiosk mode is already the
   greeter-session lifecycle — it spawns its child once the compositor is up with
   `WAYLAND_DISPLAY` injected, and exits with the child's status.
3. **`/bin/greeter-launch`** ([userland/greeter-launch](../../userland/greeter-launch/src/main.rs))
   looks the `cosmic-greeter` account up by name, `chown`s the compositor's
   `$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY` and greetd's `$GREETD_SOCK` to it while still root,
   sets the account's supplementary groups (`video`, `input`), `setresgid`/`setresuid`s,
   **reads `getuid()` back**, and `execve`s `/bin/cosmic-greeter-login`.
4. **cosmic-greeter** authenticates over greetd's IPC (it makes no PAM calls of its own in
   the login role). greetd's session worker authenticates through PAM, then
   starts the session selected in `/usr/share/wayland-sessions/cosmic.desktop` as the user,
   through `source_profile`.
5. **`/etc/profile`** ([ports/greetd/data/profile](../../ports/greetd/data/profile)) gives
   the session `PATH`, a per-uid `XDG_RUNTIME_DIR`, the desktop identity, `XDG_VTNR=1`,
   `COSMIC_BACKEND=kms` and the render environment from `gpu-env`, and then the session
   command runs. It prints one line — `profile: session starting, uid=… renderer=…` —
   because greetd itself logs nothing when it starts a session and discards the exit status
   of the one it started: a session that dies instantly is otherwise indistinguishable from
   one never scheduled.

`init` seeds `/run/user/<uid>` (0700, owned by the account) for every account in
`/etc/passwd` at boot, plus `/run/cosmic-greeter` (what upstream ships as a tmpfiles.d
entry). The user cannot create its own runtime directory: `/run/user` is a `0755 root`
tmpfs mount root. Console and VT logins get `XDG_RUNTIME_DIR` from `/bin/login`, as
`pam_systemd` would set it.

## Design decisions

Three constraints shape the arrangement, and all three are properties of this kernel
rather than of greetd.

### The greeter's role comes from its account, not its filename

`cosmic-greeter` has no flag and no environment variable for its role. It runs
`greeter::main()` only when `getpwuid(getuid())` names it `cosmic-greeter`, and
`locker::main()` otherwise. So `cosmic-greeter` is a real system account —
`cosmic-greeter:x:990:990`, home `/home/cosmic-greeter`, shell `/bin/false` — and
`/bin/greeter-launch` exists to *become* it.

- **uid 990, below `UID_MIN`.** cosmic-greeter's own `UserFilter` defaults to
  `UID_MIN 1000` when there is no `/etc/login.defs`, so a uid below 1000 keeps the greeter
  from offering its own account as a login choice; the `/bin/false` shell excludes it a
  second, independent way.
- **Read the uid back.** A launcher that reports a drop it did not perform produces a login
  screen indistinguishable from a correct one. This was not hypothetical: `setuid()` in libc
  was once accept-and-ignore in the kernel, so a process could "drop" and keep root.
- **Why a separate binary rather than a greetd config line.** greetd's
  `default_session.user` applies to the *whole* greeter session command, which here
  includes the compositor; setting it would unprivilege cosmic-comp too.
- **Why the file is named `cosmic-greeter-login`.** `cosmic-session` unconditionally
  spawns `cosmic-greeter` in-session, where it necessarily takes the locker arm, and the
  upstream non-logind locker locks immediately and exits on unlock — a restart loop under
  cosmic-session's supervision. Staged under another name, that spawn fails once per boot
  with one log line and never restart-storms, and no COSMIC patch is needed
  ([ports/cosmic-greeter/README.md](../../ports/cosmic-greeter/README.md)).
  `libpam.so.0` is the real Linux-PAM library, `DT_NEEDED` by the greeter ELF (the login
  role never calls it).

### `vt = "none"` is the only workable terminal mode for greetd itself

Six virtual terminals exist and `Ctrl+Alt+Fn` switches between them
([../servers/tty-and-vt.md](../servers/tty-and-vt.md)), but greetd's own
VT-open-on-startup path is not what drives them here. Every other `vt` value makes greetd
open `/dev/tty0` during startup and exit before it binds its socket; `none` selects
`TerminalMode::Stdin`, and every terminal call in the daemon and the session worker is
behind a match arm that `Stdin` skips. The consequence is that the greeter and the session
inherit greetd's stdio — which init points at `/var/log/greetd.log`.

What `pam_systemd` would export, `XDG_VTNR`, is set to 1 by `/etc/profile` and
`greeter-env` instead (`ec40e9f`): the graphical login *is* the display's session on VT 1.
Without it the libseat shim binds the seat to "the VT on screen when the compositor
started", which is VT 2 if someone happens to be looking at VT 2 — and the compositor then
puts VT 2 into `KD_GRAPHICS` and believes it owns it.

### greetd's control socket cannot live in `/run`

`/run` is a tmpfs (VFS `TMPFS_ROOTS`, as on Linux), so greetd binds upstream's
`/run/greetd-<pid>.sock` unpatched.

**Which directory**, since lane `perms` (2026-09-16): the VFS now enforces search
permission on every path component and write permission on a socket inode for `AF_UNIX`
`connect`, so a uid-990 client can reach nothing under a `0700 root` `/run/user/0`. The
greeter phase therefore uses **the greeter account's own runtime directory,
`/run/user/990`**: `/etc/profile` selects it
as `XDG_RUNTIME_DIR` for the session that carries `GREETD_SOCK` — the greeter session, the
only one greetd's worker puts it in — so cosmic-comp binds `wayland-N` there too.
`greeter-launch`, still root, then chowns both socket nodes to the account. That is
upstream's ownership arrangement (greetd chowns its socket to the greeter user; the
greeter's compositor runs as that user) reached without unprivileging the compositor.

`XDG_SESSION_CLASS` cannot be the discriminator: greetd's worker sets it for PAM and
removes it again before `pam_getenvlist` builds the environment, so no session ever sees
it. This was measured — the first attempt keyed on it, and the uid-990 greeter died with
`NoCompositor`.

### Real Linux-PAM

greetd authenticates through `pam-sys`, which links Alpine's real `libpam.so.0` (the locked
`linux-pam` apk). greetd is a dynamic musl PIE and libpam `dlopen`s `pam_unix`, which checks
`/etc/shadow` through the setuid `unix_chkpwd`. `/etc/pam.d/greetd` includes Alpine's `base-*`
stacks. Two LeandrOS-side pieces keep it working:

- **Nine-field shadow records.** `unix_chkpwd` reports "no such user" for a short record, so
  the image writer emits full records and `shadowfix` (run by init once per boot) pads older
  roots; it never adds or unlocks an account.
- **`pam_rundir.so`** ([ports/pam-rundir](../../ports/pam-rundir/)) fills the
  `-session optional pam_rundir.so` slot in Alpine's `base-session`, exporting
  `XDG_RUNTIME_DIR=/run/user/<uid>` when that directory exists. The `greetd` stack leaves it
  out on purpose (the greeter session is root and keeps the greeter account's runtime dir);
  `sudo` and `su` use it.
- **greetd builds the session environment from PAM.** It is exactly what went through
  `pam_putenv`, read back with `pam_getenvlist`; nothing of greetd's own environment is
  inherited. That is why the COSMIC render environment reaches the session through
  `/etc/profile` and `source_profile = true`.
- **A failed authentication is rate limited** by `pam_unix`'s own fail delay; `/bin/login`
  does not use PAM but applies a 1 s stall after each wrong attempt.

### The greetd deltas

greetd is pinned at `d6733e9` (0.10.3-22) and carries three patches
([ports/greetd/README.md](../../ports/greetd/README.md)):

| Patch | Why |
|---|---|
| `0001-leandros.patch` | greetd re-executes itself via `/proc/self/exe` for each session worker; the VFS synthesises that path inside `readlink(2)` only, so `execve` on it is `ENOENT`. The patch resolves it with `current_exe()` before the fork. It also adds utmpx/wtmp/lastlog and session-cgroup bookkeeping around the PAM session. |
| `0002-fakegreet-leandros.patch` | avoids `std::fs::canonicalize` (musl's `realpath` via `open(O_PATH)` + `/proc/self/fd`), which fakegreet `.unwrap()`ed |

Of the five workspace crates, `agreety` is not built (`/bin/login` has that role);
`fakegreet` is.

### When the greeter dies young

init respawns the graphical login, but a login that exits within `DM_STABLE_SECS` (60 s) is a crash, not a session. For the first three such exits init copies the last 3 KiB of `/var/log/greetd.log` to the console before restarting it, whole lines, prefixed `greetd.log| `, with ANSI colour sequences removed (COSMIC's tracing output is coloured; masking only ESC left `[2m…[0m` noise), other control bytes masked, empty lines dropped and lines cut at 200 bytes. The console lands in the stick's log partition, so a PC whose screen or keyboard is dead still says why the greeter or its compositor exited.

### Login diagnostics and the greetd build cache

Lane lapgreet added diagnostics for a first greetd login after boot that sometimes failed on the laptop (not reproduced: 3/3 and then 4/4 boots logged in within 6 s; the cause stays open). `greetd.log` now keeps four boots (`greetd.log`, `.1` to `.3`), rotated when the logger starts, each file capped at 4 MB (past the cap the file restarts instead of renaming, so earlier boots survive) and headed `=== boot (uptime N s) ===`. Trace lines show the IPC sequence (`greetd: ipc <type>`, the command's argv0, `session exited status=.. after N ms`, `greeter exited after start_session`), the PAM authentication result and time (never the secret) and `start-cosmic: <uptime> step ...`. The kernel logs `drm: SET_MASTER EBUSY open_id N (holder open_id M)` and `drm: master released open_id N (last close)` (8 lines a boot); a healthy login shows the master released before start-cosmic runs.

greetd is staged by a hash of its source inputs: `greetd-lane/<arch>/<srchash>/{greetd,fakegreet,greetd.srchash}`. `ports/greetd/build.sh` skips cargo when that hash directory exists (`FORCE=1` rebuilds), and `scripts/mkfs-f2fs-populated.py` always uses the hash directory of its own checkout, building it when missing (about 20 s warm; `GREETD_NO_AUTOBUILD=1` disables). Two worktrees therefore cannot ship each other's greetd, and the old mtime-based freshness guard (which gave false "stale" errors and let stale binaries through) became the hash check. The greeter has no GPU path on a Pi without one: init skips greetd when the GPU precheck refuses.

## Limitations & open work

- **cosmic-comp still runs as root in the greeter session.** libseat is a shim with no
  seatd behind it, so the compositor cannot be handed a revocable device fd by a broker.
  The user session's compositor runs as the user, but the greeter's does not.
- **The greeter's own session is not recorded.** greetd's PAM session writes the
  utmpx/wtmp/lastlog records (line `greetd`) for the user's login only.
- **Landmines recorded in the port** ([ports/greetd/README.md](../../ports/greetd/README.md#landmines)):
  greetd's parent-to-worker channel is a `UnixDatagram` socketpair (a message up to
  212960 bytes goes through, a larger one is `EMSGSIZE`, as on Linux); `prctl(PR_SET_PDEATHSIG)` is accepted and does nothing; cosmic-comp's
  argument order is load-bearing (the kiosk child is the *first* argument, so
  `cosmic-comp --no-xwayland <exec>` tries to spawn `--no-xwayland`).
- **`--no-xwayland` is not optional.** Xwayland is not shipped, and smithay's
  Xwayland-socket bind-error path leaked fds into an `EMFILE` that crashed the first
  scanout.
- Two harmless log lines are known: the greeter's cosmic-comp logs
  `eglMakeCurrent … BadDisplay` while exiting after a successful login, and a
  `Bad read on self-pipe: EBADF` panic from a thread in the session's `sh -c` wrapper
  (brush), after which the session proceeds.

## Code & tests

| Where | What |
|---|---|
| [ports/greetd/data/](../../ports/greetd/data/) | `greetd.conf`, `profile`, `greeter-env`, `greeter-real`, `greeter-fake`, `gpu-env`, `cosmic.desktop`, `pam.d-greetd` |
| [ports/pam-rundir/](../../ports/pam-rundir/) | `pam_rundir.so` and the `sudo`/`su` PAM stacks |
| [userland/greeter-launch/src/main.rs](../../userland/greeter-launch/src/main.rs) | drop to the greeter account, verify, exec |
| [userland/login/src/main.rs](../../userland/login/src/main.rs) | the text login and the shadow scheme |
| [userland/init/src/main.rs](../../userland/init/src/main.rs) | supervision, respawn backoff, `/run/user` seeding, the greetd log |
| [userland/permtest](../../userland/permtest/), [userland/vttest](../../userland/vttest/) | the permission and VT behaviour the login path depends on |

## See also

- [README.md](README.md) — the desktop overview
- [session.md](session.md) — what runs after the login
- [../servers/security.md](../servers/security.md) — accounts, `/etc/shadow`, permission enforcement
- [../servers/tty-and-vt.md](../servers/tty-and-vt.md) — VTs, per-VT DRM master
- [../userland/programs.md](../userland/programs.md) — `init`, `login`, `greeter-launch`
