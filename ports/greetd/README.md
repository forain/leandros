# greetd (LeandrOS port)

Upstream: <https://github.com/kennylevinsen/greetd>, GPL-3.0-only.
Pinned commit `d6733e983ff7821c3044007d5555345c7553188f` (`0.10.3-22-gd6733e9`,
2026-07-21). `build.sh` builds it static-musl for both arches; see that file for
the static-PIE landmine and the mandatory `-C relocation-model=static`.

greetd is the daemon behind `cosmic-greeter`. The greeter is the *client* of the
greetd IPC protocol and cannot be modified; greetd is the server.

## What is built and what is not

Upstream is a five-crate workspace:

| crate | built? | why |
|---|---|---|
| `greetd` | **yes** | the daemon |
| `greetd_ipc` | yes (as a dependency) | the wire protocol, shared with the greeter |
| `inish` | yes (as a dependency) | the INI-ish config parser |
| `agreety` | no | a tty greeter; we have `/bin/login` for that role |
| `fakegreet` | **yes** | a protocol-only stand-in; see below |

`fakegreet` speaks the full greetd protocol with no PAM, no VT, no session
worker, no `fork`, no `/proc/self/exe` and no privilege handling: it spawns its
argument with `sh -c`, sets `GREETD_SOCK`, answers the protocol from a hardcoded
credential, and returns `Success` to `start_session` without starting anything.
Its dependency set is `serde`, `greetd_ipc`, `tokio` and `thiserror` — nothing
LeandrOS lacks. That makes it a harness for proving the greeter half (env traps,
DRM, socket, the render environment) with none of the daemon's risk surface in
the picture, and it is what brought the login screen up first. Its hardcoded
flow is user `user`, password `password`, then a second prompt `7 + 2:` answered
`9`; it is deliberately left unpatched, because an accepted login makes
cosmic-greeter exit, which makes cosmic-comp exit with it, which takes the login
screen off the screen. `/bin/greeter-fake` drives it.

## The LeandrOS deltas

`patches/0001-leandros.patch` (26 lines) carries the first two;
`patches/0002-fakegreet-leandros.patch` and `patches/0003-socket-dir-leandros.patch`
were added during bring-up and are described after them.

**1. `/proc/self/exe` is not executable here.** greetd re-executes itself to
create each session worker (`session/interface.rs`,
`execv("/proc/self/exe", ["greetd", "--session-worker", <fd>])`). LeandrOS
synthesises `/proc/self/exe` *inside `readlink(2)` only* — the VFS has no node
for it (`servers/vfs/src/lib.rs`'s `gen_proc_self_content` has no `exe` branch),
so `openat` and `execve` on that path both return `ENOENT`, and every session,
greeter and user alike, would die at creation. The patch resolves the path with
`std::env::current_exe()` — which *is* that readlink, and returns the absolute
path the kernel recorded at our own `execve` — before the fork, so a failure is
a returned error rather than a panic in a child that cannot report one.

**2. There is no PAM stack.** greetd authenticates through `pam-sys`, a raw FFI
binding to `libpam`. `pam-leandros/` (in this directory, Rust) supplies that ABI
and implements the one part that must do real work — checking a password against
`/etc/shadow` in the same `$sha256$<salt>$<hex>` scheme `/bin/login` validates.
The patch adds it as a dependency and a `use pam_leandros as _;` so it is
actually linked.

**3. `std::fs::canonicalize` is avoided in `fakegreet`**
(`patches/0002-fakegreet-leandros.patch`). It is musl's `realpath(3)`, which
resolves a path by `open(O_PATH)` plus `readlink("/proc/self/fd/N")`. fakegreet
binds its socket in a tmpfs runtime dir, where neither of those is exercised,
and it `.unwrap()`ed the result — a panic, not an error. `env::current_dir()` is
already absolute, so the join is the same answer with none of that surface.

**4. The control socket's directory is steerable**
(`patches/0003-socket-dir-leandros.patch`). Upstream hardcodes
`/run/greetd-<pid>.sock` because on Linux `/run` is a tmpfs. On LeandrOS `/run`
is an ordinary f2fs directory and f2fs has no `S_IFSOCK` support, so that bind
fails with `EOPNOTSUPP` — greetd prints
`unable to open listener: Not supported (os error 95)` and exits before the
greeter is ever spawned. The socket-capable mounts are `/tmp`, `/dev/shm` and
`/run/user` (`servers/vfs`'s `TMPFS_ROOTS`). The patch reads `GREETD_SOCK_DIR`
and defaults to `/run`, so unset it is byte-for-byte upstream; `/bin/greeter-real`
sets it to `$XDG_RUNTIME_DIR`. The alternative fix is to make `/run` itself a
tmpfs mount root in the VFS, which is the more Linux-faithful answer but shadows
the image's `/run` contents and changes what `/run/user` means as a pool root —
a kernel decision, not a port decision.

Everything else upstream does is either already supported or configured away —
see the config file for the VT case.

## Why `pam-leandros` is an rlib, and the two empty archives

A Rust `staticlib` embeds its own copy of `std`; linking one into greetd (also
`std`) duplicates every `std` symbol. So `pam-leandros` is an ordinary rlib that
greetd depends on, and its `#[no_mangle] extern "C"` definitions resolve
`pam-sys`'s undefined references exactly as a real `libpam.a` would.

`pam-sys`'s build script still emits `cargo:rustc-link-lib=pam` and
`...=pam_misc` unconditionally, so `build.sh` puts two **empty** archives by
those names on the link path. The linker finds them and pulls nothing out.

`pam-leandros` is copied *outside* the greetd source tree during the build: it
declares its own `[workspace]` (so it can be tested standalone), and a second
workspace root inside greetd's workspace directory is a hard cargo error.

## Relationship to the existing `libpam.so.0` shim

`leandros-artifacts/m6-session-bins/src/libpam-shim/` already provides a libpam
shim, as a C shared library, for `cosmic-greeter`'s **lock-screen** role. It is
not reusable here as-is:

- `pam_get_user` and `pam_misc_drop_env` are not defined in it at all, and
  greetd calls both — that is a link failure, not a runtime one.
- `pam_putenv` is a no-op and `pam_getenvlist` returns an empty list. greetd
  builds a session's **entire** environment through those two functions, so with
  that shim a session would start with a completely empty environment —
  including no `GREETD_SOCK`, which `cosmic-greeter` `.expect()`s.

`pam-leandros` implements all four properly. The two implementations now share
only the `/etc/shadow` scheme; keeping them in step is a real maintenance edge,
and folding the lock-screen shim onto this crate (built additionally as a
`cdylib` named `libpam.so.0`) is the obvious follow-up — it would also retire a
C file from a Rust-only project.

## Configuration

`data/greetd.conf` → `/etc/greetd/greetd.conf`. The load-bearing settings:

- **`vt = "none"`** — the only value that keeps greetd off the terminal path.
  LeandrOS has no `/dev/tty0` and none of the `VT_*`/`KDSETMODE` ioctls; any
  other value makes greetd exit during startup, before it binds its socket.
- **`source_profile = true`** — greetd passes *none* of its own environment to a
  session; the environment is exactly what it pushed through `pam_putenv`. The
  `source_profile` wrapper (`sh -c '. /etc/profile; ... exec <cmd>'`) is the only
  hook that can put the COSMIC render environment in front of a session command
  greetd did not choose. `data/profile` → `/etc/profile` carries it.
- **`user = "root"`** — the compositor *is* the greeter session, and it must stay
  root (shim libseat, no seatd, unverified `/dev/dri` and `/dev/input` modes).
  Only the greeter client drops privileges, inside a launcher that replaces the
  greeter half of `default_session.command` when it lands.

`data/pam.d-greetd` → `/etc/pam.d/greetd` exists only because greetd refuses to
start without it. Nothing reads the contents.

## Landmines

- **A datagram over ~4 KB livelocks the daemon.** greetd's parent↔worker channel
  is a `UnixDatagram` socketpair. LeandrOS preserves datagram boundaries
  correctly (a 4-byte length record inside the ring), but `RING_SIZE` is 4096, so
  a message over 4092 bytes makes `write_dgram` fail, which surfaces as `EAGAIN`,
  which the kernel's blocking wrapper retries forever. There is no `EMSGSIZE`.
  The messages that can grow are `Args { env, cmd }` — the session command plus
  the environment the greeter sent. COSMIC's is a few hundred bytes; a session
  entry with a large `Exec=` or many `DesktopNames` is the thing to watch.
- **`prctl(PR_SET_PDEATHSIG)` silently does nothing.** The kernel's `sys_prctl`
  accepts unknown options and returns 0. Sessions therefore do not get `SIGTERM`
  when greetd dies; they have to be cleaned up some other way.
- **`setgroups` is accept-and-ignore and `getgroups` reports zero groups.** So
  `initgroups` in the session child is a no-op. Supplementary group membership
  does not exist on LeandrOS, which matters the moment anything gates on the
  `video` or `input` groups.
- **cosmic-comp's argument order is load-bearing.** Its kiosk child is
  `env::args().skip(1).next()` — the *first* argument, whatever it is — while
  its flag parser scans every argument and ignores what it does not recognise.
  So `cosmic-comp --no-xwayland <exec>` tries to spawn `--no-xwayland` and dies
  with `Error running kiosk child ... No such file or directory`, leaving a
  cleared screen and a healthy compositor with nothing on it. Write
  `cosmic-comp <exec> --no-xwayland`: the flag still takes effect and is then
  also handed to cosmic-greeter, whose own argument loop ignores it.
- **greetd's socket path contains its pid** (`<dir>/greetd-<pid>.sock`) and is
  handed to the greeter through the PAM environment, not a fixed path. If
  `pam_putenv`/`pam_getenvlist` regress to stubs, the greeter panics on a
  missing `GREETD_SOCK` and the failure looks nothing like a PAM problem.

## Bring-up on target (aarch64, 2026-08-08)

Both halves reach a rendered COSMIC login screen. `/bin/greeter-fake` (fakegreet)
and `/bin/greeter-real` (the daemon) produce the same screen; captures are in the
lane's scratchpad. With the real daemon, typing `leandro`'s password makes the
greeter exit, greetd start the scheduled session, that session fail on the
unstaged `/usr/bin/env`, and greetd bring a fresh greeter back — which is
`SIGCHLD` being delivered to greetd's tokio signal stream, the port's largest
untested dependency. A wrong password leaves the greeter up with the field
cleared and starts nothing, so `pam-leandros` really is checking `/etc/shadow`.
Guest RAM made no difference: the screen is byte-identical at `-m 2G` and
`-m 4G`.

### The greeter role, and what is still privileged

cosmic-greeter has no flag and no environment variable for its role: it runs
`greeter::main()` when `getpwuid(getuid())` is named `cosmic-greeter` and
`locker::main()` otherwise (`cosmic-greeter/src/main.rs`). The greeter client is
still uid 0 on the path this README first measured — the name was supplied by
`/etc/passwd.greeter`, the same accounts with a uid-0 `cosmic-greeter` entry
ahead of `root` (musl's `getpwuid` returns the first uid match), with
`/etc/passwd.system` as the undo. That is scaffolding for the first photograph,
not the shipping arrangement.

**The image now also carries the real account**: `cosmic-greeter:x:990:990`
with home `/home/cosmic-greeter` and shell `/bin/false`. The uid is below 1000
because cosmic-greeter's own `UserFilter` defaults to `UID_MIN 1000` with no
`/etc/login.defs` present, and it would otherwise offer the greeter's own account
as a login choice; the `/bin/false` shell excludes it a second, independent way.
Delete `passwd.greeter`/`passwd.system` and the `cp` in the launchers once the
launcher path has been photographed.

### Guest files

| path | what |
|---|---|
| `/bin/greetd`, `/bin/fakegreet` | the two binaries |
| `/bin/greeter-env` | the shared render environment, sourced by both launchers |
| `/bin/greeter-fake` | fakegreet + cosmic-comp + cosmic-greeter-login |
| `/bin/greeter-real` | the daemon |
| `/etc/greetd/greetd.conf`, `/etc/pam.d/greetd`, `/etc/profile` | config |
| `/etc/passwd.greeter`, `/etc/passwd.system` | the role switch and its undo |
| `/usr/share/wayland-sessions/cosmic.desktop` | the session the greeter offers |
